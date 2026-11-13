use super::*;
use astra_dns::wire;
use hickory_proto::{
    op::{Message, ResponseCode},
    rr::{rdata::A, RData, Record, RecordType},
};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, UdpSocket},
};

pub(super) fn app(settings: &str) -> App {
    App::from_text(
        &std::env::temp_dir().join("astra-dns-test.toml"),
        format!("[dns]\n{settings}\n"),
    )
    .unwrap()
}
fn response(raw: &[u8], truncated: bool) -> Vec<u8> {
    let query = Message::from_vec(raw).unwrap();
    let mut reply = wire::response(&query, ResponseCode::NoError);
    reply.metadata.truncation = truncated;
    if !truncated {
        reply.answers.push(Record::from_rdata(
            query.queries[0].name().clone(),
            30,
            RData::A(A("127.0.0.1".parse().unwrap())),
        ));
    }
    reply.to_vec().unwrap()
}

// Windows can reserve a UDP port even when TCP can bind the same number.
async fn dual_protocol_listeners() -> (TcpListener, UdpSocket) {
    for _ in 0..128 {
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        match UdpSocket::bind(tcp.local_addr().unwrap()).await {
            Ok(udp) => return (tcp, udp),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::AddrInUse | std::io::ErrorKind::PermissionDenied
                ) => {}
            Err(error) => panic!("bind test DNS UDP socket: {error}"),
        }
    }
    panic!("could not allocate a test port available for both TCP and UDP");
}

