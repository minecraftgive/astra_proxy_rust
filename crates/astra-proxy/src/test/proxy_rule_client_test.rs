use super::*;
use astra_model::Inbound;
use astra_model::Network;
use astra_model::RouteAction;
use astra_model::Target;
fn request() -> RouteRequest {
    RouteRequest {
        destination_ip: None,
        api_version: API_VERSION,
        id: 0,
        target: Target {
            host: "example.com".into(),
            port: 443,
        },
        network: Network::Tcp,
        inbound: Inbound::Socks5,
        process: None,
    }
}

#[tokio::test]
async fn poisoned_idle_pool_discards_channels_without_panicking_during_lease_drop() {
    let client = RuleClient::new("/nonexistent/astra-poison-test.sock".into(), 2, 100);
    let (cached, mut peer) = astra_platform::platform::stream_pair().await.unwrap();
    *client.idle() = vec![Some(cached)];
    let lease = Lease {
        owner: &client,
        stream: None, // a request owns its stream outside the idle lease
        _permit: client.available.clone().acquire_owned().await.unwrap(),
    };
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _idle = client.idle.lock().unwrap();
        panic!("injected idle pool update panic");
    }))
    .is_err());
    let original = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _lease = lease;
        panic!("original request panic");
    }))
    .unwrap_err();
    assert_eq!(
        original.downcast_ref::<&str>(),
        Some(&"original request panic")
    );
    use tokio::io::AsyncReadExt;
    assert_eq!(
        timeout(Duration::from_secs(1), peer.read(&mut [0]))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    assert!(!client.idle.is_poisoned());
    assert_eq!(client.available.available_permits(), 2);
    // The cache can have fewer entries than permits after repair. A cache
    // miss must reconnect/fail closed, never panic or invent a direct route.
    let (first, second) = tokio::join!(client.decide(request()), client.decide(request()));
    assert!(first.is_err() && second.is_err());
    assert_eq!(client.available.available_permits(), 2);
    assert!(client.idle().len() <= client.capacity);
}

#[tokio::test]
async fn routing_context_uses_original_destination_only_for_transparent_inbounds() {
    let app = crate::App::from_text(
        &std::env::temp_dir().join("astra-route-context.toml"),
        "rule_socket='unused'\nipc_connections=1\nsocks5_listen='127.0.0.1:1080'\n".into(),
    )
    .unwrap();
    let cases = [
        (Inbound::Socks5, "qq.com", "127.0.0.1:1080", None),
        (Inbound::HttpConnect, "qq.com", "127.0.0.1:8080", None),
        (
            Inbound::Tun,
            "qq.com",
            "203.0.113.8:443",
            Some("203.0.113.8"),
        ),
        (
            Inbound::Socks5,
            "203.0.113.8",
            "127.0.0.1:1080",
            Some("203.0.113.8"),
        ),
    ];
    let mut remote = connected_rule_peer(&app).await;
    let server = tokio::spawn(async move {
        for (inbound, host, _, ip) in cases {
            let req: RouteRequest = astra_ipc::receive(&mut remote).await.unwrap();
            assert_eq!(req.inbound, inbound);
            assert_eq!(req.target.host, host);
            assert_eq!(req.destination_ip, ip.map(|ip| ip.parse().unwrap()));
            astra_ipc::send(
                &mut remote,
                &RouteResponse {
                    api_version: API_VERSION,
                    id: req.id,
                    revision: 1,
                    action: RouteAction::Block,
                },
            )
            .await
            .unwrap();
        }
    });
    for (inbound, host, destination, _) in cases {
        assert_eq!(
            crate::inbound::Flow::new(
                &Target {
                    host: host.into(),
                    port: 443
                },
                inbound,
                Network::Tcp,
                None,
                Some(destination.parse().unwrap()),
            )
            .plan(&app, crate::inbound::FlowPolicy::Requested)
            .await
            .unwrap()
            .action,
            RouteAction::Block
        );
    }
    server.await.unwrap();
}

