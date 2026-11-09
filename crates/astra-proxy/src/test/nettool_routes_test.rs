use super::*;
use astra_model::{
    config::protocol::OutboundConfig, nettool::PinnedEgress, network_state::Resolver,
};
use hickory_proto::rr::{Name, Record};
use std::sync::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn tcp_request(host: &str, port: u16) -> DetailedRequest {
    Request {
        config: String::new(),
        egress: Egress::System,
        probe: Probe::Tcp {
            host: host.into(),
            port,
        },
        count: 1,
        interval_ms: 0,
        timeout_ms: 1000,
    }
    .into()
}
fn pin(request: &mut DetailedRequest, node: Option<OutboundConfig>) {
    let tag = node.as_ref().map_or("DIRECT", |n| n.tag()).to_owned();
    request.egress = Egress::Outbound(tag.clone());
    request.pinned = Some(PinnedEgress {
        version: 1,
        requested_tag: tag.clone(),
        leaf_tag: tag,
        effective_interface: None,
        interface_source: None,
        selection_chain: vec![],
        node,
    });
}
async fn collect(request: &DetailedRequest) -> (Summary, Vec<Event>) {
    let events = Mutex::new(Vec::new());
    let summary = run_context(request, |e| {
        events.lock().unwrap().push(e);
        Ok(())
    })
    .await
    .unwrap();
    (summary, events.into_inner().unwrap())
}
async fn resolver_fixture() -> (String, tokio::task::JoinHandle<String>) {
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let server = socket.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let mut raw = vec![0; 4096];
        let (n, peer) = socket.recv_from(&mut raw).await.unwrap();
        let query = Message::from_vec(&raw[..n]).unwrap();
        let name = query.queries[0].name().clone();
        let mut reply = astra_dns::wire::response(&query, ResponseCode::NoError);
        // Include an unrelated address first: it must never drive the connection.
        reply.add_answer(Record::from_rdata(
            Name::from_ascii("unrelated.test.").unwrap(),
            1,
            RData::A("192.0.2.7".parse().unwrap()),
        ));
        reply.add_answer(Record::from_rdata(
            name.clone(),
            1,
            RData::A("127.0.0.1".parse().unwrap()),
        ));
        socket
            .send_to(&reply.to_vec().unwrap(), peer)
            .await
            .unwrap();
        name.to_string()
    });
    (format!("udp://{server}"), task)
}
#[tokio::test]
async fn explicit_resolver_drives_direct_hostname_without_system_lookup() {
    SYSTEM_LOOKUPS.scope(std::cell::Cell::new(0),async {
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (resolver,task)=resolver_fixture().await;
        let mut request=tcp_request("business.test",listener.local_addr().unwrap().port());
        request.resolver=Some(resolver);
        pin(&mut request,None);
        let (summary,events)=collect(&request).await;
        assert_eq!(summary.received,1);
        listener.accept().await.unwrap();
        assert_eq!(task.await.unwrap(),"business.test.");
        assert!(events.iter().any(|e|matches!(e,Event::Resolution {purpose,method,addresses,..} if purpose=="business_target" && method=="explicit_resolver" && !addresses.is_empty())));
        SYSTEM_LOOKUPS.with(|n|assert_eq!(n.get(),0));
    }).await;
}
async fn proxy_fixture(http: bool, dns: bool) -> (String, tokio::task::JoinHandle<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let host = if http {
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                header.push(stream.read_u8().await.unwrap());
                assert!(header.len() < 8192);
            }
            let header = String::from_utf8(header).unwrap();
            let authority = header.split_ascii_whitespace().nth(1).unwrap().to_owned();
            stream
                .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .await
                .unwrap();
            authority
        } else {
            let mut greeting = [0; 3];
            stream.read_exact(&mut greeting).await.unwrap();
            assert_eq!(greeting, [5, 1, 0]);
            stream.write_all(&[5, 0]).await.unwrap();
            let mut header = [0; 4];
            stream.read_exact(&mut header).await.unwrap();
            assert_eq!(header, [5, 1, 0, 3]);
            let n = stream.read_u8().await.unwrap();
            let mut name = vec![0; n as usize];
            stream.read_exact(&mut name).await.unwrap();
            let port = stream.read_u16().await.unwrap();
            stream
                .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])
                .await
                .unwrap();
            format!("{}:{port}", String::from_utf8(name).unwrap())
        };
        if dns {
            let n = stream.read_u16().await.unwrap();
            let mut wire = vec![0; n as usize];
            stream.read_exact(&mut wire).await.unwrap();
            let query = Message::from_vec(&wire).unwrap();
            let reply = astra_dns::wire::response(&query, ResponseCode::NXDomain)
                .to_vec()
                .unwrap();
            stream.write_u16(reply.len() as u16).await.unwrap();
            stream.write_all(&reply).await.unwrap();
        }
        host
    });
    (address, task)
}
fn proxy(server: String, http: bool) -> OutboundConfig {
    if http {
        OutboundConfig::Http {
            tag: "proxy".into(),
            server,
            username: None,
            password: None,
        }
    } else {
        OutboundConfig::Socks5 {
            tag: "proxy".into(),
            server,
            username: None,
            password: None,
        }
    }
}
#[tokio::test]
async fn both_proxy_handshakes_keep_business_dns_remote_and_entry_resolution_separate() {
    SYSTEM_LOOKUPS.scope(std::cell::Cell::new(0),async {
        for http in [false,true] {
            let (address,server)=proxy_fixture(http,false).await;
            let (resolver,lookup)=resolver_fixture().await;
            let port=address.rsplit_once(':').unwrap().1;
            let mut request=tcp_request("business.invalid",8443);
            request.resolver=Some(resolver);
            pin(&mut request,Some(proxy(format!("entry.test:{port}"),http)));
            let (summary,events)=collect(&request).await;
            assert_eq!(summary.received,1,"{events:?}");
            assert_eq!(lookup.await.unwrap(),"entry.test.");
            assert_eq!(server.await.unwrap(),"business.invalid:8443");
            assert!(events.iter().any(|e|matches!(e,Event::Resolution {purpose,method,..} if purpose=="proxy_entry" && method=="explicit_resolver")));
            assert!(events.iter().any(|e|matches!(e,Event::Resolution {purpose,method,..} if purpose=="business_target" && method=="remote_proxy")));
        }
        SYSTEM_LOOKUPS.with(|n|assert_eq!(n.get(),0));
    }).await;
}
#[tokio::test]
async fn dns_tcp_stays_inside_each_proxy_tunnel_and_preserves_remote_service_name() {
    SYSTEM_LOOKUPS.scope(std::cell::Cell::new(0),async {
        for http in [false,true] {
            let (address,server)=proxy_fixture(http,true).await;
            let mut request=tcp_request("unused",443);
            request.probe=Probe::Dns {name:"business.invalid".into(),record:"A".into(),server:"tcp://resolver.invalid:5353".into()};
            pin(&mut request,Some(proxy(address,http)));
            let (summary,events)=collect(&request).await;
            assert_eq!(summary.received,1,"{events:?}");
            assert_eq!(server.await.unwrap(),"resolver.invalid:5353");
            assert!(events.iter().any(|e|matches!(e,Event::Dns {detail,..} if matches!(detail.as_ref(),DnsDetail::Response {outcome,..} if outcome=="nxdomain"))));
        }
        SYSTEM_LOOKUPS.with(|n|assert_eq!(n.get(),0));
    }).await;
}
#[tokio::test]
async fn ambiguous_bootstrap_and_unsupported_proxy_combinations_never_use_system_dns() {
    SYSTEM_LOOKUPS.scope(std::cell::Cell::new(0),async {
        let mut request=tcp_request("business.invalid",443);
        pin(&mut request,None);
        let (summary,events)=collect(&request).await;
        assert_eq!(summary.received,0);
        assert!(events.iter().any(|e|matches!(e,Event::Attempt(a) if a.error.as_ref().is_some_and(|e|e.contains("system DNS fallback is forbidden")))));
        pin(&mut request,Some(proxy("entry.invalid:1080".into(),false)));
        let (summary,events)=collect(&request).await;
        assert_eq!(summary.received,0);
        assert!(events.iter().any(|e|matches!(e,Event::Attempt(a) if a.error.as_ref().is_some_and(|e|e.contains("system DNS fallback is forbidden")))));
        request.resolver=Some("https://resolver.invalid/dns-query".into());
        assert!(run_context(&request, |_|Ok(())).await.unwrap_err().to_string().contains("bootstrap"));
        request.resolver=None;
        request.probe=Probe::Dns {name:"business.invalid".into(),record:"A".into(),server:"udp://127.0.0.1:53".into()};
        assert!(run_context(&request, |_|Ok(())).await.unwrap_err().to_string().contains("UDP"));
        request.probe=Probe::Tcp {host:"business.invalid".into(),port:443};
        pin(&mut request,Some(OutboundConfig::TrojanTls {tag:"x".into(),server:"127.0.0.1:443".into(),sni:"x".into(),password:"secret".into()}));
        let error=run_context(&request, |_|Ok(())).await.unwrap_err().to_string();
        assert!(error.contains("Unsupported"));assert!(!error.contains("secret"));
        SYSTEM_LOOKUPS.with(|n|assert_eq!(n.get(),0));
    }).await;
}
fn resolver(interface: &str, scope: &str, domain: &str, server: &str) -> Resolver {
    Resolver {
        id: server.into(),
        source: "fixture".into(),
        interface_id: Some(interface.into()),
        scope: scope.into(),
        domains: vec![domain.into()],
        servers: vec![server.into()],
        priority: None,
        flags: vec![],
    }
}
#[test]
fn automatic_resolver_requires_same_interface_reliable_scope_and_longest_applicable_domain() {
    #[cfg(target_os = "macos")]
    let interface = "lo0";
    #[cfg(not(target_os = "macos"))]
    let interface = "lo";
    let mut request = tcp_request("service.corp.test", 443);
    request.egress = Egress::Interface(interface.into());
    request.interface_resolvers = vec![
        resolver(interface, "global", "corp.test", "192.0.2.1"),
        resolver("other", "link", "corp.test", "192.0.2.2"),
        resolver(interface, "link", "other.test", "192.0.2.3"),
        resolver(interface, "link", "corp.test", "resolver.invalid"),
        resolver(interface, "link", "~.", "192.0.2.4"),
        resolver(interface, "link", "corp.test", "192.0.2.5"),
    ];
    let route = path::Route::new(&request).unwrap();
    assert_eq!(
        route.selected_resolver("service.corp.test").unwrap().0,
        "192.0.2.5"
    );
    assert_eq!(
        route.selected_resolver("other.invalid").unwrap().0,
        "192.0.2.4"
    );
    request.resolver = Some("udp://127.0.0.1:5353".into());
    assert_eq!(
        path::Route::new(&request)
            .unwrap()
            .selected_resolver("service.corp.test")
            .unwrap()
            .1,
        "explicit_resolver"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn named_tcp_binds_resolver_and_business_socket_to_the_selected_interface() {
    SYSTEM_LOOKUPS
        .scope(std::cell::Cell::new(0), async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let (resolver, task) = resolver_fixture().await;
            let mut request = tcp_request("business.test", listener.local_addr().unwrap().port());
            #[cfg(target_os = "macos")]
            let interface = "lo0";
            #[cfg(not(target_os = "macos"))]
            let interface = "lo";
            request.egress = Egress::Interface(interface.into());
            request.resolver = Some(resolver);
            let (summary, events) = collect(&request).await;
            assert_eq!(summary.received, 1, "{events:?}");
            listener.accept().await.unwrap();
            task.await.unwrap();
            assert!(events.iter().any(
                |e| matches!(e,Event::Resolution {interface:Some(bound),..} if bound==interface)
            ));
            SYSTEM_LOOKUPS.with(|n| assert_eq!(n.get(), 0));
        })
        .await;
}

#[cfg(unix)]
#[tokio::test]
async fn automatic_split_dns_rejects_cname_outside_its_applicable_scope() {
    SYSTEM_LOOKUPS
        .scope(std::cell::Cell::new(0), async {
            #[cfg(target_os = "macos")]
            let interface = "lo0";
            #[cfg(not(target_os = "macos"))]
            let interface = "lo";
            let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let address = socket.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let mut raw = vec![0; 4096];
                let (n, peer) = socket.recv_from(&mut raw).await.unwrap();
                let query = Message::from_vec(&raw[..n]).unwrap();
                let mut reply = astra_dns::wire::response(&query, ResponseCode::NoError);
                let target = Name::from_ascii("outside.invalid.").unwrap();
                reply.add_answer(Record::from_rdata(
                    query.queries[0].name().clone(),
                    1,
                    RData::CNAME(hickory_proto::rr::rdata::CNAME(target.clone())),
                ));
                reply.add_answer(Record::from_rdata(
                    target,
                    1,
                    RData::A("127.0.0.1".parse().unwrap()),
                ));
                socket
                    .send_to(&reply.to_vec().unwrap(), peer)
                    .await
                    .unwrap();
            });
            let mut request = tcp_request("service.corp.test", 443);
            request.egress = Egress::Interface(interface.into());
            request.interface_resolvers =
                vec![resolver(interface, "link", "corp.test", "127.0.0.1")];
            let route = path::Route::new(&request).unwrap();
            let endpoint = Endpoint::parse(&format!("udp://{address}")).unwrap();
            let error = route
                .lookup(
                    1,
                    "business_target",
                    "service.corp.test",
                    &endpoint,
                    address,
                    &|_| Ok(()),
                )
                .await
                .unwrap_err();
            assert!(error
                .to_string()
                .contains("CNAME crosses automatic resolver scope"));
            server.await.unwrap();
            SYSTEM_LOOKUPS.with(|n| assert_eq!(n.get(), 0));
        })
        .await;
}