#[tokio::test]
async fn udp_upstream_retries_truncated_responses_over_tcp() {
    let (tcp, udp) = dual_protocol_listeners().await;
    let address = tcp.local_addr().unwrap();
    let worker = tokio::spawn(async move {
        let mut query = vec![0; 4096];
        let (size, peer) = udp.recv_from(&mut query).await.unwrap();
        udp.send_to(&response(&query[..size], true), peer)
            .await
            .unwrap();
        let (mut stream, _) = tcp.accept().await.unwrap();
        let size = stream.read_u16().await.unwrap() as usize;
        let mut query = vec![0; size];
        stream.read_exact(&mut query).await.unwrap();
        let reply = response(&query, false);
        stream.write_u16(reply.len() as u16).await.unwrap();
        stream.write_all(&reply).await.unwrap();
    });
    let app = app(&format!(
        "nameserver=['udp://{address}']\nplaintext_bypass=true"
    ));
    let query = wire::query("truncate.example", RecordType::A).unwrap();
    let reply = tokio::time::timeout(
        Duration::from_secs(3),
        app.dns_answer(&query.to_vec().unwrap()),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!Message::from_vec(&reply).unwrap().metadata.truncation);
    worker.await.unwrap();
}
#[tokio::test]
async fn local_udp_and_tcp_dns_listener_reload_keeps_fake_address_ownership() {
    let (probe, udp_probe) = dual_protocol_listeners().await;
    let address = probe.local_addr().unwrap();
    drop(probe);
    drop(udp_probe);
    let app = app("fake_ip=true");
    let current = Arc::new(arc_swap::ArcSwap::from_pointee(app));
    let guard = Prepared::bind(&[address], None)
        .await
        .unwrap()
        .start(current.clone());
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client.connect(address).await.unwrap();
    let query = wire::query("listener.example", RecordType::A)
        .unwrap()
        .to_vec()
        .unwrap();
    client.send(&query).await.unwrap();
    let mut reply = vec![0; 4096];
    let size = tokio::time::timeout(Duration::from_secs(2), client.recv(&mut reply))
        .await
        .unwrap()
        .unwrap();
    let reply = Message::from_vec(&reply[..size]).unwrap();
    let RData::A(ip) = reply.answers[0].data else {
        panic!()
    };
    let mut next = super::tests::app("fake_ip=true\ncache_capacity=128");
    next.dns
        .as_mut()
        .unwrap()
        .inherit(current.load().dns.as_ref().unwrap())
        .unwrap();
    current.store(Arc::new(next));
    let prepared = Prepared::bind(&[address], Some(&guard)).await.unwrap();
    drop(guard);
    let _guard = prepared.start(current.clone());
    let tcp = tokio::net::TcpStream::connect(address).await.unwrap();
    let reply = super::upstream::framed(tcp, &query).await.unwrap();
    let reply = Message::from_vec(&reply).unwrap();
    assert_eq!(reply.answers[0].data, RData::A(ip));
    assert_eq!(
        current
            .load()
            .dns
            .as_ref()
            .unwrap()
            .restore(ip.0.into())
            .unwrap()
            .as_deref(),
        Some("listener.example")
    );
}
#[tokio::test]
async fn tun_dns_interception_and_fake_udp_direct_egress_preserve_virtual_source() {
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let upstream_address = upstream.local_addr().unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let calls = count.clone();
    let worker = tokio::spawn(async move {
        let mut query = vec![0; 4096];
        loop {
            let (size, peer) = upstream.recv_from(&mut query).await.unwrap();
            calls.fetch_add(1, Ordering::SeqCst);
            upstream
                .send_to(&response(&query[..size], false), peer)
                .await
                .unwrap();
        }
    });
    let echo = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let echo_port = echo.local_addr().unwrap().port();
    let echo_task = tokio::spawn(async move {
        let mut bytes = vec![0; 1024];
        let (size, peer) = echo.recv_from(&mut bytes).await.unwrap();
        echo.send_to(&bytes[..size], peer).await.unwrap();
    });
    // Use a manual DIRECT group to avoid requiring a separate rule process.
    let source=format!("standalone=true\nstandalone_outbound='direct-group'\nsocks5_listen='127.0.0.1:1080'\n[proxy_groups.direct-group]\nmembers=['DIRECT']\n[dns]\nlisten=[]\nfake_ip=true\nnameserver=['udp://{upstream_address}']\nplaintext_bypass=true\n");
    let source = format!("max_udp_flows=1\nudp_targets_per_association=1\n{source}");
    let app = App::from_text(&std::env::temp_dir().join("astra-dns-udp.toml"), source).unwrap();
    let current = Arc::new(arc_swap::ArcSwap::from_pointee(app));
    let (send, receive) = tokio::sync::mpsc::channel(4);
    let (output, mut replies) = tokio::sync::mpsc::channel(4);
    let dispatcher = tokio::spawn(crate::packet::dispatch(
        receive,
        output,
        current.clone(),
        astra_model::Inbound::Tun,
    ));
    let source = "192.0.2.2:40000".parse().unwrap();
    let dns_dest = "8.8.8.8:53".parse().unwrap();
    let query = wire::query("udp-echo.example", RecordType::A)
        .unwrap()
        .to_vec()
        .unwrap();
    send.send((query, source, dns_dest)).await.unwrap();
    let (reply, from, to) = tokio::time::timeout(Duration::from_secs(2), replies.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(from, dns_dest);
    assert_eq!(to, source);
    let reply = Message::from_vec(&reply).unwrap();
    let RData::A(ip) = reply.answers[0].data else {
        panic!()
    };
    let fake = std::net::SocketAddr::new(ip.0.into(), echo_port);
    send.send((b"fake-udp-echo".to_vec(), source, fake))
        .await
        .unwrap();
    let (reply, from, to) = tokio::time::timeout(Duration::from_secs(3), replies.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reply, b"fake-udp-echo");
    assert_eq!(from, fake);
    assert_eq!(to, source);
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "direct fake egress must resolve real IP without querying its own fake listener"
    );
    echo_task.await.unwrap();
    // Keep the UDP echo flow alive and its sole permit occupied. DNS still
    // works and uses the new configuration on the same client tuple.
    let text = current
        .load()
        .source_text
        .replace("fake_ip=true", "fake_ip=true\nfake_ip_filter=['+.example']");
    let mut next = App::from_text(&std::env::temp_dir().join("astra-dns-udp.toml"), text).unwrap();
    next.dns
        .as_mut()
        .unwrap()
        .inherit(current.load().dns.as_ref().unwrap())
        .unwrap();
    let previous = current.load_full();
    current.store(Arc::new(next));
    previous.egress_signal.send_replace(true);
    tokio::time::timeout(Duration::from_secs(2), previous.udp_permits.idle())
        .await
        .unwrap();
    let query = wire::query("udp-echo.example", RecordType::A)
        .unwrap()
        .to_vec()
        .unwrap();
    send.send((query, source, dns_dest)).await.unwrap();
    let (reply, _, _) = tokio::time::timeout(Duration::from_secs(2), replies.recv())
        .await
        .unwrap()
        .unwrap();
    let reply = Message::from_vec(&reply).unwrap();
    assert_eq!(
        reply.answers[0].data,
        RData::A(A("127.0.0.1".parse().unwrap()))
    );
    dispatcher.abort();
    worker.abort();
}