#[tokio::test]
async fn fake_ip_restores_domain_before_rule_ipc_for_tcp_and_udp() {
    let text="rule_socket='unused'\nipc_connections=1\nsocks5_listen='127.0.0.1:1080'\n[dns]\nlisten=[]\nfake_ip=true\n";
    let app = crate::App::from_text(
        &std::env::temp_dir().join("astra-dns-rule.toml"),
        text.into(),
    )
    .unwrap();
    let mut remote = connected_rule_peer(&app).await;
    let server = tokio::spawn(async move {
        for network in [Network::Tcp, Network::Udp] {
            let request: RouteRequest = astra_ipc::receive(&mut remote).await.unwrap();
            assert_eq!(request.target.host, "blocked.example");
            assert_eq!(request.network, network);
            astra_ipc::send(
                &mut remote,
                &RouteResponse {
                    api_version: API_VERSION,
                    id: request.id,
                    revision: 1,
                    action: RouteAction::Block,
                },
            )
            .await
            .unwrap();
        }
    });
    let query = astra_dns::wire::query("blocked.example", hickory_proto::rr::RecordType::A)
        .unwrap()
        .to_vec()
        .unwrap();
    let reply =
        hickory_proto::op::Message::from_vec(&app.dns_answer(&query).await.unwrap()).unwrap();
    let hickory_proto::rr::RData::A(ip) = reply.answers[0].data else {
        panic!()
    };
    let target = app
        .restore_target(&Target {
            host: ip.0.to_string(),
            port: 443,
        })
        .unwrap();
    for network in [Network::Tcp, Network::Udp] {
        assert_eq!(
            app.route_network(&target, Inbound::Tun, network)
                .await
                .unwrap(),
            RouteAction::Block
        );
    }
    server.await.unwrap();
}

#[tokio::test]
async fn timeout_discards_channel_and_returns_capacity() {
    let client = RuleClient::new("/nonexistent/astra-test.sock".into(), 1, 30);
    let (local, mut remote) = astra_platform::platform::stream_pair().await.unwrap();
    client.idle.lock().unwrap()[0] = Some(local);
    let server = tokio::spawn(async move {
        let _: RouteRequest = astra_ipc::receive(&mut remote).await.unwrap();
        let mut byte = [0];
        use tokio::io::AsyncReadExt;
        assert_eq!(remote.read(&mut byte).await.unwrap(), 0);
    });
    let err = client.decide(request()).await.unwrap_err();
    assert!(err.to_string().contains("deadline"));
    assert_eq!(client.available.available_permits(), 1);
    assert!(client.idle.lock().unwrap()[0].is_none());
    server.await.unwrap();
}

#[tokio::test]
async fn busy_channel_does_not_block_an_idle_channel() {
    let client = Arc::new(RuleClient::new("unused".into(), 2, 500));
    let (fast, mut fast_peer) = astra_platform::platform::stream_pair().await.unwrap();
    let (slow, mut slow_peer) = astra_platform::platform::stream_pair().await.unwrap();
    *client.idle.lock().unwrap() = vec![Some(fast), Some(slow)];
    let (started, received) = tokio::sync::oneshot::channel();
    let slow_server = tokio::spawn(async move {
        let _: RouteRequest = astra_ipc::receive(&mut slow_peer).await.unwrap();
        started.send(()).unwrap();
        std::future::pending::<()>().await;
    });
    let worker = {
        let client = client.clone();
        tokio::spawn(async move { client.decide(request()).await })
    };
    received.await.unwrap();
    let fast_server = tokio::spawn(async move {
        let req: RouteRequest = astra_ipc::receive(&mut fast_peer).await.unwrap();
        astra_ipc::send(
            &mut fast_peer,
            &RouteResponse {
                api_version: API_VERSION,
                revision: 1,
                id: req.id,
                action: RouteAction::Block,
            },
        )
        .await
        .unwrap();
    });
    assert_eq!(
        timeout(Duration::from_millis(100), client.decide(request()))
            .await
            .unwrap()
            .unwrap()
            .action,
        RouteAction::Block
    );
    worker.abort();
    let _ = worker.await;
    slow_server.abort();
    fast_server.await.unwrap();
    assert_eq!(client.available.available_permits(), 2);
}

async fn connected_rule_peer(app: &crate::App) -> UnixStream {
    let (local, remote) = astra_platform::platform::stream_pair().await.unwrap();
    app.rules.rule_client.as_ref().unwrap().idle.lock().unwrap()[0] = Some(local);
    remote
}

