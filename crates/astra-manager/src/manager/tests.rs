use super::*;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicU64, Ordering};
static NEXT: AtomicU64 = AtomicU64::new(0);
fn test_binaries() -> PathBuf {
    std::env::var_os("ASTRA_UPDATE_TEST_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_exe()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .to_owned()
        })
}
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "at-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[cfg(windows)]
#[tokio::test]
async fn control_endpoint_recovery_preserves_live_and_unknown_owners() {
    let temp = Temp::new();
    let path = temp.0.join("t2.sock");
    prepare_control_endpoint(&path).await.unwrap();
    fs::write(&path, b"").unwrap();
    prepare_control_endpoint(&path).await.unwrap();
    assert!(!path.exists());
    fs::write(&path, b"not a marker").unwrap();
    assert!(prepare_control_endpoint(&path).await.is_err());
    assert_eq!(fs::read(&path).unwrap(), b"not a marker");
    fs::remove_file(&path).unwrap();
    let control = control::Control::bind(Some(path.clone())).unwrap();
    let server = tokio::spawn(async move { control.shutdown().await });
    let error = prepare_control_endpoint(&path).await.unwrap_err();
    assert!(format!("{error:#}").contains(&format!("running core PID {}", std::process::id())));
    assert!(path.exists());
    assert_eq!(
        control::request(&path, Command::Status).await.unwrap().pid,
        std::process::id()
    );
    control::request(&path, Command::Stop).await.unwrap();
    server.await.unwrap().unwrap();
    let unresponsive = UnixListener::bind(&path).unwrap();
    assert!(prepare_control_endpoint(&path).await.is_err());
    assert!(
        path.exists(),
        "an unresponsive server still owns its endpoint"
    );
    drop(unresponsive);
}
#[tokio::test]
async fn node_suggestions_use_full_tags_from_bound_proxies_and_never_include_credentials() {
    let temp = Temp::new();
    let mut manager = Manager::load(temp.0.clone(), PathBuf::new()).unwrap();
    for (name, kind) in [
        ("r1", Kind::Rule),
        ("r2", Kind::Rule),
        ("p1", Kind::Proxy),
        ("p2", Kind::Proxy),
    ] {
        manager.registry.instances.push(Instance {
            name: name.into(),
            kind,
            config: temp.0.join(format!("{name}.toml")),
        });
    }
    let mut config = String::from("standalone=true\n");
    for i in 0..9 {
        config.push_str(&format!("[[outbounds]]\nkind='socks5'\ntag='节点-{i}'\nserver='127.0.0.1:1080'\npassword='secret-should-not-leak'\n"));
    }
    fs::write(temp.0.join("p1.toml"), config).unwrap();
    fs::write(
        temp.0.join("p2.toml"),
        "[[outbounds]]\nkind='socks5'\ntag='other'\nserver='127.0.0.1:1080'\n",
    )
    .unwrap();
    assert_eq!(manager.node_tags("r1").unwrap().len(), 11);
    manager.bind_start("p1", vec!["r1".into()]).unwrap();
    manager.bind_start("p2", vec!["r2".into()]).unwrap();
    let reply = manager.action(Action::NodeTags("r1".into())).await.unwrap();
    assert!(!reply.text.contains("secret-should-not-leak"));
    let tags: Vec<String> = serde_json::from_str(&reply.text).unwrap();
    assert_eq!(tags.len(), 10);
    assert!(tags.contains(&"节点-8".into()));
    assert!(!tags.contains(&"other".into()));
    assert_eq!(manager.node_tags("r2").unwrap(), ["PROXY", "other"]);
    assert_eq!(manager.node_tags("p2").unwrap(), ["PROXY", "other"]);
    assert!(manager.node_tags("missing").is_err());
    let path = temp.0.join("p1.toml");
    let mut text = fs::read_to_string(&path).unwrap();
    text.push_str("\n[proxy_groups.choose]\nmode='select'\nmembers=['节点-0','节点-8']\n");
    fs::write(&path, text).unwrap();
    assert!(manager.node_tags("r1").unwrap().contains(&"choose".into()));

    fs::write(temp.0.join("p2.toml"), "invalid TOML").unwrap();
    assert!(manager.node_tags("r2").unwrap().is_empty());
}
#[test]
fn diagnostic_plan_freezes_saved_configuration_and_rejects_rule_instances() {
    let temp = Temp::new();
    let path = temp.0.join("proxy.toml");
    fs::write(&path, "standalone=true\n").unwrap();
    let mut manager = Manager::load(temp.0.clone(), PathBuf::from("unused")).unwrap();
    manager.registry.instances.push(Instance {
        name: "proxy".into(),
        kind: Kind::Proxy,
        config: path.clone(),
    });
    let plan = manager.diagnostic_plan("proxy").unwrap();
    fs::write(&path, "changed after request").unwrap();
    assert_eq!(plan.text, "standalone=true\n");
    assert!(!plan.running);
    manager.registry.instances[0].kind = Kind::Rule;
    assert!(manager.diagnostic_plan("proxy").is_err());
}
#[tokio::test]
#[ignore = "requires freshly built core binaries"]
async fn live_diagnostics_stream_updates_early_and_cancel_closes_core() {
    use astra_model::diagnostics::Event;
    use tokio::io::AsyncReadExt;
    let temp = Temp::new();
    let home = temp.0.join("state");
    let socket = home.join("manager.sock");
    let bin = test_binaries();
    let slow = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = slow.local_addr().unwrap();
    let fast = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fast_address = fast.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        let (mut stream, _) = slow.accept().await.unwrap();
        let mut greeting = [0; 3];
        stream.read_exact(&mut greeting).await.unwrap();
        assert!(stream.read_u8().await.is_err());
    });
    let quick = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut stream, _) = fast.accept().await.unwrap();
            let mut greeting = [0; 3];
            stream.read_exact(&mut greeting).await.unwrap();
        }
    });
    let source = temp.0.join("proxy.toml");
    fs::write(&source,format!("standalone=true\nstandalone_outbound='slow'\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\nkind='socks5'\ntag='slow'\nserver='{address}'\n[[outbounds]]\nkind='socks5'\ntag='fast'\nserver='{fast_address}'\n")).unwrap();
    let manager = tokio::spawn(serve(home, bin));
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    call(
        &socket,
        Action::Add(Instance {
            name: "proxy".into(),
            kind: Kind::Proxy,
            config: source,
        }),
    )
    .await
    .unwrap();
    let mut stream = astra_platform::platform::LocalStream::connect(&socket)
        .await
        .unwrap();
    astra_ipc::send_management(
        &mut stream,
        &Request {
            version: astra_ipc::API_VERSION,
            action: Action::DiagnosticsStream {
                name: "proxy".into(),
                targets: vec!["slow".into(), "fast".into()],
            },
        },
    )
    .await
    .unwrap();
    timeout(Duration::from_secs(2), async {
        loop {
            let reply: Reply = astra_ipc::receive_management(&mut stream).await.unwrap();
            assert!(reply.error.is_none(), "{:?}", reply.error);
            if let Event::Finished { probe } = serde_json::from_str::<Event>(&reply.text).unwrap() {
                assert_eq!(probe.outbound, "fast");
                break;
            }
        }
    })
    .await
    .expect("fast result must arrive while slow node is still pending");
    timeout(Duration::from_secs(1), call(&socket, Action::List))
        .await
        .unwrap()
        .unwrap();
    drop(stream);
    timeout(Duration::from_secs(2), peer)
        .await
        .expect("cancel must close the diagnostic child tunnel")
        .unwrap();
    let mut stream = astra_platform::platform::LocalStream::connect(&socket)
        .await
        .unwrap();
    astra_ipc::send_management(
        &mut stream,
        &Request {
            version: astra_ipc::API_VERSION,
            action: Action::DiagnosticsStream {
                name: "proxy".into(),
                targets: vec!["fast".into()],
            },
        },
    )
    .await
    .unwrap();
    timeout(Duration::from_secs(3), async {
        loop {
            let reply: Reply = astra_ipc::receive_management(&mut stream).await.unwrap();
            assert!(reply.error.is_none(), "{:?}", reply.error);
            if matches!(
                serde_json::from_str::<Event>(&reply.text).unwrap(),
                Event::Done
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    quick.await.unwrap();
    call(&socket, Action::Shutdown).await.unwrap();
    manager.await.unwrap().unwrap();
}
#[tokio::test]
#[ignore = "requires freshly built core binaries"]
async fn live_stopped_subscription_update_keeps_manager_responsive() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let temp = Temp::new();
    let home = temp.0.join("state");
    let socket = home.join("manager.sock");
    let bin = test_binaries();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let fake = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            request.push(stream.read_u8().await.unwrap());
        }
        tx.send(()).unwrap();
        sleep(Duration::from_millis(300)).await;
        let body = "proxies: [{name: fetched, type: http, server: 127.0.0.1, port: 1234}]";
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
    });
    let source = temp.0.join("proxy.toml");
    // This fixture has no proxy nodes. Use DIRECT so the delayed server is
    // reached and the assertions actually exercise an in-flight refresh.
    let text = format!("standalone=true\nsocks5_listen='127.0.0.1:1080'\n[subscriptions.remote]\nurl='http://{address}/sub'\nproxy='DIRECT'\n");
    fs::write(&source, &text).unwrap();
    let manager = tokio::spawn(serve(home, bin));
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    call(
        &socket,
        Action::Add(Instance {
            name: "proxy".into(),
            kind: Kind::Proxy,
            config: source.clone(),
        }),
    )
    .await
    .unwrap();
    let first_socket = socket.clone();
    let update = tokio::spawn(async move {
        call(&first_socket, Action::UpdateSubscriptions("proxy".into())).await
    });
    timeout(Duration::from_secs(5), rx).await.unwrap().unwrap();
    let status = timeout(Duration::from_millis(200), call(&socket, Action::List))
        .await
        .unwrap()
        .unwrap();
    assert!(status.rows[0].pid.is_none());
    let mut push = crate::protocol::subscribe(&socket, Action::WatchList)
        .await
        .unwrap();
    let state: Reply = timeout(
        Duration::from_millis(200),
        astra_ipc::receive_management(&mut push),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(state.rows[0].instance.name, "proxy");
    let mut log = crate::log_stream::connect(&socket, "@tui".into(), None)
        .await
        .unwrap();
    let event: LogEvent = timeout(
        Duration::from_millis(200),
        astra_ipc::receive_management(&mut log),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        matches!(event,LogEvent::Data{bytes,..}if String::from_utf8_lossy(&bytes).contains("manager"))
    );
    drop((push, log));

    assert!(update.await.unwrap().unwrap().text.contains("Updated 1"));
    let refresh_log = fs::read_to_string(socket.parent().unwrap().join("proxy.log")).unwrap();
    for expected in [
        "subscription update started",
        "subscription converted",
        "nodes=1",
        "subscription refresh finished; node cache saved",
    ] {
        assert!(refresh_log.contains(expected), "{refresh_log}");
    }
    let nodes = call(&socket, Action::SubscriptionNodes("proxy".into()))
        .await
        .unwrap();
    assert!(nodes.text.contains("fetched"));
    assert_eq!(fs::read_to_string(source).unwrap(), text);
    call(&socket, Action::Shutdown).await.unwrap();
    fake.await.unwrap();
    manager.await.unwrap().unwrap();
}
#[tokio::test]
#[ignore = "requires freshly built core binaries"]
async fn live_diagnostics_use_running_snapshot_keep_manager_responsive_and_cancel_child() {
    use tokio::io::AsyncReadExt;
    let temp = Temp::new();
    let home = temp.0.join("state");
    let socket = home.join("manager.sock");
    let bin = test_binaries();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = listener.local_addr().unwrap();
    let (accepted_tx, mut accepted_rx) = tokio::sync::mpsc::channel(2);
    let fake = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut greeting = [0; 3];
            stream.read_exact(&mut greeting).await.unwrap();
            accepted_tx.send(()).await.unwrap();
            assert!(stream.read_u8().await.is_err()); // timeout/cancel closes the outbound.
        }
    });
    let source = temp.0.join("proxy.toml");
    let inbound = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = inbound.local_addr().unwrap();
    drop(inbound);
    fs::write(&source, format!("standalone=true\nsocks5_listen='{address}'\n[[outbounds]]\nkind='socks5'\ntag='edge'\nserver='{upstream}'\n")).unwrap();
    let manager = tokio::spawn(serve(home, bin));
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    call(
        &socket,
        Action::Add(Instance {
            name: "proxy".into(),
            kind: Kind::Proxy,
            config: source.clone(),
        }),
    )
    .await
    .unwrap();
    call(&socket, Action::Start("proxy".into())).await.unwrap();
    let reloaded = fs::read_to_string(&source)
        .unwrap()
        .replace("tag='edge'", "tag='live-edge'");
    fs::write(&source, reloaded).unwrap();
    // Reload the core directly, bypassing the manager's cached configuration.
    control::request(
        &socket.with_file_name("proxy.sock"),
        Command::Reload { path: None },
    )
    .await
    .unwrap();
    fs::write(
        &source,
        "invalid modified file; running core must still be used",
    )
    .unwrap();
    let catalog = call(&socket, Action::DiagnosticTargets("proxy".into()))
        .await
        .unwrap();
    let targets: Vec<String> = serde_json::from_str(&catalog.text).unwrap();
    assert_eq!(targets, ["DIRECT", "live-edge", "PROXY"]);
    assert!(
        accepted_rx.try_recv().is_err(),
        "Listing nodes must not start probes"
    );
    let first_socket = socket.clone();
    let first = tokio::spawn(async move {
        call(
            &first_socket,
            Action::DiagnosticsSelected {
                name: "proxy".into(),
                targets: vec!["live-edge".into()],
            },
        )
        .await
    });
    timeout(Duration::from_secs(5), accepted_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let list = timeout(Duration::from_secs(1), call(&socket, Action::List))
        .await
        .unwrap()
        .unwrap();
    assert!(list.rows[0].pid.is_some());
    let reply = timeout(Duration::from_secs(12), first)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let report: astra_model::diagnostics::Report = serde_json::from_str(&reply.text).unwrap();
    assert!(report.running_config);
    assert_eq!(report.probes.len(), 1);
    assert_eq!(report.probes[0].outbound, "live-edge");
    assert!(!report.probes[0].direct);
    assert!(report.probes[0].ip.is_none());
    assert!(report.probes[0].error.is_some());
    let second_socket = socket.clone();
    let second = tokio::spawn(async move {
        call(
            &second_socket,
            Action::DiagnosticsSelected {
                name: "proxy".into(),
                targets: vec!["live-edge".into()],
            },
        )
        .await
    });
    timeout(Duration::from_secs(5), accepted_rx.recv())
        .await
        .unwrap()
        .unwrap();
    second.abort();
    assert!(second.await.unwrap_err().is_cancelled());
    timeout(Duration::from_secs(3), fake)
        .await
        .expect("cancelling the diagnostic call must close the child's outbound connection")
        .unwrap();
    call(&socket, Action::Shutdown).await.unwrap();
    manager.await.unwrap().unwrap();
}
#[test]
fn startup_bindings_are_proxy_to_one_rule_and_persistent() {
    let temp = Temp::new();
    let mut manager = Manager::load(temp.0.clone(), PathBuf::from("/unused")).unwrap();
    for (name, kind) in [
        ("proxy", Kind::Proxy),
        ("other-proxy", Kind::Proxy),
        ("rules-a", Kind::Rule),
        ("rules-b", Kind::Rule),
    ] {
        manager.registry.instances.push(Instance {
            name: name.into(),
            kind,
            config: temp.0.join(format!("{name}.toml")),
        });
    }
    manager
        .bind_start("proxy", vec!["rules-a".into(), "rules-a".into()])
        .unwrap();
    manager
        .bind_start("other-proxy", vec!["rules-a".into()])
        .unwrap();
    assert_eq!(manager.start_plan("proxy").unwrap(), ["rules-a", "proxy"]);
    assert!(manager
        .bind_start("proxy", vec!["rules-a".into(), "rules-b".into()])
        .is_err());
    assert!(manager
        .bind_start("proxy", vec!["other-proxy".into()])
        .is_err());
    assert!(manager
        .bind_start("rules-b", vec!["rules-a".into()])
        .is_err());
    assert!(manager.bind_start("proxy", vec!["missing".into()]).is_err());
    let restored = Manager::load(temp.0.clone(), PathBuf::from("/unused")).unwrap();
    assert_eq!(restored.start_plan("proxy").unwrap(), ["rules-a", "proxy"]);
    manager
        .registry
        .start_bindings
        .insert("rules-a".into(), vec!["proxy".into()]);
    assert!(manager.start_plan("proxy").is_err());
    manager.bind_start("rules-a", Vec::new()).unwrap();
    manager.bind_start("proxy", Vec::new()).unwrap();
    assert_eq!(manager.start_plan("proxy").unwrap(), ["proxy"]);
}
#[test]
fn rule_bindings_edit_the_same_relation_and_start_rule_before_all_proxies() {
    let temp = Temp::new();
    let mut manager = Manager::load(temp.0.clone(), PathBuf::from("/unused")).unwrap();
    for (name, kind) in [
        ("rule-a", Kind::Rule),
        ("rule-b", Kind::Rule),
        ("proxy-a", Kind::Proxy),
        ("proxy-b", Kind::Proxy),
    ] {
        manager.registry.instances.push(Instance {
            name: name.into(),
            kind,
            config: temp.0.join(format!("{name}.toml")),
        });
    }
    manager
        .bind_start(
            "rule-a",
            vec!["proxy-b".into(), "proxy-a".into(), "proxy-a".into()],
        )
        .unwrap();
    assert_eq!(
        manager.start_plan("rule-a").unwrap(),
        ["rule-a", "proxy-a", "proxy-b"]
    );
    assert_eq!(
        manager.start_plan("proxy-a").unwrap(),
        ["rule-a", "proxy-a"]
    );
    assert_eq!(
        manager
            .details(&manager.instance("rule-a").unwrap())
            .bindings,
        ["proxy-a", "proxy-b"]
    );
    assert_eq!(
        manager
            .details(&manager.instance("proxy-a").unwrap())
            .bindings,
        ["rule-a"]
    );
    assert!(manager.bind_start("rule-a", vec!["rule-b".into()]).is_err());
    assert!(manager
        .bind_start("rule-a", vec!["missing".into()])
        .is_err());
    assert_eq!(manager.bound_proxies("rule-a"), ["proxy-a", "proxy-b"]);
    manager
        .bind_start("rule-b", vec!["proxy-b".into()])
        .unwrap();
    assert_eq!(manager.bound_proxies("rule-a"), ["proxy-a"]);
    let restored = Manager::load(temp.0.clone(), PathBuf::from("/unused")).unwrap();
    assert_eq!(
        restored.start_plan("rule-b").unwrap(),
        ["rule-b", "proxy-b"]
    );
    manager.bind_start("rule-a", Vec::new()).unwrap();
    assert_eq!(manager.start_plan("proxy-a").unwrap(), ["proxy-a"]);
    assert_eq!(
        manager.start_plan("proxy-b").unwrap(),
        ["rule-b", "proxy-b"]
    );
}
#[test]
fn large_logs_rotate_to_a_single_previous_file() {
    let temp = Temp::new();
    let log = temp.0.join("core.log");
    let old = temp.0.join("core.log.1");
    fs::write(&log, b"small").unwrap();
    rotate_log(&log);
    assert_eq!(fs::read(&log).unwrap(), b"small");
    assert!(!old.exists());
    fs::write(&old, b"older").unwrap();
    fs::File::create(&log)
        .unwrap()
        .set_len(LOG_ROTATE_BYTES + 1)
        .unwrap();
    rotate_log(&log);
    assert!(!log.exists());
    assert_eq!(fs::metadata(&old).unwrap().len(), LOG_ROTATE_BYTES + 1);
}
#[test]
fn proxy_without_binding_starts_the_rule_core_its_config_uses() {
    let temp = Temp::new();
    let mut manager = Manager::load(temp.0.clone(), PathBuf::from("/unused")).unwrap();
    let socket = toml::Value::String(temp.0.join("rule.r").to_string_lossy().into_owned());
    let files = [
        (
            "other",
            Kind::Rule,
            "socket = 'elsewhere'\ndefault_action = 'direct'\n".to_owned(),
        ),
        (
            "rules",
            Kind::Rule,
            format!("socket = {socket}\ndefault_action = 'direct'\n"),
        ),
        ("proxy", Kind::Proxy, format!("rule_socket = {socket}\n")),
        (
            "solo",
            Kind::Proxy,
            format!("standalone = true\nrule_socket = {socket}\n"),
        ),
    ];
    for (name, kind, text) in files {
        let config = temp.0.join(format!("{name}.toml"));
        fs::write(&config, text).unwrap();
        manager.registry.instances.push(Instance {
            name: name.into(),
            kind,
            config,
        });
    }
    assert_eq!(manager.start_plan("proxy").unwrap(), ["rules", "proxy"]);
    assert_eq!(manager.start_plan("solo").unwrap(), ["solo"]);
    // An explicit binding still wins over the inferred one.
    manager.bind_start("proxy", vec!["other".into()]).unwrap();
    assert_eq!(manager.start_plan("proxy").unwrap(), ["other", "proxy"]);
}
#[test]
fn instance_summary_contains_endpoints_modes_tags_and_no_credentials() {
    let temp = Temp::new();
    let config = temp.0.join("proxy.toml");
    fs::write(&config, "standalone=true\nhttp_listen='127.0.0.1:8080'\n[tun]\nname='Astra'\n[[outbounds]]\ntag='edge'\nkind='socks5'\nserver='127.0.0.1:1080'\npassword='do-not-expose'\n").unwrap();
    let mut manager = Manager::load(temp.0.clone(), PathBuf::from("/unused")).unwrap();
    manager
        .registry
        .start_bindings
        .insert("proxy".into(), vec!["rules".into()]);
    let instance = Instance {
        name: "proxy".into(),
        kind: Kind::Proxy,
        config,
    };
    let details = manager.details(&instance);
    assert_eq!(details.modes, ["Standalone"]);
    assert_eq!(details.listeners, ["HTTP 127.0.0.1:8080"]);
    assert_eq!(details.outbounds, ["edge", "PROXY [select]"]);
    assert_eq!(details.outbound_count, 2);
    assert_eq!(details.bindings, ["rules"]);
    assert!(!serde_json::to_string(&details)
        .unwrap()
        .contains("do-not-expose"));
    let large = format!(
        "standalone=true\n{}",
        format!("[[outbounds]]\ntag='{}'\n", "a".repeat(40000)).repeat(12)
    );
    fs::write(&instance.config, large).unwrap();
    let summary = manager.details(&instance);
    assert_eq!(summary.outbound_count, 13);
    assert!(
        serde_json::to_vec(&summary).unwrap().len() < 4096,
        "List metadata must stay bounded even with very long node tags"
    );
}
#[tokio::test]
#[ignore = "requires freshly built core binaries"]
async fn live_force_start_replaces_blocked_bound_sessions_and_keeps_running_cores() {
    let temp = Temp::new();
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy_address = reservation.local_addr().unwrap();
    let home = temp.0.join("state");
    private_dir(&home).unwrap();
    private_dir(&home.join("processes")).unwrap();
    let mut manager = Manager::load_with_runtime(
        home.clone(),
        temp.0.join("config"),
        test_binaries(),
        Some(temp.0.join("runtime")),
    )
    .unwrap();
    let route = temp.0.join("rule.r");
    for (name, kind, text) in [
        (
            "rules",
            Kind::Rule,
            format!(
                "socket={:?}\ndefault_action='direct'\n",
                route.to_string_lossy()
            ),
        ),
        (
            "proxy",
            Kind::Proxy,
            format!(
                "rule_socket={:?}\nmixed_listen='{proxy_address}'\n",
                route.to_string_lossy()
            ),
        ),
    ] {
        let config = temp.0.join(format!("{name}.toml"));
        fs::write(&config, text).unwrap();
        manager.registry.instances.push(Instance {
            name: name.into(),
            kind,
            config,
        });
        fs::write(
            home.join("processes").join(format!("{name}.json")),
            b"invalid old session",
        )
        .unwrap();
        #[cfg(windows)]
        fs::write(manager.socket(name), b"").unwrap();
        #[cfg(unix)]
        drop(std::os::unix::net::UnixListener::bind(manager.socket(name)).unwrap());
    }
    manager
        .registry
        .start_bindings
        .insert("proxy".into(), vec!["rules".into()]);
    manager.recover_processes().await.unwrap();
    assert!(manager.action(Action::Start("proxy".into())).await.is_err());
    drop(reservation);
    let result = manager.action(Action::ForceStart("proxy".into())).await;
    let first: Vec<_> = ["rules", "proxy"]
        .map(|name| manager.running.get(name).and_then(|run| run.child.id()))
        .into();
    let repeat = if result.is_ok() {
        manager.action(Action::ForceStart("proxy".into())).await
    } else {
        Ok(Reply::text("not retried"))
    };
    let second: Vec<_> = ["rules", "proxy"]
        .map(|name| manager.running.get(name).and_then(|run| run.child.id()))
        .into();
    let records: Vec<_> = ["rules", "proxy"]
        .map(|name| fs::read(home.join("processes").join(format!("{name}.json"))).ok())
        .into();
    let cleared = manager.blocked.is_empty();
    let cleanup = manager.stop_all().await;
    result.unwrap();
    repeat.unwrap();
    cleanup.unwrap();
    assert!(cleared);
    assert!(first.iter().all(Option::is_some));
    assert_eq!(
        first, second,
        "force start must reuse already managed running dependencies"
    );
    for bytes in records {
        let record: serde_json::Value = serde_json::from_slice(&bytes.unwrap()).unwrap();
        assert_eq!(record["version"], 1);
        assert!(record["identity"]["boot_id"]
            .as_str()
            .is_some_and(|boot| !boot.is_empty()));
    }
}