#[tokio::test]
async fn reports_omit_doh_path_and_query_secrets_but_preserve_the_wire_endpoint() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);
        }
    });
    let url=format!("https://resolver.invalid:{port}/private-path-secret?token=query-secret#bootstrap=127.0.0.1");
    let parsed = Endpoint::parse(&url).unwrap();
    assert_eq!(parsed.path, "/private-path-secret?token=query-secret");
    let mut request = tcp_request("business.invalid", 443);
    request.resolver = Some(url.clone());
    let (_, events) = collect(&request).await;
    let report = serde_json::to_string(&events).unwrap();
    assert!(
        !report.contains("private-path-secret") && !report.contains("query-secret"),
        "{report}"
    );
    assert!(report.contains("resolver.invalid"));
    request.resolver = None;
    request.probe = Probe::Dns {
        name: "example.test".into(),
        record: "A".into(),
        server: url,
    };
    let (_, events) = collect(&request).await;
    let report = serde_json::to_string(&events).unwrap();
    assert!(
        !report.contains("private-path-secret") && !report.contains("query-secret"),
        "{report}"
    );
    assert!(report.contains("resolver.invalid"));
    server.await.unwrap();
    for value in [
        "https://user:userinfo-secret@resolver.invalid/dns-query",
        "tls://user:userinfo-secret@resolver.invalid:853",
        "tcp://resolver.invalid:0?token=query-secret",
    ] {
        let error = match Endpoint::parse(value) {
            Ok(_) => panic!("private malformed endpoint accepted"),
            Err(error) => error.to_string(),
        };
        assert!(!error.contains("userinfo-secret") && !error.contains("query-secret"));
    }
}

