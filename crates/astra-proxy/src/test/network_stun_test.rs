use super::*;
use crate::nettool::nat_fixture as fixture;
use astra_model::nettool::{AddressFamily, PinnedEgress, Probe, Request};
use std::{
    cell::RefCell,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

fn settings(server: SocketAddr) -> DetailedRequest {
    let mut request: DetailedRequest = Request {
        config: String::new(),
        egress: Egress::System,
        probe: Probe::Stun {
            server: server.to_string(),
        },
        count: 2,
        timeout_ms: 1000,
        interval_ms: 0,
    }
    .into();
    request.family = Some(AddressFamily::Ipv4);
    request
}
#[tokio::test]
async fn direct_stun_only_reports_one_session_observations() {
    let service = fixture::start(Default::default()).await;
    let request = settings(service.endpoints[0]);
    let events = RefCell::new(vec![]);
    let summary = run(&request, &service.endpoints[0].to_string(), &|event| {
        events.borrow_mut().push(event);
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(summary.received, 2, "events: {:?}", events.borrow());
    let events = events.borrow();
    let observations: Vec<_> = events
        .iter()
        .filter_map(|event| {
            if let Event::Stun(value) = event {
                Some(value)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(observations.len(), 2);
    assert!(observations
        .iter()
        .all(|result| !result.proxy_session && result.observation.response_received));
    assert_ne!(
        observations[0].observation.local_endpoint,
        observations[1].observation.local_endpoint
    );
    assert!(!events.iter().any(|event| matches!(event, Event::Nat(_))));
}
#[tokio::test]
async fn socks_udp_uses_explicit_relay_and_independent_owned_control_sessions() {
    let service = fixture::start(Default::default()).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = listener.local_addr().unwrap();
    let sessions = Arc::new(AtomicUsize::new(0));
    let closed = Arc::new(AtomicUsize::new(0));
    let session_count = sessions.clone();
    let closed_count = closed.clone();
    let server = tokio::spawn(async move {
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..2 {
            let (mut control, _) = listener.accept().await.unwrap();
            session_count.fetch_add(1, Ordering::SeqCst);
            let closed = closed_count.clone();
            tasks.spawn(async move {
                let (command, _) = astra_protocols::socks5::read_socks_request(&mut control)
                    .await
                    .unwrap();
                assert_eq!(command, 3);
                let relay = UdpSocket::bind("127.0.0.1:0").await.unwrap();
                let remote = UdpSocket::bind("127.0.0.1:0").await.unwrap();
                let relay_address = relay.local_addr().unwrap();
                let mut reply = vec![5, 0, 0];
                astra_protocols::outbound::append_socks_address(
                    &mut reply,
                    &Target {
                        host: relay_address.ip().to_string(),
                        port: relay_address.port(),
                    },
                )
                .unwrap();
                control.write_all(&reply).await.unwrap();
                let mut buffer = [0; 4096];
                let (len, client) = relay.recv_from(&mut buffer).await.unwrap();
                let (target, payload) =
                    astra_protocols::socks5::udp::decode(&buffer[..len]).unwrap();
                let target = SocketAddr::new(target.host.parse().unwrap(), target.port);
                remote.send_to(payload, target).await.unwrap();
                let (len, source) = remote.recv_from(&mut buffer).await.unwrap();
                let mut wrapped = vec![0, 0, 0];
                astra_protocols::outbound::append_socks_address(
                    &mut wrapped,
                    &Target {
                        host: source.ip().to_string(),
                        port: source.port(),
                    },
                )
                .unwrap();
                wrapped.extend_from_slice(&buffer[..len]);
                relay.send_to(&wrapped, client).await.unwrap();
                let mut one = [0];
                assert_eq!(control.read(&mut one).await.unwrap(), 0);
                closed.fetch_add(1, Ordering::SeqCst);
            });
        }
        while let Some(result) = tasks.join_next().await {
            result.unwrap();
        }
    });
    let mut request = settings(service.endpoints[0]);
    request.egress = Egress::Outbound("socks-fixture".into());
    request.pinned = Some(PinnedEgress {
        version: 1,
        requested_tag: "socks-fixture".into(),
        leaf_tag: "socks-fixture".into(),
        effective_interface: None,
        interface_source: None,
        selection_chain: vec![],
        node: Some(OutboundConfig::Socks5 {
            dialer_proxy: None,
            tag: "socks-fixture".into(),
            server: endpoint.to_string(),
            username: None,
            password: None,
        }),
    });
    let events = RefCell::new(vec![]);
    let summary = run(&request, &service.endpoints[0].to_string(), &|event| {
        events.borrow_mut().push(event);
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(summary.received, 2, "events: {:?}", events.borrow());
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(sessions.load(Ordering::SeqCst), 2);
    assert_eq!(closed.load(Ordering::SeqCst), 2);
    assert!(events
        .borrow()
        .iter()
        .filter_map(|event| if let Event::Stun(value) = event {
            Some(value)
        } else {
            None
        })
        .all(|value| value.proxy_session && value.observation.response_received));
}
#[tokio::test]
async fn unverified_udp_protocol_is_rejected_without_direct_probe() {
    let service = fixture::start(Default::default()).await;
    let mut request = settings(service.endpoints[0]);
    request.count = 1;
    request.egress = Egress::Outbound("http".into());
    request.pinned = Some(PinnedEgress {
        version: 1,
        requested_tag: "http".into(),
        leaf_tag: "http".into(),
        effective_interface: None,
        interface_source: None,
        selection_chain: vec![],
        node: Some(OutboundConfig::Http {
            dialer_proxy: None,
            tag: "http".into(),
            server: "127.0.0.1:9".into(),
            username: None,
            password: None,
        }),
    });
    let events = RefCell::new(vec![]);
    let summary = run(&request, &service.endpoints[0].to_string(), &|event| {
        events.borrow_mut().push(event);
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(summary.received, 0);
    assert!(service.contacts.lock().unwrap().is_empty());
    assert!(events.borrow().iter().any(|event|matches!(event,Event::Stun(result) if result.observation.error.as_ref().is_some_and(|s|s.contains("SOCKS5 only")))));
}