#[tokio::test]
#[ignore = "requires freshly built core binaries"]
async fn live_bound_start_reuses_running_dependencies_and_rolls_back_new_ones() {
    let temp = Temp::new();
    let bin = test_binaries();
    let route = temp.0.join("rule.r");
    let rules = temp.0.join("rule.toml");
    fs::write(
        &rules,
        format!(
            "socket={:?}\ndefault_action='direct'\n",
            route.to_string_lossy()
        ),
    )
    .unwrap();
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy = temp.0.join("proxy.toml");
    fs::write(
        &proxy,
        format!(
            "rule_socket={:?}\nsocks5_listen='{}'\n",
            route.to_string_lossy(),
            occupied.local_addr().unwrap()
        ),
    )
    .unwrap();
    let mut manager = Manager::load(temp.0.clone(), bin).unwrap();
    manager.registry.instances = vec![
        Instance {
            name: "rules".into(),
            kind: Kind::Rule,
            config: rules,
        },
        Instance {
            name: "proxy".into(),
            kind: Kind::Proxy,
            config: proxy,
        },
    ];
    manager.bind_start("proxy", vec!["rules".into()]).unwrap();
    assert!(manager.start("proxy").await.is_err());
    assert!(
        manager.running.is_empty(),
        "Newly started dependency must be rolled back"
    );
    manager.start_one("rules").await.unwrap();
    let pid = manager.running["rules"].child.id();
    assert!(manager.start("proxy").await.is_err());
    assert_eq!(
        manager.running["rules"].child.id(),
        pid,
        "Existing dependency must be kept"
    );
    drop(occupied);
    manager.start("proxy").await.unwrap();
    let proxy_pid = manager.running["proxy"].child.id();
    let rows = manager.action(Action::List).await.unwrap().rows;
    assert!(
        rows.iter().all(|row| !row.changed),
        "Canonical path spelling must not create a pending reload"
    );
    manager.start("proxy").await.unwrap();
    assert_eq!(manager.running["rules"].child.id(), pid);
    assert_eq!(manager.running["proxy"].child.id(), proxy_pid);
    manager.stop("proxy").await.unwrap();
    assert!(manager.running.contains_key("rules"));
    let proxy_two = temp.0.join("proxy-two.toml");
    let blocked = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    fs::write(
        &proxy_two,
        format!(
            "rule_socket={:?}\nsocks5_listen='{}'\n",
            route.to_string_lossy(),
            blocked.local_addr().unwrap()
        ),
    )
    .unwrap();
    manager.registry.instances.push(Instance {
        name: "proxy-two".into(),
        kind: Kind::Proxy,
        config: proxy_two,
    });
    manager
        .bind_start("rules", vec!["proxy".into(), "proxy-two".into()])
        .unwrap();
    assert!(manager.start("rules").await.is_err());
    assert_eq!(
        manager.running.len(),
        1,
        "A failed second proxy rolls back the first newly started proxy"
    );
    assert_eq!(manager.running["rules"].child.id(), pid);
    drop(blocked);
    manager.start("rules").await.unwrap();
    assert_eq!(manager.running["rules"].child.id(), pid);
    assert_eq!(manager.running.len(), 3);
    let pids: Vec<_> = ["rules", "proxy", "proxy-two"]
        .iter()
        .map(|name| manager.running[*name].child.id())
        .collect();
    manager.start("rules").await.unwrap();
    assert_eq!(
        pids,
        ["rules", "proxy", "proxy-two"]
            .iter()
            .map(|name| manager.running[*name].child.id())
            .collect::<Vec<_>>()
    );
    manager.stop_all().await.unwrap();
    #[cfg(windows)]
    for name in ["rules", "proxy", "proxy-two"] {
        fs::write(manager.socket(name), b"").unwrap();
    }
    manager.start("rules").await.unwrap();
    assert_eq!(manager.running.len(), 3);
    manager.stop_all().await.unwrap();
}
#[test]
fn defaults_create_parents_are_private_and_preserve_existing_files() {
    let temp = Temp::new();
    for kind in [Kind::Proxy, Kind::Rule] {
        let path = temp.0.join(kind.binary()).join("config.toml");
        assert!(ensure_config(kind, &path).unwrap());
        let document: toml::Value = toml::from_str(&read_config(&path).unwrap()).unwrap();
        assert!(document
            .get(if kind == Kind::Proxy {
                "rule_socket"
            } else {
                "socket"
            })
            .is_some());
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::write(&path, "# user configuration").unwrap();
        assert!(!ensure_config(kind, &path).unwrap());
        assert_eq!(read_config(&path).unwrap(), "# user configuration");
    }
}
#[test]
fn default_creation_errors_can_be_fixed_and_retried_without_overwrite() {
    let temp = Temp::new();
    let directory = temp.0.join("blocked");
    fs::write(&directory, "existing file").unwrap();
    let path = directory.join("rule.toml");
    assert!(ensure_config(Kind::Rule, &path).is_err());
    assert_eq!(fs::read_to_string(&directory).unwrap(), "existing file");
    fs::remove_file(&directory).unwrap();
    assert!(ensure_config(Kind::Rule, &path).unwrap());
    assert!(!ensure_config(Kind::Rule, &path).unwrap());
}
#[tokio::test]
async fn legacy_registry_migrates_and_runtime_changes_leave_configuration_untouched() {
    let temp = Temp::new();
    let state = temp.0.join("state");
    let config = temp.0.join("settings");
    private_dir(&state).unwrap();
    let legacy = "[[instances]]\nname='proxy'\nkind='proxy'\nconfig='external.toml'\n[log_levels]\nproxy='debug'\n";
    fs::write(state.join("instances.toml"), legacy).unwrap();
    let mut manager =
        Manager::load_with_config(state.clone(), config.clone(), PathBuf::new()).unwrap();
    assert_eq!(manager.level("proxy"), LogLevel::Debug);
    let registry = fs::read(config.join("instances.toml")).unwrap();
    assert!(!String::from_utf8_lossy(&registry).contains("log_levels"));
    manager
        .action(Action::SetLogLevel {
            name: "proxy".into(),
            level: LogLevel::Trace,
        })
        .await
        .unwrap();
    assert_eq!(fs::read(config.join("instances.toml")).unwrap(), registry);
    assert_eq!(
        fs::read_to_string(state.join("instances.toml")).unwrap(),
        legacy
    );
    let restored =
        Manager::load_with_config(state.clone(), config.clone(), PathBuf::new()).unwrap();
    assert_eq!(restored.level("proxy"), LogLevel::Trace);
    assert!(!state.join("configs").exists());
    assert!(config.join("configs").is_dir());
}