#[tokio::test]
async fn cancelling_or_timing_out_each_proxy_handshake_closes_its_owned_socket() {
    for http in [false, true] {
        for cancel in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap().to_string();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                if http {
                    let mut header = Vec::new();
                    while !header.ends_with(b"\r\n\r\n") {
                        header.push(stream.read_u8().await.unwrap());
                    }
                } else {
                    let mut greeting = [0; 3];
                    stream.read_exact(&mut greeting).await.unwrap();
                }
                // Do not answer the handshake. Dropping the parent operation
                // must release this connection without a detached worker.
                let mut rest = Vec::new();
                tokio::time::timeout(Duration::from_secs(1), stream.read_to_end(&mut rest))
                    .await
                    .unwrap()
                    .unwrap();
            });
            let mut request = tcp_request("remote.invalid", 443);
            pin(&mut request, Some(proxy(address, http)));
            request.timeout_ms = if cancel { 1000 } else { 100 };
            if cancel {
                assert!(tokio::time::timeout(
                    Duration::from_millis(100),
                    run_context(&request, |_| Ok(()))
                )
                .await
                .is_err());
            } else {
                assert_eq!(collect(&request).await.0.received, 0);
            }
            server.await.unwrap();
        }
    }
}

#[tokio::test]
async fn timing_out_a_constrained_tcp_resolver_drops_its_connection() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let n = stream.read_u16().await.unwrap();
        let mut query = vec![0; n as usize];
        stream.read_exact(&mut query).await.unwrap();
        let mut rest = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), stream.read_to_end(&mut rest))
            .await
            .unwrap()
            .unwrap();
    });
    let mut request = tcp_request("business.invalid", 443);
    request.resolver = Some(format!("tcp://{address}"));
    request.timeout_ms = 100;
    assert_eq!(collect(&request).await.0.received, 0);
    server.await.unwrap();
}