use std::net::SocketAddr;
fn core(extra: &str) -> App {
    App::from_text(
        &std::env::temp_dir().join("astra-dns-lookup-test.toml"),
        format!("rule_socket='unused'\nsocks5_listen='127.0.0.1:1080'\n{extra}"),
    )
    .unwrap()
}
/// Answers every UDP query with 127.0.0.1, counting queries.
async fn udp_mock() -> (SocketAddr, Arc<AtomicUsize>) {
    let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let address = udp.local_addr().unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let seen = count.clone();
    tokio::spawn(async move {
        let mut query = vec![0; 4096];
        while let Ok((size, peer)) = udp.recv_from(&mut query).await {
            seen.fetch_add(1, Ordering::SeqCst);
            let _ = udp.send_to(&response(&query[..size], false), peer).await;
        }
    });
    (address, count)
}

#[test]
fn local_lookups_use_an_encrypted_resolver_unless_plaintext_is_bypassed() {
    // No [dns]: lookups still get the default encrypted resolver.
    let app = core("");
    assert!(app.dns.is_none());
    let resolver = app.resolver().expect("lookup-only resolver");
    assert!(resolver
        .config
        .nameserver
        .iter()
        .all(|raw| astra_dns::Upstream::parse(raw).unwrap().is_encrypted()));
    assert!(!resolver.config.fake_ip && resolver.config.listen.is_empty());
    // Disabled service: lookups keep the configured encrypted upstreams.
    let app = core("[dns]\nenable=false\nnameserver=['tls://9.9.9.9']\n");
    assert!(app.dns.is_none());
    assert_eq!(app.resolver().unwrap().config.nameserver, ["tls://9.9.9.9"]);
    // Explicit bypass with a disabled service: the system resolver is allowed.
    let app = core("[dns]\nenable=false\nplaintext_bypass=true\n");
    assert!(app.resolver().is_none());
    // An enabled service answers lookups itself.
    let app = app_with_service();
    assert!(app.lookup_dns.is_none() && app.resolver().is_some());
}

#[tokio::test]
async fn proxy_server_dns_is_direct_with_unavailable_proxy_and_disabled_service() {
    use meow_common::HostResolver;
    let (direct_dns, queries) = udp_mock().await;
    for enabled in [true, false] {
        let source = format!("standalone=true\nstandalone_outbound='main'\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\nkind='socks5'\ntag='node'\nserver='Node.Direct-Dns.Example:9'\n[proxy_groups.main]\nmembers=['node']\n[dns]\nenable={enabled}\nlisten=[]\nplaintext_bypass=true\nrespect_rules=true\nquery_timeout_ms=100\nnameserver=['tcp://127.0.0.1:9']\nproxy_server_nameserver=['udp://{direct_dns}']\nfake_ip=true\n[dns.nameserver_policy]\n'node.direct-dns.example'=['tcp://127.0.0.1:9']\n");
        let app = Arc::new(
            App::from_text(&std::env::temp_dir().join("direct-dns-test.toml"), source).unwrap(),
        );
        let hook = HostHook(Box::new({
            let app = app.clone();
            move || app.clone()
        }));
        assert_eq!(
            hook.resolve_all("node.direct-dns.example").await.unwrap(),
            ["127.0.0.1".parse::<IpAddr>().unwrap()]
        );
        assert!(hook
            .resolve_all_nested("nested.direct-dns.example")
            .await
            .unwrap()
            .is_ok());
        assert_eq!(
            app.lookup_real("node.direct-dns.example", 443)
                .await
                .unwrap()[0]
                .port(),
            443
        );
    }
    assert_eq!(
        queries.load(Ordering::SeqCst),
        4,
        "each core has two direct lookups, and a cache hit"
    );
}