#[test]
fn invalid_legacy_registry_is_retained_without_import_or_runtime_reset() {
    for text in [
        "[[instances]]\nname='manager'\nkind='proxy'\nconfig='external.toml'\n",
        "[[instances]]\nname='management'\nkind='proxy'\nconfig='external.toml'\n",
        "[[instances]]\nname='server-node'\nkind='rule'\nconfig='external.toml'\n",
        "[[instances]]\nname='node'\nkind='server'\nconfig='external.toml'\n",
        "future_setting=true\n",
        "[[instances]]\nname='node'\nkind='proxy'\nconfig='external.toml'\nfuture_setting=true\n",
    ] {
        let temp = Temp::new();
        let state = temp.0.join("state");
        let config = temp.0.join("settings");
        private_dir(&state).unwrap();
        fs::write(state.join("instances.toml"), text).unwrap();
        assert!(Manager::load_with_config(state.clone(), config.clone(), PathBuf::new()).is_err());
        assert_eq!(
            fs::read_to_string(state.join("instances.toml")).unwrap(),
            text
        );
        assert!(!config.join("instances.toml").exists());
        assert!(!state.join("runtime.json").exists());
    }
}

#[test]
fn unknown_runtime_state_is_retained_without_reset() {
    let temp = Temp::new();
    let state = temp.0.join("state");
    let config = temp.0.join("settings");
    private_dir(&state).unwrap();
    let original = "{\"log_levels\":{},\"future_setting\":true}";
    fs::write(state.join("runtime.json"), original).unwrap();
    assert!(Manager::load_with_config(state.clone(), config.clone(), PathBuf::new()).is_err());
    assert_eq!(
        fs::read_to_string(state.join("runtime.json")).unwrap(),
        original
    );
    assert!(!config.join("instances.toml").exists());
}

