use super::*;
use hickory_proto::rr::{rdata, Name, Record};
use std::sync::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn request(server: String) -> Request {
    Request {
        config: String::new(),
        egress: Egress::System,
        probe: Probe::Dns {
            name: "example.test".into(),
            record: "A".into(),
            server,
        },
        count: 1,
        interval_ms: 0,
        timeout_ms: 500,
    }
}
async fn collect(request: Request) -> Vec<Event> {
    let events = Mutex::new(Vec::new());
    run(&request, |event| {
        events.lock().unwrap().push(event);
        Ok(())
    })
    .await
    .unwrap();
    events.into_inner().unwrap()
}
fn answer(query: &[u8], code: ResponseCode) -> Message {
    let query = Message::from_vec(query).unwrap();
    let mut reply = astra_dns::wire::response(&query, code);
    if code == ResponseCode::NoError {
        reply.add_answer(Record::from_rdata(
            query.queries[0].name().clone(),
            123,
            RData::A("192.0.2.7".parse().unwrap()),
        ));
        reply.add_authority(Record::from_rdata(
            Name::from_ascii("example.test").unwrap(),
            456,
            RData::NS(rdata::NS(Name::from_ascii("ns.example.test").unwrap())),
        ));
        reply.add_additional(Record::from_rdata(
            Name::from_ascii("ns.example.test").unwrap(),
            789,
            RData::A("192.0.2.8".parse().unwrap()),
        ));
        reply.metadata.authentic_data = true;
        let mut edns = hickory_proto::op::Edns::new();
        edns.set_max_payload(1232);
        edns.set_dnssec_ok(true);
        reply.edns = Some(edns);
    }
    reply
}
async fn udp_fixture(
    transform: impl FnOnce(&[u8]) -> Vec<u8> + Send + 'static,
) -> (String, tokio::task::JoinHandle<()>) {
    let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let address = server.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let mut bytes = [0; 4096];
        let (n, peer) = server.recv_from(&mut bytes).await.unwrap();
        server.send_to(&transform(&bytes[..n]), peer).await.unwrap();
    });
    (format!("udp://{address}"), task)
}
fn response(events: &[Event]) -> &DnsDetail {
    events
        .iter()
        .find_map(|e| match e {
            Event::Dns { detail, .. } if matches!(**detail, DnsDetail::Response { .. }) => {
                Some(detail.as_ref())
            }
            _ => None,
        })
        .unwrap()
}
#[test]
fn service_bootstrap_preserves_tls_name_and_doh_path() {
    let e = Endpoint::parse("https://resolver.example:8443/custom?mode=dns#bootstrap=192.0.2.9")
        .unwrap();
    assert_eq!(e.host, "resolver.example");
    assert_eq!(e.port, 8443);
    assert_eq!(e.path, "/custom?mode=dns");
    assert_eq!(e.bootstrap, Some("192.0.2.9".parse().unwrap()));
    assert_eq!(
        Endpoint::parse("tls://resolver.example#bootstrap=2001:db8::1")
            .unwrap()
            .protocol,
        Protocol::Dot
    );
    assert!(Endpoint::parse("https://user:password@resolver.example/path").is_err());
    assert!(Endpoint::parse("http://127.0.0.1/dns-query").is_err());
    assert!(Endpoint::parse("https://resolver.example#bootstrap=hostname.test").is_err());
}
#[tokio::test]
async fn constrained_resolution_never_calls_the_system_resolver() {
    SYSTEM_LOOKUPS
        .scope(std::cell::Cell::new(0), async {
            assert!(resolve_for(
                "must-not-resolve.invalid",
                53,
                Some("interface-fixture"),
                None
            )
            .await
            .is_err());
            assert_eq!(
                resolve_for(
                    "resolver.example",
                    853,
                    Some("interface-fixture"),
                    Some("192.0.2.5".parse().unwrap())
                )
                .await
                .unwrap()
                .ip(),
                "192.0.2.5".parse::<IpAddr>().unwrap()
            );
            assert_eq!(
                resolve_for("127.0.0.1", 53, Some("interface-fixture"), None)
                    .await
                    .unwrap()
                    .port(),
                53
            );
            SYSTEM_LOOKUPS.with(|calls| assert_eq!(calls.get(), 0));
        })
        .await;
}
#[tokio::test]
async fn dns_sections_ttl_edns_and_ad_are_evidence_not_dnssec_validation() {
    let (server, task) =
        udp_fixture(|query| answer(query, ResponseCode::NoError).to_vec().unwrap()).await;
    let events = collect(request(server)).await;
    task.await.unwrap();
    let DnsDetail::Response {
        outcome,
        flags,
        edns,
        questions,
        answer_count,
        authority_count,
        additional_count,
        dnssec_locally_validated,
        ..
    } = response(&events)
    else {
        panic!()
    };
    assert_eq!(outcome, "resolved");
    assert!(flags.authenticated_data);
    assert!(!dnssec_locally_validated);
    assert_eq!(
        (*answer_count, *authority_count, *additional_count),
        (1, 1, 1)
    );
    assert_eq!(edns.as_ref().unwrap().udp_payload, 1232);
    assert!(edns.as_ref().unwrap().dnssec_ok);
    assert_eq!(questions[0].record_type, "A");
    let records = events
        .iter()
        .filter_map(|event| match event {
            Event::Dns { detail, .. } => match detail.as_ref() {
                DnsDetail::Records { section, records } => Some((section.as_str(), records[0].ttl)),
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        records,
        vec![("answer", 123), ("authority", 456), ("additional", 789)]
    );
}
#[tokio::test]
async fn dns_negative_responses_are_distinct_from_network_failures() {
    for (code, expected) in [
        (ResponseCode::NXDomain, "nxdomain"),
        (ResponseCode::ServFail, "server_failure"),
        (ResponseCode::NoError, "no_records"),
    ] {
        let (server, task) = udp_fixture(move |query| {
            let mut reply = answer(query, code);
            reply.answers.clear();
            reply.to_vec().unwrap()
        })
        .await;
        let events = collect(request(server)).await;
        task.await.unwrap();
        let DnsDetail::Response {
            outcome,
            response_received,
            parsed,
            ..
        } = response(&events)
        else {
            panic!()
        };
        assert_eq!(outcome, expected);
        assert!(*response_received && *parsed);
        assert!(!events.iter().any(
            |e| matches!(e,Event::Dns {detail,..} if matches!(**detail,DnsDetail::Failure {..}))
        ));
    }
}
#[tokio::test]
async fn truncated_udp_retries_tcp_on_same_endpoint_and_exposes_both_responses() {
    let udp = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let address = udp.local_addr().unwrap();
    let tcp = tokio::net::TcpListener::bind(address).await.unwrap();
    let udp_task = tokio::spawn(async move {
        let mut bytes = [0; 4096];
        let (n, peer) = udp.recv_from(&mut bytes).await.unwrap();
        let mut reply = answer(&bytes[..n], ResponseCode::NoError);
        reply.answers.clear();
        reply.metadata.truncation = true;
        udp.send_to(&reply.to_vec().unwrap(), peer).await.unwrap();
    });
    let tcp_task = tokio::spawn(async move {
        let (mut stream, _) = tcp.accept().await.unwrap();
        let size = stream.read_u16().await.unwrap();
        let mut bytes = vec![0; size as usize];
        stream.read_exact(&mut bytes).await.unwrap();
        let reply = answer(&bytes, ResponseCode::NoError).to_vec().unwrap();
        stream.write_u16(reply.len() as u16).await.unwrap();
        stream.write_all(&reply).await.unwrap();
    });
    let events = collect(request(format!("udp://{address}"))).await;
    udp_task.await.unwrap();
    tcp_task.await.unwrap();
    let transports = events
        .iter()
        .filter_map(|e| match e {
            Event::Dns { detail, .. } => match detail.as_ref() {
                DnsDetail::Response { transport, .. } => Some(transport.as_str()),
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(transports, vec!["udp", "tcp"]);
    assert!(events
        .iter()
        .any(|e| matches!(e,Event::Dns {detail,..} if matches!(**detail,DnsDetail::Retry {..}))));
}
#[tokio::test]
async fn malformed_and_mismatched_responses_are_not_nxdomain_or_no_records() {
    for malformed in [true, false] {
        let (server, task) = udp_fixture(move |query| {
            if malformed {
                vec![query[0], query[1], 0]
            } else {
                let mut reply = answer(query, ResponseCode::NoError);
                reply.queries.clear();
                reply.to_vec().unwrap()
            }
        })
        .await;
        let events = collect(request(server)).await;
        task.await.unwrap();
        assert!(events.iter().any(|event|matches!(event,Event::Dns {detail,..} if matches!(detail.as_ref(),DnsDetail::Failure {outcome,response_received:true,..} if outcome==if malformed {"malformed"}else{"response_mismatch"}))));
    }
}
#[tokio::test]
async fn timeout_records_no_response_instead_of_a_dns_rcode() {
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut request = request(format!("udp://{}", socket.local_addr().unwrap()));
    request.timeout_ms = 100;
    let events = collect(request).await;
    assert!(events.iter().any(|event|matches!(event,Event::Dns {detail,..} if matches!(detail.as_ref(),DnsDetail::Failure {outcome,response_received:false,..} if outcome=="timeout"))));
}
#[test]
fn record_and_text_limits_are_explicit_and_chunks_fit_the_transport() {
    let query = astra_dns::wire::query("example.test", RecordType::TXT).unwrap();
    let mut reply = astra_dns::wire::response(&query, ResponseCode::NoError);
    for _ in 0..300 {
        reply.add_answer(Record::from_rdata(
            query.queries[0].name().clone(),
            1,
            RData::TXT(rdata::TXT::new(vec!["x".repeat(9000)])),
        ));
    }
    let endpoint = Endpoint::parse("127.0.0.1").unwrap();
    let events = Mutex::new(Vec::new());
    details(
        1,
        &reply,
        &endpoint,
        Protocol::Udp,
        "127.0.0.1:53".parse::<SocketAddr>().unwrap(),
        RecordType::TXT,
        &|e| {
            events.lock().unwrap().push(e);
            Ok(())
        },
    )
    .unwrap();
    let events = events.into_inner().unwrap();
    assert!(matches!(
        response(&events),
        DnsDetail::Response {
            truncated: true,
            ..
        }
    ));
    let mut count = 0;
    for event in events {
        assert!(serde_json::to_vec(&event).unwrap().len() < 56 * 1024);
        if let Event::Dns { detail, .. } = event {
            if let DnsDetail::Records { records, .. } = *detail {
                count += records.len();
                for record in records {
                    assert!(record.text.len() <= 8192);
                    assert!(record.text_truncated);
                }
            }
        }
    }
    assert_eq!(count, 256);
}
async fn http_fixture(
    status: &str,
    content_type: &str,
    body: Vec<u8>,
) -> (TcpStream, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let header=format!("HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len());
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut header_in = Vec::new();
        while !header_in.ends_with(b"\r\n\r\n") {
            header_in.push(stream.read_u8().await.unwrap());
        }
        let text = String::from_utf8(header_in).unwrap();
        assert!(text.starts_with("POST /dns-query HTTP/1.1"));
        let size = text
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length: ")
                    .and_then(|s| s.parse::<usize>().ok())
            })
            .unwrap();
        let mut bytes = vec![0; size];
        stream.read_exact(&mut bytes).await.unwrap();
        let _ = stream.write_all(header.as_bytes()).await;
        let _ = stream.write_all(&body).await;
    });
    (TcpStream::connect(address).await.unwrap(), task)
}
#[tokio::test]
async fn doh_http_parser_bounds_body_checks_media_type_and_rejects_redirects() {
    let endpoint = Endpoint::parse("https://resolver.example/dns-query").unwrap();
    let query = astra_dns::wire::query("example.test", RecordType::A)
        .unwrap()
        .to_vec()
        .unwrap();
    for (status, kind, body, ok) in [
        ("200 OK", "application/dns-message", vec![1, 2, 3], true),
        ("302 Found", "application/dns-message", vec![], false),
        ("200 OK", "text/html", vec![1, 2], false),
        ("200 OK", "application/dns-message", vec![0; 65536], false),
    ] {
        let (stream, task) = http_fixture(status, kind, body.clone()).await;
        let result = doh_http(stream, &endpoint, &query).await;
        assert_eq!(result.is_ok(), ok, "{result:?}");
        if ok {
            assert_eq!(result.unwrap(), body);
        }
        task.await.unwrap();
    }
}
#[tokio::test]
async fn legacy_dns_stream_does_not_emit_detailed_events() {
    let (server, task) =
        udp_fixture(|query| answer(query, ResponseCode::NoError).to_vec().unwrap()).await;
    let events = Mutex::new(Vec::new());
    super::super::run(None, &request(server), false, |event| {
        events.lock().unwrap().push(event);
        Ok(())
    })
    .await
    .unwrap();
    task.await.unwrap();
    assert!(!events.into_inner().unwrap().iter().any(|event| matches!(
        event,
        Event::Dns { .. } | Event::Resolution { .. } | Event::Context(_)
    )));
}

#[test]
fn cname_chain_follows_question_and_unrelated_answers_are_not_resolution() {
    let query = astra_dns::wire::query("example.test", RecordType::A).unwrap();
    let mut reply = astra_dns::wire::response(&query, ResponseCode::NoError);
    reply.add_answer(Record::from_rdata(
        Name::from_ascii("unrelated.test").unwrap(),
        1,
        RData::A("192.0.2.1".parse().unwrap()),
    ));
    assert_eq!(outcome(&reply, RecordType::A), "no_records");
    reply.add_answer(Record::from_rdata(
        query.queries[0].name().clone(),
        60,
        RData::CNAME(rdata::CNAME(Name::from_ascii("target.test.").unwrap())),
    ));
    reply.add_answer(Record::from_rdata(
        Name::from_ascii("target.test.").unwrap(),
        30,
        RData::A("192.0.2.7".parse().unwrap()),
    ));
    assert_eq!(outcome(&reply, RecordType::A), "resolved");
    let events = Mutex::new(Vec::new());
    details(
        1,
        &reply,
        &Endpoint::parse("127.0.0.1").unwrap(),
        Protocol::Udp,
        "127.0.0.1:53".parse::<SocketAddr>().unwrap(),
        RecordType::A,
        &|event| {
            events.lock().unwrap().push(event);
            Ok(())
        },
    )
    .unwrap();
    assert!(
        matches!(response(&events.into_inner().unwrap()),DnsDetail::Response {cname_chain,..} if cname_chain==&["target.test."])
    );
}

#[tokio::test]
async fn detailed_tcp_uses_literal_socket_endpoint_without_hidden_system_lookup() {
    SYSTEM_LOOKUPS
        .scope(std::cell::Cell::new(0), async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let mut request = request(String::new());
            request.probe = Probe::Tcp {
                host: "127.0.0.1".into(),
                port: listener.local_addr().unwrap().port(),
            };
            let events = collect(request).await;
            assert!(events
                .iter()
                .any(|event| matches!(event,Event::Attempt(attempt) if attempt.error.is_none())));
            SYSTEM_LOOKUPS.with(|calls| assert_eq!(calls.get(), 0));
        })
        .await;
}

#[tokio::test]
async fn untrusted_dot_and_doh_certificates_fail_without_sending_dns_payload() {
    use std::sync::Arc;
    use tokio_rustls::rustls::{self, pki_types::PrivatePkcs8KeyDer};
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let server = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.cert.der().clone()],
            PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()).into(),
        )
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server));
    for scheme in ["tls", "https"] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let acceptor = acceptor.clone();
        let worker = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            assert!(acceptor.accept(socket).await.is_err());
        });
        let events = collect(request(format!(
            "{scheme}://localhost:{}#bootstrap=127.0.0.1",
            address.port()
        )))
        .await;
        assert!(events.iter().any(|event|matches!(event,Event::Dns {detail,..} if matches!(detail.as_ref(),DnsDetail::Failure {response_received:false,message,..} if message.contains("TLS")))));
        assert!(!events.iter().any(|event|matches!(event,Event::Dns {detail,..} if matches!(**detail,DnsDetail::Response {..}))));
        worker.await.unwrap();
    }
}