#[tokio::test]
async fn cancelling_doh_drops_both_driver_and_body_and_keeps_private_query_on_wire() {
    let (client, mut server) = tokio::io::duplex(8192);
    let endpoint =
        Endpoint::parse("https://resolver.invalid/private-path?token=wire-secret").unwrap();
    let peer = tokio::spawn(async move {
        let mut header = Vec::new();
        while !header.ends_with(b"\r\n\r\n") {
            header.push(server.read_u8().await.unwrap());
        }
        let text = String::from_utf8(header).unwrap();
        assert!(text.starts_with("POST /private-path?token=wire-secret HTTP/1.1"));
        let n = text
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length: ")
                    .and_then(|s| s.parse::<usize>().ok())
            })
            .unwrap();
        let mut body = vec![0; n];
        server.read_exact(&mut body).await.unwrap();
        let mut rest = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), server.read_to_end(&mut rest))
            .await
            .unwrap()
            .unwrap();
    });
    assert!(tokio::time::timeout(
        Duration::from_millis(100),
        doh_http(client, &endpoint, &[1, 2, 3])
    )
    .await
    .is_err());
    peer.await.unwrap();
}

#[tokio::test]
async fn ipv6_business_query_can_use_an_ipv4_explicit_resolver_without_system_lookup() {
    SYSTEM_LOOKUPS
        .scope(std::cell::Cell::new(0), async {
            let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
            let resolver = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let address = resolver.local_addr().unwrap();
            let worker = tokio::spawn(async move {
                let mut wire = vec![0; 4096];
                let (n, peer) = resolver.recv_from(&mut wire).await.unwrap();
                let query = Message::from_vec(&wire[..n]).unwrap();
                assert_eq!(query.queries[0].query_type(), RecordType::AAAA);
                let mut reply = astra_dns::wire::response(&query, ResponseCode::NoError);
                reply.add_answer(Record::from_rdata(
                    query.queries[0].name().clone(),
                    30,
                    RData::AAAA("::1".parse().unwrap()),
                ));
                resolver
                    .send_to(&reply.to_vec().unwrap(), peer)
                    .await
                    .unwrap();
            });
            let mut request = tcp_request("v6.test", listener.local_addr().unwrap().port());
            request.family = Some(astra_model::nettool::AddressFamily::Ipv6);
            request.resolver = Some(format!("udp://{address}"));
            let (summary, events) = collect(&request).await;
            assert_eq!(summary.received, 1, "{events:?}");
            listener.accept().await.unwrap();
            worker.await.unwrap();
            SYSTEM_LOOKUPS.with(|n| assert_eq!(n.get(), 0));
        })
        .await;
}
#[tokio::test]
async fn ipv4_proxy_entry_can_carry_a_literal_ipv6_business_target() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let worker = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut greeting = [0; 3];
        stream.read_exact(&mut greeting).await.unwrap();
        stream.write_all(&[5, 0]).await.unwrap();
        let mut header = [0; 4];
        stream.read_exact(&mut header).await.unwrap();
        assert_eq!(header, [5, 1, 0, 4]);
        let mut ip = [0; 16];
        stream.read_exact(&mut ip).await.unwrap();
        assert_eq!(
            std::net::Ipv6Addr::from(ip),
            "2001:db8::7".parse::<std::net::Ipv6Addr>().unwrap()
        );
        assert_eq!(stream.read_u16().await.unwrap(), 443);
        stream
            .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])
            .await
            .unwrap();
    });
    let mut request = tcp_request("2001:db8::7", 443);
    request.family = Some(astra_model::nettool::AddressFamily::Ipv6);
    pin(&mut request, Some(proxy(address.to_string(), false)));
    let (summary, events) = collect(&request).await;
    assert_eq!(summary.received, 1, "{events:?}");
    worker.await.unwrap();
}