#[tokio::test]
async fn direct_dns_list_also_bypasses_rules_for_regular_queries_and_bootstrap() {
    let (direct_dns, queries) = udp_mock().await;
    let app = core(&format!("[dns]\nlisten=[]\nplaintext_bypass=true\nrespect_rules=true\nnameserver=['udp://dns-bootstrap.example:{}']\nproxy_server_nameserver=['udp://{direct_dns}']\n", direct_dns.port()));
    // No rule core exists. Routing either bootstrap or this listed DNS through
    // rules would fail; the configured direct server must remain reachable.
    let query = wire::query("business.example", RecordType::A)
        .unwrap()
        .to_vec()
        .unwrap();
    let listed = astra_dns::Upstream::parse(&format!("udp://{direct_dns}")).unwrap();
    upstream::exchange(&app, &listed, query.clone())
        .await
        .unwrap();
    // Hostname bootstrap must go to the dedicated literal server, even though
    // the normal upstream itself cannot be reached through the missing proxy.
    let _ = app.dns_answer(&query).await;
    assert_eq!(queries.load(Ordering::SeqCst), 2);
    assert_eq!(
        app.resolver()
            .unwrap()
            .bootstrap_cached("dns-bootstrap.example"),
        Some("127.0.0.1".parse().unwrap())
    );
}

#[tokio::test]
async fn empty_proxy_dns_list_preserves_normal_rule_routing() {
    let (upstream, queries) = udp_mock().await;
    let app = core(&format!("[[outbounds]]\nkind='socks5'\ntag='node'\nserver='node.empty-direct-dns.example:9'\n[dns]\nlisten=[]\nplaintext_bypass=true\nrespect_rules=true\nnameserver=['udp://{upstream}']\n"));
    assert!(
        app.lookup_real("node.empty-direct-dns.example", 443)
            .await
            .is_err(),
        "no rule core exists; an empty direct list must not bypass route selection"
    );
    assert_eq!(queries.load(Ordering::SeqCst), 0);
}
fn app_with_service() -> App {
    app("")
}

#[tokio::test]
async fn lookups_short_circuit_literals_and_localhost_and_honour_bypass_policy() {
    let app = core("");
    for (host, expected) in [
        ("192.0.2.7", vec!["192.0.2.7:80"]),
        ("[2001:db8::1]", vec!["[2001:db8::1]:80"]),
        ("localhost", vec!["127.0.0.1:80", "[::1]:80"]),
        ("api.LOCALHOST.", vec!["127.0.0.1:80", "[::1]:80"]),
    ] {
        let found = app.lookup_real(host, 80).await.unwrap();
        let expected: Vec<SocketAddr> = expected.iter().map(|a| a.parse().unwrap()).collect();
        assert_eq!(found, expected, "{host}");
    }
    let (upstream, queries) = udp_mock().await;
    let app = core(&format!(
        "[dns]\nenable=false\nplaintext_bypass_domains=['+.corp']\n[dns.nameserver_policy]\n'+.corp'=['udp://{upstream}']\n"
    ));
    let found = tokio::time::timeout(Duration::from_secs(3), app.lookup_real("git.corp", 22))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found, ["127.0.0.1:22".parse::<SocketAddr>().unwrap()]);
    assert_eq!(
        queries.load(Ordering::SeqCst),
        1,
        "only the A query is needed"
    );
}