/// Exercise the same stream and datagram entry points used by the TUN stack.
/// The real rule engine answers over private IPC, with default BLOCK so that
/// losing the recovered name cannot accidentally reach either echo server.
#[tokio::test]
async fn tun_fake_ip_routes_tcp_and_udp_through_the_domain_group() {
    use crate::{
        chain_tests::{node, socks_server},
        inbound, App,
    };
    use astra_rule::engine::{RuleConfig, RuleEngine};
    use std::net::SocketAddr;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, UdpSocket},
    };
    timeout(Duration::from_secs(10), async {
        let node_server = socks_server().await;
        let rule_config: RuleConfig = toml::from_str(
            "socket='unused'\ndirect_local=false\ndefault_action='block'\n[[rules]]\nkind='domain_exact'\nvalue='localhost'\naction='proxy:selected'\n"
        ).unwrap();
        let engine = RuleEngine::compile_at(&rule_config, std::path::Path::new(".")).unwrap();
        let config = format!("rule_socket='unused'\nipc_connections=1\nsocks5_listen='127.0.0.1:1080'\n{}\n[proxy_groups.selected]\nmembers=['edge']\n[dns]\nlisten=[]\nfake_ip=true\nfake_ip_filter=[]\n", node("edge", node_server.addr, None));
        let app = Arc::new(App::from_text(std::path::Path::new("tun-domain-group.toml"), config).unwrap());
        let mut stream = connected_rule_peer(&app).await;
        let rules = tokio::spawn(async move {
            for network in [Network::Tcp, Network::Udp] {
                let request: astra_model::RouteRequest = astra_ipc::receive(&mut stream).await.unwrap();
                assert_eq!((request.inbound, request.network, request.destination_ip), (Inbound::Tun, network, None));
                assert_eq!(request.target.host, "localhost");
                let action = engine.decision(&request).action;
                assert_eq!(action, RouteAction::Proxy("selected".into()));
                astra_ipc::send(&mut stream, &astra_model::RouteResponse {
                    api_version: astra_ipc::API_VERSION, id: request.id, revision: 1, action
                }).await.unwrap();
            }
        });
        let query = astra_dns::wire::query("localhost", hickory_proto::rr::RecordType::A).unwrap().to_vec().unwrap();
        let reply = hickory_proto::op::Message::from_vec(&app.dns_answer(&query).await.unwrap()).unwrap();
        let hickory_proto::rr::RData::A(fake) = reply.answers[0].data else { panic!("expected synthetic A record") };
        assert!(app.synthetic_ip(fake.0.into()));
        let source = "192.0.2.2:40000".parse().unwrap();

        let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dest = SocketAddr::new(fake.0.into(), echo.local_addr().unwrap().port());
        let (ingress, mut client) = tokio::io::duplex(128);
        let tcp = tokio::spawn(inbound::transparent_stream(ingress, app.clone(), Inbound::Tun, source, dest));
        client.write_all(b"tcp").await.unwrap();
        let (mut server, _) = echo.accept().await.unwrap();
        let mut bytes = [0; 3];
        server.read_exact(&mut bytes).await.unwrap();
        server.write_all(&bytes).await.unwrap();
        client.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"tcp");
        drop(server);
        drop(client);
        tcp.await.unwrap().unwrap();

        let echo = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let dest = SocketAddr::new(fake.0.into(),echo.local_addr().unwrap().port());
        let (send, input) = tokio::sync::mpsc::channel(4);
        let (output, mut replies) = tokio::sync::mpsc::channel(4);
        let current = Arc::new(arc_swap::ArcSwap::from(app));
        let udp = tokio::spawn(inbound::packet::dispatch(input,output,current,Inbound::Tun));
        for body in [b"one",b"two"] {
            send.send((body.to_vec(),source,dest)).await.unwrap();
            let (size,peer) = echo.recv_from(&mut bytes).await.unwrap();
            echo.send_to(&bytes[..size],peer).await.unwrap();
            assert_eq!(replies.recv().await.unwrap(),(body.to_vec(),dest,source));
        }
        rules.await.unwrap();
        assert_eq!(node_server.targets.lock().unwrap().len(),2,"one proxy session per flow");
        drop(send);
        udp.await.unwrap();
    }).await.unwrap();
}