#[test]
fn atomic_save_and_private_permissions() {
    let temp = Temp::new();
    private_dir(&temp.0).unwrap();
    let path = temp.0.join("config.toml");
    atomic_write(&path, b"old").unwrap();
    atomic_write(&path, b"new").unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"new");
    #[cfg(unix)]
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    #[cfg(unix)]
    assert_eq!(
        fs::metadata(&temp.0).unwrap().permissions().mode() & 0o777,
        0o700
    );
}
#[tokio::test]
async fn manager_rejects_version_and_cleans_socket() {
    let temp = Temp::new();
    let home = temp.0.clone();
    let socket = home.join("manager.sock");
    let task = tokio::spawn(serve(home, PathBuf::from("/nonexistent")));
    for _ in 0..50 {
        if socket.exists() {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    let mut stream = astra_platform::platform::LocalStream::connect(&socket)
        .await
        .unwrap();
    astra_ipc::send_management(
        &mut stream,
        &Request {
            version: 999,
            action: Action::Shutdown,
        },
    )
    .await
    .unwrap();
    let reply: Reply = astra_ipc::receive_management(&mut stream).await.unwrap();
    assert!(reply.error.unwrap().contains("version"));
    assert!(call(&socket, Action::List).await.unwrap().rows.is_empty());
    call(&socket, Action::Shutdown).await.unwrap();
    task.await.unwrap().unwrap();
    assert!(!socket.exists());
}
#[tokio::test]
#[ignore = "run after cargo build --workspace"]
async fn live_bound_configs_streaming_and_runtime_log_level() {
    use tokio::io::AsyncWriteExt;
    let temp = Temp::new();
    let home = temp.0.join("state");
    let socket = home.join("manager.sock");
    let bin = test_binaries();
    let task = tokio::spawn(serve(home.clone(), bin));
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    for (name, kind) in [("rules", Kind::Rule), ("proxy", Kind::Proxy)] {
        call(
            &socket,
            Action::Create {
                name: name.into(),
                kind,
            },
        )
        .await
        .unwrap();
    }
    let canonical_home = fs::canonicalize(&home).unwrap();
    let rows = call(&socket, Action::List).await.unwrap().rows;
    assert_eq!(
        rows[0].instance.config,
        canonical_home.join("config/configs/rules.toml")
    );
    assert_eq!(
        rows[1].instance.config,
        canonical_home.join("config/configs/proxy.toml")
    );
    let rule_config: toml::Value =
        toml::from_str(&fs::read_to_string(&rows[0].instance.config).unwrap()).unwrap();
    let proxy_config: toml::Value =
        toml::from_str(&fs::read_to_string(&rows[1].instance.config).unwrap()).unwrap();
    assert_eq!(
        rule_config["socket"].as_str(),
        proxy_config["rule_socket"].as_str()
    );
    assert!(call(
        &socket,
        Action::Create {
            name: "rules".into(),
            kind: Kind::Rule
        }
    )
    .await
    .is_err());
    let mut stream = crate::log_stream::connect(&socket, "rules".into(), None)
        .await
        .unwrap();
    assert!(matches!(
        astra_ipc::receive_management::<LogEvent>(&mut stream)
            .await
            .unwrap(),
        LogEvent::Waiting
    ));
    call(&socket, Action::Start("rules".into())).await.unwrap();
    async fn until(stream: &mut astra_platform::platform::LocalStream, needle: &str) -> String {
        timeout(Duration::from_secs(5), async {
            let mut output = String::new();
            loop {
                if let LogEvent::Data { bytes, .. } =
                    astra_ipc::receive_management::<LogEvent>(stream)
                        .await
                        .unwrap()
                {
                    output.push_str(&String::from_utf8_lossy(&bytes));
                    if output.contains(needle) {
                        return output;
                    }
                }
            }
        })
        .await
        .unwrap()
    }
    until(&mut stream, "Rule Core ready").await;
    let pid = call(&socket, Action::List).await.unwrap().rows[0].pid;
    call(
        &socket,
        Action::SetLogLevel {
            name: "rules".into(),
            level: LogLevel::Route,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        call(&socket, Action::GetLogLevel("rules".into()))
            .await
            .unwrap()
            .text,
        "route"
    );
    assert_eq!(pid, call(&socket, Action::List).await.unwrap().rows[0].pid);
    call(
        &socket,
        Action::SetLogLevel {
            name: "rules".into(),
            level: LogLevel::Debug,
        },
    )
    .await
    .unwrap();
    assert_eq!(pid, call(&socket, Action::List).await.unwrap().rows[0].pid);
    let route = PathBuf::from(rule_config["socket"].as_str().unwrap());
    let mut bad = astra_platform::platform::LocalStream::connect(&route)
        .await
        .unwrap();
    bad.write_all(&[0, 0, 0, 0]).await.unwrap();
    drop(bad);
    let text = until(&mut stream, "IPC peer disconnected").await;
    assert!(text.contains("DEBUG"));
    assert!(!text.contains('\x1b'));
    call(
        &socket,
        Action::SetLogLevel {
            name: "rules".into(),
            level: LogLevel::Error,
        },
    )
    .await
    .unwrap();
    let previous = fs::metadata(home.join("rules.log")).unwrap().len();
    let mut bad = astra_platform::platform::LocalStream::connect(&route)
        .await
        .unwrap();
    bad.write_all(&[0, 0, 0, 0]).await.unwrap();
    drop(bad);
    sleep(Duration::from_millis(250)).await;
    assert_eq!(
        fs::metadata(home.join("rules.log")).unwrap().len(),
        previous
    );
    call(&socket, Action::Shutdown).await.unwrap();
    drop(stream);
    task.await.unwrap().unwrap();
    let manager = Manager::load(home.clone(), PathBuf::new()).unwrap();
    assert_eq!(manager.level("rules"), LogLevel::Error);
    assert_eq!(
        manager.instance("rules").unwrap().config,
        canonical_home.join("config/configs/rules.toml")
    );
}
/// Requires freshly built core binaries; exercises the actual management IPC.
#[tokio::test]
#[ignore = "run after cargo build --workspace"]
async fn live_cores_and_config_lifecycle() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let temp = Temp::new();
    let home = temp.0.join("state");
    let bin = test_binaries();
    let route_socket = temp.0.join("rule.sock");
    let rule_path = temp.0.join("rule.toml");
    let proxy_path = temp.0.join("proxy.toml");
    let rule_text = format!(
        "socket = {:?}\ndefault_action = \"direct\"\n",
        route_socket.to_str().unwrap()
    );
    fs::write(&rule_path, &rule_text).unwrap();
    let reserve = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy_addr = reserve.local_addr().unwrap();
    drop(reserve);
    fs::write(
        &proxy_path,
        format!(
            "rule_socket = {:?}\nsocks5_listen = \"{proxy_addr}\"\ndecision_cache_ttl_secs = 0\n",
            route_socket.to_str().unwrap()
        ),
    )
    .unwrap();
    let task = tokio::spawn(serve(home.clone(), bin));
    let socket = home.join("manager.sock");
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    for (name, kind, config) in [
        ("rules", Kind::Rule, rule_path.clone()),
        ("proxy", Kind::Proxy, proxy_path.clone()),
    ] {
        call(
            &socket,
            Action::Add(Instance {
                name: name.into(),
                kind,
                config,
            }),
        )
        .await
        .unwrap();
        call(&socket, Action::Start(name.into())).await.unwrap();
    }
    let rows = call(&socket, Action::List).await.unwrap().rows;
    assert!(rows.iter().all(|r| r.state == "Running" && r.pid.is_some()));
    let proxy_pid = rows
        .iter()
        .find(|row| row.instance.name == "proxy")
        .unwrap()
        .pid;
    call(&socket, Action::Start("proxy".into())).await.unwrap();
    assert_eq!(
        call(&socket, Action::List)
            .await
            .unwrap()
            .rows
            .iter()
            .find(|row| row.instance.name == "proxy")
            .unwrap()
            .pid,
        proxy_pid
    );
    assert!(call(&socket, Action::Remove("rules".into())).await.is_err());
    assert!(call(
        &socket,
        Action::Save {
            name: "rules".into(),
            original: rule_text.clone(),
            text: "invalid = [".into()
        }
    )
    .await
    .is_err());
    assert_eq!(fs::read_to_string(&rule_path).unwrap(), rule_text);
    let modified = format!("{rule_text}# edited\n");
    call(
        &socket,
        Action::Save {
            name: "rules".into(),
            original: rule_text.clone(),
            text: modified.clone(),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        fs::read_to_string(home.join("backups/rules.toml.bak")).unwrap(),
        rule_text
    );
    assert!(!call(&socket, Action::List).await.unwrap().rows[0].changed);
    assert!(call(
        &socket,
        Action::Save {
            name: "rules".into(),
            original: rule_text,
            text: modified
        }
    )
    .await
    .is_err());
    call(&socket, Action::Restart("rules".into()))
        .await
        .unwrap();
    assert!(!call(&socket, Action::List).await.unwrap().rows[0].changed);
    // Real SOCKS5 CONNECT + echo after independently restarting Rule Core.
    let echo = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let echo_port = echo.local_addr().unwrap().port();
    let echo_task = tokio::spawn(async move {
        let (mut stream, _) = echo.accept().await.unwrap();
        let mut bytes = [0; 4];
        stream.read_exact(&mut bytes).await.unwrap();
        stream.write_all(&bytes).await.unwrap();
    });
    let mut stream = tokio::net::TcpStream::connect(proxy_addr).await.unwrap();
    stream.write_all(&[5, 1, 0]).await.unwrap();
    let mut greeting = [0; 2];
    stream.read_exact(&mut greeting).await.unwrap();
    assert_eq!(greeting, [5, 0]);
    let [hi, lo] = echo_port.to_be_bytes();
    stream
        .write_all(&[5, 1, 0, 1, 127, 0, 0, 1, hi, lo])
        .await
        .unwrap();
    let mut connected = [0; 10];
    stream.read_exact(&mut connected).await.unwrap();
    assert_eq!(connected[1], 0);
    stream.write_all(b"test").await.unwrap();
    let mut bytes = [0; 4];
    stream.read_exact(&mut bytes).await.unwrap();
    assert_eq!(&bytes, b"test");
    drop(stream);
    echo_task.await.unwrap();
    call(
        &socket,
        Action::Add(Instance {
            name: "conflict".into(),
            kind: Kind::Proxy,
            config: proxy_path,
        }),
    )
    .await
    .unwrap();
    assert!(call(&socket, Action::Start("conflict".into()))
        .await
        .is_err());
    let log = call(&socket, Action::Logs("conflict".into()))
        .await
        .unwrap()
        .text
        .to_lowercase();
    assert!(
        log.contains("bind") || log.contains("address already in use"),
        "{log}"
    );
    call(&socket, Action::Shutdown).await.unwrap();
    task.await.unwrap().unwrap();
    assert!(!route_socket.exists());
    assert!(!home.join("proxy.sock").exists());
    let manager = Manager::load(home, PathBuf::new()).unwrap();
    assert_eq!(manager.registry.instances.len(), 3);
    assert!(manager.running.is_empty());
}