#[tokio::test]
async fn protocol_egress_resolves_node_hostnames_through_the_host_hook() {
    use meow_common::HostResolver;
    let (upstream, queries) = udp_mock().await;
    let app = Arc::new(core(&format!(
        "[dns]\nenable=false\nplaintext_bypass_domains=['+.corp']\n[dns.nameserver_policy]\n'+.corp'=['udp://{upstream}']\n"
    )));
    let hook = crate::dns::HostHook(Box::new({
        let app = app.clone();
        move || app.clone()
    }));
    assert_eq!(
        hook.resolve_all("node.corp").await.unwrap(),
        ["127.0.0.1".parse::<IpAddr>().unwrap()]
    );
    assert!(hook.resolve_all_nested("node.corp").await.unwrap().is_ok());
    let bypass = Arc::new(core("[dns]\nenable=false\nplaintext_bypass=true\n"));
    let open = crate::dns::HostHook(Box::new(move || bypass.clone()));
    assert!(
        open.resolve_all_nested("node.corp").await.is_none(),
        "without an encrypted resolver nested lookups keep the legacy fallback"
    );

    // End to end through the vendored hook. The installed hook is
    // process-global, so it only claims `.corp` names; everything else keeps
    // the system path other concurrently running tests expect.
    struct Scoped(crate::dns::HostHook);
    #[async_trait::async_trait]
    impl HostResolver for Scoped {
        async fn resolve(&self, host: &str) -> std::io::Result<IpAddr> {
            Ok(self.resolve_all(host).await?[0])
        }
        async fn resolve_all(&self, host: &str) -> std::io::Result<Vec<IpAddr>> {
            if host.ends_with(".corp") {
                return self.0.resolve_all(host).await;
            }
            Ok(tokio::net::lookup_host((host, 0))
                .await?
                .map(|a| a.ip())
                .collect())
        }
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    meow_common::set_host_resolver(Arc::new(Scoped(hook)));
    let dial = astra_protocols::egress::connect(("proxy-node.corp", port)).await;
    let text = format!("proxy-node.corp:{port}");
    let dial_text = astra_protocols::egress::connect(text.as_str()).await;
    meow_common::clear_host_resolver();
    dial.unwrap();
    dial_text.unwrap();
    listener.accept().await.unwrap();
    // node.corp + proxy-node.corp; the second dial is a cache hit.
    assert_eq!(queries.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn ipv6_tun_takeover_adds_split_routes_and_reserves_the_tun_dns_address() {
    let text = |extra: &str| {
        format!("rule_socket='unused'\n[tun]\ninterface='en0'\nipv6_address='fdfe:dcba:9876::1'\n{extra}[dns]\nlisten=[]\nfake_ip=true\n")
    };
    let app = core_text(&text(""));
    let routes = &app.config.tun.as_ref().unwrap().routes;
    assert!(routes.contains(&"::/1".to_owned()) && routes.contains(&"8000::/1".to_owned()));
    // An explicit IPv6 route list is left alone.
    let app = core_text(&text("routes=['0.0.0.0/1','128.0.0.0/1','2000::/3']\n"));
    let routes = &app.config.tun.as_ref().unwrap().routes;
    assert!(routes.contains(&"2000::/3".to_owned()) && !routes.contains(&"::/1".to_owned()));
    // Opting out of both takeover parts keeps the IPv4-only default.
    let off = "rule_socket='unused'\n[tun]\ninterface='en0'\nipv6_address='fdfe:dcba:9876::1'\n[dns]\nlisten=[]\nsystem_takeover=false\nroute_system_dns=false\n";
    assert!(!core_text(off)
        .config
        .tun
        .as_ref()
        .unwrap()
        .routes
        .iter()
        .any(|r| r.contains(':')));

    // 198.18.0.0-.3 are TUN network/address/DNS/broadcast; Fake IP starts after.
    let query = wire::query("first.example", RecordType::A).unwrap();
    let reply = Message::from_vec(
        &core_text(&text(""))
            .dns_answer(&query.to_vec().unwrap())
            .await
            .unwrap(),
    )
    .unwrap();
    match &reply.answers[0].data {
        RData::A(ip) => assert_eq!(ip.0, std::net::Ipv4Addr::new(198, 18, 0, 4)),
        other => panic!("{other:?}"),
    }
}
fn core_text(text: &str) -> App {
    App::from_text(
        &std::env::temp_dir().join("astra-dns-takeover-test.toml"),
        text.to_owned(),
    )
    .unwrap()
}
