use super::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[tokio::test]
async fn poisoned_selection_discards_partial_route_and_cancels_old_sessions() {
    let app = App::from_text(std::path::Path::new("poisoned-groups.toml"),
            "standalone=true\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\nkind='socks5'\ntag='node'\nserver='127.0.0.1:1234'\n[proxy_groups.main]\nmode='url-test'\nmembers=['node']\n".into()).unwrap();
    let old = app.groups.sessions.lease(&["main".into()]);
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut selections = app.groups.selections.write().unwrap();
        selections.insert(
            "main".into(),
            Selection {
                member: Some("DIRECT".into()),
                ..Default::default()
            },
        );
        panic!("injected partial selection update");
    }));
    assert!(panic.is_err());
    assert!(app.groups.cached(&app, "main").is_err());
    assert!(app.groups.read_selections().is_empty());
    assert!(!app.groups.selections.is_poisoned());
    tokio::time::timeout(Duration::from_secs(1), old.cancelled())
        .await
        .unwrap();
    let fresh = app.groups.sessions.lease(&["main".into()]);
    assert!(
        tokio::time::timeout(Duration::from_millis(10), fresh.cancelled())
            .await
            .is_err()
    );
}
#[tokio::test]
async fn poisoned_group_test_slot_is_aborted_without_a_second_panic() {
    let app = App::from_text(std::path::Path::new("test-slot.toml"),
            "standalone=true\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\nkind='socks5'\ntag='node'\nserver='127.0.0.1:1234'\n".into()).unwrap();
    let task = tokio::spawn(std::future::pending::<()>());
    let aborted = task.abort_handle();
    *app.groups.test_slot() = Some(task);
    app.groups.testing.store(true, Ordering::Release);
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = app.groups.test_task.lock().unwrap();
        panic!("injected task slot panic");
    }))
    .is_err());
    app.groups.stop_test();
    tokio::task::yield_now().await;
    assert!(aborted.is_finished());
    assert!(!app.groups.testing.load(Ordering::Acquire));
    assert!(app.groups.test_slot().is_none());
}
#[tokio::test]
async fn cancelled_old_test_cannot_clear_replacement_running_flag() {
    let app = Arc::new(App::from_text(std::path::Path::new("test-lifetime.toml"),
            "standalone=true\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\nkind='socks5'\ntag='node'\nserver='127.0.0.1:1234'\n".into()).unwrap());
    let spawn = |app: Arc<App>| {
        let (started, ready) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _testing = Testing(&app.groups, tokio::task::id());
            let _ = started.send(());
            std::future::pending::<()>().await;
        });
        (task, ready)
    };
    let (old, ready) = spawn(app.clone());
    let old_abort = old.abort_handle();
    *app.groups.test_slot() = Some(old);
    app.groups.testing.store(true, Ordering::Release);
    ready.await.unwrap();
    let (next, ready) = spawn(app.clone());
    let next_abort = next.abort_handle();
    *app.groups.test_slot() = Some(next);
    ready.await.unwrap();
    old_abort.abort();
    tokio::time::timeout(Duration::from_secs(1), async {
        while !old_abort.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(app.groups.testing.load(Ordering::Acquire));
    next_abort.abort();
    tokio::time::timeout(Duration::from_secs(1), async {
        while !next_abort.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!app.groups.testing.load(Ordering::Acquire));
}
#[test]
fn group_bindings_round_trip_and_reject_empty_names() {
    let group: astra_model::groups::Group =
        toml::from_str("members=['DIRECT']\ninterface='虚拟网卡 VPN'\n").unwrap();
    assert_eq!(group.interface.as_deref(), Some("虚拟网卡 VPN"));
    let encoded = toml::to_string(&group).unwrap();
    assert_eq!(
        toml::from_str::<astra_model::groups::Group>(&encoded).unwrap(),
        group
    );
    for interface in ["", " ", "bad\nname"] {
        let mut groups = std::collections::BTreeMap::new();
        groups.insert(
            "one".into(),
            astra_model::groups::Group {
                interface: Some(interface.into()),
                members: vec!["DIRECT".into()],
                ..Default::default()
            },
        );
        assert!(astra_model::groups::validate(&groups, &HashSet::new()).is_err());
    }
}

#[tokio::test]
async fn nested_groups_pin_the_outer_interface_and_isolate_pools() {
    let text = "standalone=true\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\nkind='socks5'\ntag='node'\nserver='127.0.0.1:1234'\n[proxy_groups.outer]\nmembers=['child']\ninterface='Ethernet'\n[proxy_groups.child]\nmembers=['node']\ninterface='虚拟网卡 VPN'\n[proxy_groups.inherit]\nmembers=['child']\n";
    let app = App::from_text(std::path::Path::new("group-bindings.toml"), text.into()).unwrap();
    for (name, expected) in [
        ("outer", "Ethernet"),
        ("child", "虚拟网卡 VPN"),
        ("inherit", "虚拟网卡 VPN"),
    ] {
        let (action, lease) = app
            .resolve_session(&RouteAction::Proxy(name.into()))
            .await
            .unwrap();
        assert_eq!(action, RouteAction::Proxy("node".into()));
        assert_eq!(lease.1.as_deref(), Some(expected));
    }
    let adapter = |name| match app.native_for(Some(name)).get("node").unwrap() {
        crate::native::NativeOutbound::Adapter(adapter) => adapter.clone(),
        _ => panic!("expected SOCKS adapter"),
    };
    assert!(!Arc::ptr_eq(&adapter("Ethernet"), &adapter("虚拟网卡 VPN")));
    assert!(Arc::ptr_eq(&adapter("Ethernet"), &adapter("Ethernet")));
    let (_, lease) = app
        .resolve_session(&RouteAction::Proxy("node".into()))
        .await
        .unwrap();
    assert!(lease.1.is_none());
}

#[tokio::test]
async fn direct_and_block_route_through_their_builtin_groups() {
    let defaults = App::from_text(
            std::path::Path::new("builtin-groups.toml"),
            "standalone=true\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\nkind='socks5'\ntag='node'\nserver='127.0.0.1:1234'\n".into(),
        )
        .unwrap();
    for (action, expected) in [
        (RouteAction::Direct, RouteAction::Direct),
        (RouteAction::Block, RouteAction::Block),
        (RouteAction::Proxy("DIRECT".into()), RouteAction::Direct),
    ] {
        let (resolved, _) = defaults.resolve_session(&action).await.unwrap();
        assert_eq!(resolved, expected);
    }
    // Rerouting the DIRECT group moves every direct decision, including other groups' DIRECT members.
    let text = "standalone=true\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\nkind='socks5'\ntag='node'\nserver='127.0.0.1:1234'\n[proxy_groups.DIRECT]\nmembers=['DIRECT','node']\nselected='node'\ninterface='Ethernet'\n[proxy_groups.main]\nmembers=['DIRECT']\n";
    let app = App::from_text(std::path::Path::new("builtin-groups.toml"), text.into()).unwrap();
    for action in [RouteAction::Direct, RouteAction::Proxy("main".into())] {
        let (resolved, lease) = app.resolve_session(&action).await.unwrap();
        assert_eq!(resolved, RouteAction::Proxy("node".into()));
        assert_eq!(lease.1.as_deref(), Some("Ethernet"));
    }
    assert_eq!(
        app.resolve_action(&RouteAction::Block).await.unwrap(),
        RouteAction::Block
    );
    assert!(!crate::diagnostics::targets(&app)
        .iter()
        .any(|tag| tag == "REJECT"));
}

#[tokio::test]
async fn missing_group_adapter_fails_closed_for_tcp_and_udp() {
    let text = "standalone=true\nstandalone_outbound='bound'\nsocks5_listen='127.0.0.1:1080'\n[proxy_groups.bound]\nmembers=['DIRECT']\ninterface='no-such-iface-zz9'\n";
    let app = App::from_text(std::path::Path::new("group-bindings.toml"), text.into()).unwrap();
    let action = RouteAction::Proxy("bound".into());
    let target = Target {
        host: "127.0.0.1".into(),
        port: 1234,
    };
    let error = app.connect(&target, &action).await.err().unwrap();
    assert!(error.to_string().contains("does not exist"), "{error:#}");
    let error = crate::udp::open_action(
        &app,
        &target,
        astra_model::Inbound::Socks5,
        "127.0.0.1:12345".parse().unwrap(),
        action,
    )
    .await
    .err()
    .unwrap();
    assert!(error.to_string().contains("does not exist"), "{error:#}");
}

#[test]
fn test_urls_reject_invalid_targets_and_allow_ipv6() {
    for url in [
        "ftp://example.test/",
        "relative/path",
        "http://user:pass@example.test/",
        "http://example.test/#fragment",
        "http://example.test:0/",
        "http://example.test:65536/",
    ] {
        assert!(test_uri(url).is_err(), "{url}");
    }
    for url in [
        "http://example.test/204",
        "https://example.test/",
        "http://[::1]:8080/204",
    ] {
        assert!(test_uri(url).is_ok(), "{url}");
    }
}
struct Stub {
    address: std::net::SocketAddr,
    delay: Arc<AtomicU64>,
    healthy: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Stub {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn headers(stream: &mut tokio::net::TcpStream) -> Result<String> {
    let mut text = Vec::new();
    while !text.ends_with(b"\r\n\r\n") {
        ensure!(text.len() < 8192, "headers too large");
        text.push(stream.read_u8().await?);
    }
    Ok(String::from_utf8(text)?)
}
async fn stub(delay: u64, marker: &'static str) -> Stub {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let delay = Arc::new(AtomicU64::new(delay));
    let healthy = Arc::new(AtomicBool::new(true));
    let (d, h) = (delay.clone(), healthy.clone());
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let (d, h) = (d.clone(), h.clone());
            tokio::spawn(async move {
                let request = headers(&mut socket).await?;
                ensure!(
                    request.starts_with("CONNECT example.test:80 "),
                    "wrong test target"
                );
                socket
                    .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                    .await?;
                let request = headers(&mut socket).await?;
                ensure!(request.starts_with("GET /204 "), "wrong test path");
                tokio::time::sleep(Duration::from_millis(d.load(Ordering::SeqCst))).await;
                let status = if h.load(Ordering::SeqCst) { 200 } else { 503 };
                socket.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{marker}",marker.len()).as_bytes()).await?;
                Ok::<_, anyhow::Error>(())
            });
        }
    });
    Stub {
        address,
        delay,
        healthy,
        task,
    }
}
fn expire(app: &App) {
    for selection in app.groups.selections.write().unwrap().values_mut() {
        selection.checked = None;
    }
}
fn app(a: &Stub, b: &Stub, mode: &str, selected: &str, tolerance: u64) -> App {
    App::from_text(std::path::Path::new("groups.toml"), format!("standalone=true\nstandalone_outbound='main'\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\nkind='http'\ntag='a'\nserver='{}'\n[[outbounds]]\nkind='http'\ntag='b'\nserver='{}'\n[proxy_groups.speed]\nmode='{mode}'\nmembers=['a','b']\nselected='{selected}'\nurl='http://example.test/204'\ninterval=1\ntimeout_ms=1000\ntolerance_ms={tolerance}\n[proxy_groups.main]\nmode='select'\nmembers=['speed','DIRECT']\nselected='speed'\n",a.address,b.address)).unwrap()
}
#[tokio::test]
async fn fallback_uses_priority_and_load_balance_rotates_only_healthy_members() {
    let a = stub(100, "a").await;
    let b = stub(1, "b").await;
    let action = RouteAction::Proxy("main".into());
    let fallback = app(&a, &b, "fallback", "a", 0);
    assert_eq!(
        fallback.resolve_session(&action).await.unwrap().0,
        RouteAction::Proxy("a".into())
    );
    a.healthy.store(false, Ordering::SeqCst);
    expire(&fallback);
    assert_eq!(
        fallback.resolve_session(&action).await.unwrap().0,
        RouteAction::Proxy("b".into())
    );
    a.healthy.store(true, Ordering::SeqCst);
    expire(&fallback);
    assert_eq!(
        fallback.resolve_session(&action).await.unwrap().0,
        RouteAction::Proxy("a".into())
    );
    let balanced = app(&a, &b, "load_balance", "a", 0);
    for name in ["a", "b", "a", "b"] {
        assert_eq!(
            balanced.resolve_session(&action).await.unwrap().0,
            RouteAction::Proxy(name.into())
        );
    }
    a.healthy.store(false, Ordering::SeqCst);
    expire(&balanced);
    for _ in 0..3 {
        assert_eq!(
            balanced.resolve_session(&action).await.unwrap().0,
            RouteAction::Proxy("b".into())
        );
    }
    b.healthy.store(false, Ordering::SeqCst);
    expire(&balanced);
    assert!(balanced.resolve_session(&action).await.is_err());
}

#[tokio::test]
async fn parent_pins_descendant_without_changing_shared_child_selection() {
    let a = stub(1, "a").await;
    let b = stub(1, "b").await;
    let original = app(&a, &b, "select", "b", 0);
    let text = original
        .source_text
        .replace("selected='speed'", "selected='a'");
    let app = App::from_text(std::path::Path::new("groups.toml"), text).unwrap();
    let parent = RouteAction::Proxy("main".into());
    let child = RouteAction::Proxy("speed".into());
    assert_eq!(
        app.groups.resolve(&app, &parent).await.unwrap(),
        RouteAction::Proxy("a".into())
    );
    assert_eq!(
        app.groups.resolve(&app, &child).await.unwrap(),
        RouteAction::Proxy("b".into())
    );
    let (resolved, path) = app.groups.cached_path(&app, "main", Vec::new()).unwrap();
    assert_eq!(resolved, RouteAction::Proxy("a".into()));
    assert_eq!(path, ["main", "speed"]);
    assert_eq!(
        app.groups.resolve_session(&app, &parent).await.unwrap().0,
        resolved
    );
    let invalid = app
        .source_text
        .replace("selected='a'", "selected='missing'");
    assert!(App::from_text(std::path::Path::new("groups.toml"), invalid).is_err());
}
#[tokio::test]
async fn automatic_groups_measure_switch_retain_tolerance_and_fail_closed() {
    let a = stub(20, "a").await;
    let b = stub(160, "b").await;
    let app = app(&a, &b, "auto", "a", 0);
    let action = RouteAction::Proxy("main".into());
    assert_eq!(
        app.groups.resolve(&app, &action).await.unwrap(),
        RouteAction::Proxy("a".into())
    );
    {
        let _checking = app.groups.locks["speed"].lock().await;
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(50), app.resolve_action(&action))
                .await
                .unwrap()
                .unwrap(),
            RouteAction::Proxy("a".into())
        );
    }
    a.healthy.store(false, Ordering::SeqCst);
    expire(&app);
    assert_eq!(
        app.groups.resolve(&app, &action).await.unwrap(),
        RouteAction::Proxy("b".into())
    );
    b.healthy.store(false, Ordering::SeqCst);
    expire(&app);
    assert!(app.groups.resolve(&app, &action).await.is_err());
    a.healthy.store(true, Ordering::SeqCst);
    expire(&app);
    assert_eq!(
        app.groups.resolve(&app, &action).await.unwrap(),
        RouteAction::Proxy("a".into())
    );
    // Small changes retain a measured healthy member, even if a different one is faster.
    b.healthy.store(true, Ordering::SeqCst);
    let app = self::app(&a, &b, "auto", "a", 500);
    app.groups.resolve(&app, &action).await.unwrap();
    a.delay.store(180, Ordering::SeqCst);
    b.delay.store(10, Ordering::SeqCst);
    expire(&app);
    assert_eq!(
        app.groups.resolve(&app, &action).await.unwrap(),
        RouteAction::Proxy("a".into())
    );
}
#[tokio::test]
async fn switch_reasons_obey_independent_disconnect_policies() {
    let a = stub(10, "a").await;
    let b = stub(100, "b").await;
    let action = RouteAction::Proxy("main".into());
    let mut app = self::app(&a, &b, "auto", "a", 0);
    app.config
        .proxy_groups
        .get_mut("speed")
        .unwrap()
        .disconnect_on_failure = true;
    let (_, lease) = app.resolve_session(&action).await.unwrap();
    // A healthy improvement keeps the old lease because its policy is false.
    a.delay.store(100, Ordering::SeqCst);
    b.delay.store(10, Ordering::SeqCst);
    expire(&app);
    app.resolve_action(&action).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(10), lease.cancelled())
            .await
            .is_err()
    );
    // Failure closes every older connection through this group, even those kept at earlier switches.
    b.healthy.store(false, Ordering::SeqCst);
    expire(&app);
    app.resolve_action(&action).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), lease.cancelled())
        .await
        .unwrap();
    let (_, lease) = app.resolve_session(&action).await.unwrap();
    a.healthy.store(false, Ordering::SeqCst);
    expire(&app);
    assert!(app.resolve_action(&action).await.is_err());
    tokio::time::timeout(Duration::from_secs(1), lease.cancelled())
        .await
        .unwrap();
    // Opt in only to recovery: preserve a lease during outage, close on recovery.
    a.healthy.store(true, Ordering::SeqCst);
    let mut app = self::app(&a, &b, "auto", "a", 0);
    app.config
        .proxy_groups
        .get_mut("speed")
        .unwrap()
        .disconnect_on_recovery = true;
    let (_, lease) = app.resolve_session(&action).await.unwrap();
    a.healthy.store(false, Ordering::SeqCst);
    expire(&app);
    assert!(app.resolve_action(&action).await.is_err());
    assert!(
        tokio::time::timeout(Duration::from_millis(10), lease.cancelled())
            .await
            .is_err()
    );
    a.healthy.store(true, Ordering::SeqCst);
    expire(&app);
    app.resolve_action(&action).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), lease.cancelled())
        .await
        .unwrap();
    // Independently opt in to healthy automatic improvements.
    b.healthy.store(true, Ordering::SeqCst);
    a.delay.store(10, Ordering::SeqCst);
    b.delay.store(100, Ordering::SeqCst);
    let mut app = self::app(&a, &b, "auto", "a", 0);
    app.config
        .proxy_groups
        .get_mut("speed")
        .unwrap()
        .disconnect_on_auto = true;
    let (_, lease) = app.resolve_session(&action).await.unwrap();
    a.delay.store(100, Ordering::SeqCst);
    b.delay.store(10, Ordering::SeqCst);
    expire(&app);
    app.resolve_action(&action).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), lease.cancelled())
        .await
        .unwrap();
}
#[tokio::test]
async fn reload_cancels_only_configured_reasons_and_preserves_unrelated_connections() {
    let a = stub(0, "a").await;
    let b = stub(0, "b").await;
    let old = self::app(&a, &b, "select", "a", 50);
    let action = RouteAction::Proxy("main".into());
    let (_, lease) = old.resolve_session(&action).await.unwrap();
    let (_, unrelated) = old
        .resolve_session(&RouteAction::Proxy("b".into()))
        .await
        .unwrap();
    let mut next = self::app(&a, &b, "select", "b", 50);
    next.config
        .proxy_groups
        .get_mut("speed")
        .unwrap()
        .disconnect_on_manual = true;
    next.groups.inherit(&old, &next.config);
    next.groups.committed(&old, &next.config);
    tokio::time::timeout(Duration::from_secs(1), lease.cancelled())
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(10), unrelated.cancelled())
            .await
            .is_err()
    );
    let (_, lease) = next.resolve_session(&action).await.unwrap();
    let mut last = self::app(&a, &b, "select", "b", 50);
    last.config
        .proxy_groups
        .get_mut("speed")
        .unwrap()
        .disconnect_on_config_change = true;
    last.config.proxy_groups.get_mut("speed").unwrap().members = vec!["b".into()];
    last.groups.inherit(&next, &last.config);
    last.groups.committed(&next, &last.config);
    tokio::time::timeout(Duration::from_secs(1), lease.cancelled())
        .await
        .unwrap();
}
#[tokio::test]
async fn default_dynamic_group_reloads_members_and_never_falls_back_to_direct_when_empty() {
    let settings = |tag: &str| {
        format!("standalone=true\nstandalone_outbound='PROXY'\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\nkind='socks5'\ntag='{tag}'\nserver='127.0.0.1:1081'")
    };
    let old = App::from_text(std::path::Path::new("dynamic.toml"), settings("a")).unwrap();
    let action = RouteAction::Proxy("PROXY".into());
    assert_eq!(
        old.resolve_action(&action).await.unwrap(),
        RouteAction::Proxy("a".into())
    );
    assert!(!old.source_text.contains("proxy_groups"));
    let mut next = App::from_text(std::path::Path::new("dynamic.toml"), settings("b")).unwrap();
    let (_, lease) = old.resolve_session(&action).await.unwrap();
    next.config
        .proxy_groups
        .get_mut("PROXY")
        .unwrap()
        .disconnect_on_config_change = true;
    next.groups.inherit(&old, &next.config);
    next.groups.committed(&old, &next.config);
    assert_eq!(
        next.resolve_action(&action).await.unwrap(),
        RouteAction::Proxy("b".into())
    );
    tokio::time::timeout(Duration::from_secs(1), lease.cancelled())
        .await
        .unwrap();
    let empty = App::from_text(
        std::path::Path::new("dynamic.toml"),
        "standalone=true\nstandalone_outbound='PROXY'\nsocks5_listen='127.0.0.1:1080'".into(),
    )
    .unwrap();
    assert!(empty.resolve_action(&action).await.is_err());
}
#[tokio::test]
async fn manual_group_connects_selected_member_and_existing_stream_survives_new_choice() {
    let a = stub(0, "a").await;
    let b = stub(0, "b").await;
    let app = app(&a, &b, "select", "b", 50);
    let action = RouteAction::Proxy("main".into());
    let target = Target {
        host: "example.test".into(),
        port: 80,
    };
    let mut connection = app.connect(&target, &action).await.unwrap();
    assert!(
        app.groups.selections.read().unwrap().is_empty(),
        "manual selection must not start probes"
    );
    let next = self::app(&a, &b, "select", "a", 50);
    assert_eq!(
        next.groups.resolve(&next, &action).await.unwrap(),
        RouteAction::Proxy("a".into())
    );
    connection
        .write_all(b"GET /204 HTTP/1.1\r\nHost: example.test\r\n\r\n")
        .await
        .unwrap();
    let mut bytes = vec![];
    connection.read_to_end(&mut bytes).await.unwrap();
    assert!(bytes.ends_with(b"b"));
}