#[tokio::test]
async fn dot_and_doh_exchange_with_a_local_verified_tls_service() {
    use std::sync::Arc;
    use tokio_rustls::rustls::{self, pki_types::PrivatePkcs8KeyDer, RootCertStore};
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let server = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.cert.der().clone()],
            PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()).into(),
        )
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server));
    let mut roots = RootCertStore::empty();
    roots.add(cert.cert.der().clone()).unwrap();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(
        rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ));
    for protocol in [Protocol::Dot, Protocol::Doh] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let acceptor = acceptor.clone();
        let worker = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut stream = acceptor.accept(socket).await.unwrap();
            let size = if protocol == Protocol::Dot {
                stream.read_u16().await.unwrap() as usize
            } else {
                let mut raw = Vec::new();
                while !raw.ends_with(b"\r\n\r\n") {
                    raw.push(stream.read_u8().await.unwrap());
                    assert!(raw.len() < 8192);
                }
                let header = String::from_utf8(raw).unwrap();
                assert!(header.starts_with("POST /dns-query HTTP/1.1"));
                header
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .and_then(|s| s.parse::<usize>().ok())
                    })
                    .unwrap()
            };
            let mut query = vec![0; size];
            stream.read_exact(&mut query).await.unwrap();
            let response = answer(&query, ResponseCode::NoError).to_vec().unwrap();
            if protocol == Protocol::Dot {
                stream.write_u16(response.len() as u16).await.unwrap();
            } else {
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/dns-message\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",response.len()).as_bytes()).await.unwrap();
            }
            stream.write_all(&response).await.unwrap();
            stream.flush().await.unwrap();
        });
        let query = astra_dns::wire::query("example.test", RecordType::A).unwrap();
        let endpoint = Endpoint {
            protocol,
            host: "localhost".into(),
            port: address.port(),
            path: "/dns-query".into(),
            bootstrap: Some(address.ip()),
        };
        let stream = dial(address, None).await.unwrap();
        let raw = encrypted_exchange(
            protocol,
            &endpoint,
            stream,
            &query.to_vec().unwrap(),
            &connector,
        )
        .await
        .unwrap();
        let reply = Message::from_vec(&raw).unwrap();
        astra_dns::wire::validate_response(&query, &reply).unwrap();
        assert_eq!(outcome(&reply, RecordType::A), "resolved");
        worker.await.unwrap();
    }
}

