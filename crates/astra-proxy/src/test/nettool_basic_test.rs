use super::*;
use astra_model::nettool::{AddressFamily, Egress, PinnedEgress, Request};
use std::sync::Mutex;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
fn request(probe: Probe) -> DetailedRequest {
    Request {
        config: String::new(),
        egress: Egress::System,
        probe,
        count: 1,
        interval_ms: 0,
        timeout_ms: 1000,
    }
    .into()
}
#[tokio::test]
async fn one_port_and_multiple_ports_share_typed_connection_outcomes() {
    let open = TcpListener::bind("[::1]:0").await.unwrap();
    let port = open.local_addr().unwrap().port();
    let closed = TcpListener::bind("[::1]:0").await.unwrap();
    let closed_port = closed.local_addr().unwrap().port();
    drop(closed);
    let expected_closed = ConnectionOutcome::Refused;
    #[cfg(windows)]
    let expected_closed = if std::env::var("ASTRA_TEST_TCP_CLOSED_TIMED_OUT").as_deref() == Ok("1")
    {
        // Opt in only after an independent OS socket probe confirmed that this
        // host drops closed-port connections. Do not relabel timeouts as refusals.
        let independent = tokio::time::timeout(
            std::time::Duration::from_millis(1000),
            tokio::net::TcpStream::connect((std::net::Ipv6Addr::LOCALHOST, closed_port)),
        )
        .await;
        assert!(
            independent.is_err(),
            "environment exception requires an actual closed-port timeout"
        );
        ConnectionOutcome::TimedOut
    } else {
        expected_closed
    };
    for (probe, expected) in [
        (
            Probe::Tcp {
                host: "::1".into(),
                port,
            },
            vec![ConnectionOutcome::Connected],
        ),
        (
            Probe::Tcp {
                host: "::1".into(),
                port: closed_port,
            },
            vec![expected_closed],
        ),
        (
            Probe::TcpPorts {
                host: "::1".into(),
                ports: vec![port, closed_port],
            },
            vec![ConnectionOutcome::Connected, expected_closed],
        ),
    ] {
        let events = Mutex::new(vec![]);
        let summary = super::super::detailed::run_context(&request(probe), |e| {
            events.lock().unwrap().push(e);
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(
            summary.received as usize,
            expected
                .iter()
                .filter(|outcome| **outcome == ConnectionOutcome::Connected)
                .count()
        );
        let events = events.into_inner().unwrap();
        let results: Vec<_> = events
            .iter()
            .filter_map(|event| {
                if let Event::Tcp { result, .. } = event {
                    Some(result)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(results.len(), expected.len());
        for (result, expected) in results.iter().zip(&expected) {
            assert_eq!(&result.outcome, expected);
            if *expected != ConnectionOutcome::Connected {
                assert!(result.connect_ms.is_none());
                assert!(!result.message.is_empty());
            }
        }
        for event in events {
            if let Event::Attempt(attempt) = event {
                if attempt.error.is_some() {
                    assert!(attempt.rtt_ms.is_none());
                }
            }
        }
    }
}
#[tokio::test]
async fn socks5_refusal_retains_protocol_error_kind() {
    for (code, expected) in [
        (3, ConnectionOutcome::Unreachable),
        (4, ConnectionOutcome::Unreachable),
        (5, ConnectionOutcome::Refused),
        (6, ConnectionOutcome::TimedOut),
    ] {
        let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = server.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            let (mut stream, _) = server.accept().await.unwrap();
            let mut greeting = [0; 3];
            stream.read_exact(&mut greeting).await.unwrap();
            assert_eq!(greeting, [5, 1, 0]);
            stream.write_all(&[5, 0]).await.unwrap();
            let mut connect = [0; 10];
            stream.read_exact(&mut connect).await.unwrap();
            stream
                .write_all(&[5, code, 0, 1, 0, 0, 0, 0, 0, 0])
                .await
                .unwrap();
        });
        let mut request = request(Probe::Tcp {
            host: "192.0.2.1".into(),
            port: 443,
        });
        request.egress = Egress::Outbound("fixture".into());
        request.pinned = Some(PinnedEgress {
            version: 1,
            requested_tag: "fixture".into(),
            leaf_tag: "fixture".into(),
            effective_interface: None,
            interface_source: None,
            selection_chain: vec![],
            node: Some(astra_model::config::protocol::OutboundConfig::Socks5 {
                tag: "fixture".into(),
                server: address.to_string(),
                username: None,
                password: None,
            }),
        });
        let events = Mutex::new(vec![]);
        run(&request, |e| {
            events.lock().unwrap().push(e);
            Ok(())
        })
        .await
        .unwrap();
        peer.await.unwrap();
        assert!(events
            .into_inner()
            .unwrap()
            .iter()
            .any(|e| matches!(e,Event::Tcp {result,..} if result.outcome==expected)));
    }
}
#[tokio::test]
async fn duplicate_ports_are_rejected_before_any_connection() {
    let events = Mutex::new(vec![]);
    assert!(run(
        &request(Probe::TcpPorts {
            host: "127.0.0.1".into(),
            ports: vec![80, 80]
        }),
        |e| {
            events.lock().unwrap().push(e);
            Ok(())
        }
    )
    .await
    .is_err());
    assert!(events.into_inner().unwrap().is_empty());
}

#[tokio::test]
async fn completed_partial_nat_still_terminates_the_versioned_stream() {
    let mut request = request(Probe::Nat {
        request: astra_model::network_nat::Request {
            agent_id: "fixture".into(),
            interface_id: "missing-physical-fixture".into(),
            family: AddressFamily::Ipv4,
            servers: vec!["127.0.0.1:3478".into()],
            advanced: false,
        },
    });
    request.egress = Egress::Interface("missing-physical-fixture".into());
    let events = Mutex::new(vec![]);
    super::super::detailed::run_context(&request, |e| {
        events.lock().unwrap().push(e);
        Ok(())
    })
    .await
    .unwrap();
    let events = events.into_inner().unwrap();
    assert!(matches!(events.first(), Some(Event::Started { .. })));
    assert!(events.iter().any(|e| matches!(e,Event::Nat(astra_model::network_nat::Event::Report {report}) if report.partial)));
    assert!(matches!(events.last(), Some(Event::Done)));
    assert_eq!(
        events.iter().filter(|e| matches!(e, Event::Done)).count(),
        1
    );
}
#[tokio::test]
async fn standalone_stun_dispatcher_emits_summary_and_completion() {
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let server = socket.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        let mut bytes = [0; 128];
        let (n, peer) = socket.recv_from(&mut bytes).await.unwrap();
        assert!(n >= 20);
        let mut reply = vec![1, 1, 0, 12, 0x21, 0x12, 0xa4, 0x42];
        reply.extend_from_slice(&bytes[8..20]);
        reply.extend_from_slice(&[0, 0x20, 0, 8, 0, 1]);
        reply.extend_from_slice(&(peer.port() ^ 0x2112).to_be_bytes());
        let std::net::IpAddr::V4(ip) = peer.ip() else {
            unreachable!()
        };
        reply.extend(
            ip.octets()
                .iter()
                .zip([0x21, 0x12, 0xa4, 0x42])
                .map(|(a, b)| a ^ b),
        );
        socket.send_to(&reply, peer).await.unwrap();
    });
    let events = Mutex::new(vec![]);
    super::super::detailed::run_context(
        &request(Probe::Stun {
            server: server.to_string(),
        }),
        |e| {
            events.lock().unwrap().push(e);
            Ok(())
        },
    )
    .await
    .unwrap();
    peer.await.unwrap();
    let events = events.into_inner().unwrap();
    assert!(matches!(events.first(), Some(Event::Started { .. })));
    assert!(events.iter().any(|e| matches!(e, Event::Summary(_))));
    assert!(matches!(events.last(), Some(Event::Done)));
}
