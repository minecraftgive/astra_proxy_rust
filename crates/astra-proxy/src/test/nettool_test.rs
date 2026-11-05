use super::*;
use std::sync::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn request(egress: Egress, probe: Probe) -> Request {
    Request {
        config: String::new(),
        egress,
        probe,
        count: 2,
        interval_ms: 0,
        timeout_ms: 2000,
    }
}
async fn collect(app: Option<&App>, request: Request) -> Result<Vec<Event>> {
    let events = Mutex::new(Vec::new());
    run(app, &request, false, |event| {
        events.lock().unwrap().push(event);
        Ok(())
    })
    .await?;
    Ok(events.into_inner().unwrap())
}

#[test]
fn server_forms_and_record_types_parse() {
    assert_eq!(
        parse_server("").unwrap(),
        (Transport::Udp, false, "1.1.1.1".into(), 53)
    );
    assert_eq!(
        parse_server("tls://dns.google").unwrap(),
        (Transport::Tls, true, "dns.google".into(), 853)
    );
    assert_eq!(
        parse_server("tcp://[2001:db8::1]:5353").unwrap(),
        (Transport::Tcp, true, "2001:db8::1".into(), 5353)
    );
    assert_eq!(
        parse_server("2001:db8::1").unwrap(),
        (Transport::Udp, false, "2001:db8::1".into(), 53)
    );
    assert!(parse_server("https://dns.google").is_err());
    assert_eq!(record_type("aaaa").unwrap(), RecordType::AAAA);
    assert!(record_type("NOPE").is_err());
}

#[tokio::test]
async fn tcp_ping_reports_each_attempt_and_refused_ports() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            drop(socket);
        }
    });
    let events = collect(
        None,
        request(
            Egress::System,
            Probe::Tcp {
                host: "127.0.0.1".into(),
                port,
            },
        ),
    )
    .await
    .unwrap();
    assert!(matches!(events[0], Event::Started { .. }));
    let Event::Summary(summary) = &events[3] else {
        panic!("{events:?}")
    };
    assert_eq!((summary.sent, summary.received), (2, 2));
    assert_eq!(events.last(), Some(&Event::Done));

    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let events = collect(
        None,
        request(
            Egress::System,
            Probe::Tcp {
                host: "127.0.0.1".into(),
                port,
            },
        ),
    )
    .await
    .unwrap();
    let Event::Attempt(attempt) = &events[1] else {
        panic!()
    };
    assert!(attempt.error.is_some() && attempt.rtt_ms.is_none());
}

async fn dns_server() -> (SocketAddr, SocketAddr) {
    let udp = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addresses = (udp.local_addr().unwrap(), tcp.local_addr().unwrap());
    fn answer(query: &[u8]) -> Vec<u8> {
        let query = Message::from_vec(query).unwrap();
        let mut reply = astra_dns::wire::response(&query, hickory_proto::op::ResponseCode::NoError);
        reply.add_answer(hickory_proto::rr::Record::from_rdata(
            query.queries[0].name().clone(),
            60,
            hickory_proto::rr::RData::A("192.0.2.7".parse().unwrap()),
        ));
        reply.to_vec().unwrap()
    }
    tokio::spawn(async move {
        let mut buffer = [0; 1500];
        while let Ok((size, peer)) = udp.recv_from(&mut buffer).await {
            udp.send_to(&answer(&buffer[..size]), peer).await.unwrap();
        }
    });
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = tcp.accept().await {
            let size = socket.read_u16().await.unwrap() as usize;
            let mut query = vec![0; size];
            socket.read_exact(&mut query).await.unwrap();
            let reply = answer(&query);
            socket.write_u16(reply.len() as u16).await.unwrap();
            socket.write_all(&reply).await.unwrap();
        }
    });
    addresses
}

#[tokio::test]
async fn dns_queries_over_udp_tcp_and_through_a_proxy_outbound() {
    let (udp, tcp) = dns_server().await;
    for server in [udp.to_string(), format!("tcp://{tcp}")] {
        let events = collect(
            None,
            request(
                Egress::System,
                Probe::Dns {
                    name: "example.test".into(),
                    record: "A".into(),
                    server,
                },
            ),
        )
        .await
        .unwrap();
        let Event::Attempt(attempt) = &events[1] else {
            panic!()
        };
        assert!(attempt.detail.contains("192.0.2.7"), "{attempt:?}");
    }
    // A SOCKS5 outbound carries the query as DNS-over-TCP; bare servers switch from UDP.
    let socks = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let socks_address = socks.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut client, _) = socks.accept().await.unwrap();
        let mut greeting = [0; 3];
        client.read_exact(&mut greeting).await.unwrap();
        client.write_all(&[5, 0]).await.unwrap();
        let mut header = [0; 4];
        client.read_exact(&mut header).await.unwrap();
        assert_eq!(
            header[3], 1,
            "proxy must receive the literal server address"
        );
        let mut rest = [0; 6];
        client.read_exact(&mut rest).await.unwrap();
        client
            .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])
            .await
            .unwrap();
        let mut upstream = tokio::net::TcpStream::connect(tcp).await.unwrap();
        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    });
    let app = App::from_text(
            std::path::Path::new("nettool.toml"),
            format!("standalone=true\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\nkind='socks5'\ntag='edge'\nserver='{socks_address}'\n"),
        )
        .unwrap();
    let mut dns = request(
        Egress::Outbound("edge".into()),
        Probe::Dns {
            name: "example.test".into(),
            record: "A".into(),
            server: tcp.to_string(),
        },
    );
    dns.count = 1;
    let events = collect(Some(&app), dns.clone()).await.unwrap();
    let Event::Started { note, .. } = &events[0] else {
        panic!()
    };
    assert!(note.contains("over TCP"), "{note}");
    let Event::Attempt(attempt) = &events[1] else {
        panic!()
    };
    assert!(attempt.detail.contains("192.0.2.7"), "{attempt:?}");

    if let Probe::Dns { server, .. } = &mut dns.probe {
        *server = format!("udp://{udp}");
    }
    assert!(collect(Some(&app), dns).await.is_err());
    let icmp = request(
        Egress::Outbound("edge".into()),
        Probe::Icmp {
            host: "127.0.0.1".into(),
        },
    );
    assert!(format!("{:#}", collect(Some(&app), icmp).await.unwrap_err()).contains("ICMP"));
    let missing = request(
        Egress::Outbound("missing".into()),
        Probe::Icmp {
            host: "127.0.0.1".into(),
        },
    );
    assert!(collect(Some(&app), missing).await.is_err());
}

#[tokio::test]
async fn icmp_echo_reaches_loopback() {
    let mut probe = request(
        Egress::System,
        Probe::Icmp {
            host: "127.0.0.1".into(),
        },
    );
    probe.count = 1;
    match collect(None, probe).await {
        Ok(events) => {
            let Event::Attempt(attempt) = &events[1] else {
                panic!()
            };
            assert!(attempt.error.is_none(), "{attempt:?}");
        }
        // Unix hosts may forbid unprivileged ICMP sockets (net.ipv4.ping_group_range).
        Err(error) if cfg!(unix) => eprintln!("skipped: {error:#}"),
        Err(error) => panic!("{error:#}"),
    }
}