#[tokio::test]
async fn stream_dns_truncation_retains_records_and_reports_incomplete_failure() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let n = stream.read_u16().await.unwrap();
        let mut query = vec![0; n as usize];
        stream.read_exact(&mut query).await.unwrap();
        let mut reply = answer(&query, ResponseCode::NoError);
        reply.metadata.truncation = true;
        let wire = reply.to_vec().unwrap();
        stream.write_u16(wire.len() as u16).await.unwrap();
        stream.write_all(&wire).await.unwrap();
    });
    let events = collect(request(format!("tcp://{address}"))).await;
    server.await.unwrap();
    assert!(events.iter().any(|event|matches!(event,Event::Dns {detail,..} if matches!(detail.as_ref(),DnsDetail::Response {truncated:true,..}))));
    assert!(events.iter().any(|event|matches!(event,Event::Dns {detail,..} if matches!(detail.as_ref(),DnsDetail::Records {records,..} if !records.is_empty()))));
    assert!(events.iter().any(|event|matches!(event,Event::Dns {detail,..} if matches!(detail.as_ref(),DnsDetail::Failure {outcome,response_received:true,..} if outcome=="truncated_response"))));
    assert!(events
        .iter()
        .any(|event| matches!(event,Event::Attempt(attempt) if attempt.error.is_some())));
}
