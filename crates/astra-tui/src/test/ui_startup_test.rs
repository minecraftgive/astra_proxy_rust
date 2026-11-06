use super::*;
use crate::windows::Launch;

fn row(name: &str, kind: Kind, pid: Option<u32>) -> Row {
    Row {
        instance: Instance {
            name: name.into(),
            kind,
            config: format!("{name}.toml").into(),
        },
        pid,
        state: if pid.is_some() { "Running" } else { "Stopped" }.into(),
        uptime: 0,
        changed: false,
    }
}

fn enable_all(ui: &mut Ui) {
    for row in &ui.rows {
        ui.details
            .entry(row.instance.name.clone())
            .or_default()
            .auto_start = true;
    }
}

fn next(startup: &mut Instances, ui: &Ui, ready: bool) -> Option<Action> {
    startup
        .prepare(ui, ready)
        .then(|| startup.next_action(&ui.rows, &ui.details))
        .flatten()
}

fn assert_start(action: Option<Action>, name: &str) {
    assert!(matches!(action, Some(Action::Start(actual)) if actual == name));
}

#[test]
fn resumed_interface_does_not_start_stopped_auto_start_instances() {
    let mut ui = Ui::new(Session::default());
    ui.rows = vec![
        row("proxy", Kind::Proxy, None),
        row("rule", Kind::Rule, None),
    ];
    enable_all(&mut ui);
    let mut startup = Instances::resumed();
    assert!(next(&mut startup, &ui, false).is_none());
    assert!(next(&mut startup, &ui, true).is_none());
    assert!(startup.refresh.is_none());
}

#[test]
fn waits_for_initial_snapshot_and_only_starts_stopped_instances_once() {
    let mut ui = Ui::new(Session::default());
    enable_all(&mut ui);
    let mut startup = Instances::new(&Launch::default());
    assert!(next(&mut startup, &ui, false).is_none());
    ui.rows = vec![
        row("running", Kind::Proxy, Some(12)),
        row("proxy", Kind::Proxy, None),
        row("rule", Kind::Rule, None),
        row("server", Kind::Server, None),
    ];
    enable_all(&mut ui);
    assert_start(next(&mut startup, &ui, true), "proxy");
    assert_start(next(&mut startup, &ui, true), "server");
    assert_start(next(&mut startup, &ui, true), "rule");
    // An unchanged stopped state (including a failed start) is not retried.
    assert!(next(&mut startup, &ui, true).is_none());
    ui.rows.push(row("created-later", Kind::Proxy, None));
    assert!(next(&mut startup, &ui, false).is_none());
    assert!(next(&mut startup, &ui, true).is_none());
}

#[test]
fn detached_windows_and_unselected_instances_never_queue_starts() {
    let mut ui = Ui::new(Session::default());
    ui.rows.push(row("proxy", Kind::Proxy, None));
    let launch = Launch::detached(crate::windows::View::Dashboard, None, Kind::Proxy);
    enable_all(&mut ui);
    let mut detached = Instances::new(&launch);
    assert!(next(&mut detached, &ui, true).is_none());
    ui.details.clear();
    let mut disabled = Instances::new(&Launch::default());
    assert!(next(&mut disabled, &ui, true).is_none());
}

#[test]
fn only_configs_with_auto_start_are_selected_including_an_independent_bound_rule() {
    let mut ui = Ui::new(Session::default());
    ui.rows = vec![
        row("missing-details", Kind::Proxy, None),
        row("default-off", Kind::Proxy, None),
        row("unavailable", Kind::Proxy, None),
        row("rule", Kind::Rule, None),
        row("server", Kind::Server, None),
    ];
    ui.details
        .insert("default-off".into(), InstanceDetails::default());
    ui.details.insert(
        "unavailable".into(),
        InstanceDetails {
            auto_start: true,
            config_unavailable: true,
            ..Default::default()
        },
    );
    ui.details.insert(
        "rule".into(),
        InstanceDetails {
            auto_start: true,
            bindings: vec!["default-off".into()],
            ..Default::default()
        },
    );
    ui.details.insert(
        "server".into(),
        InstanceDetails {
            auto_start: true,
            ..Default::default()
        },
    );
    let mut startup = Instances::new(&Launch::default());
    // Selecting only the rule must still dispatch it; the manager owns bindings.
    assert_start(next(&mut startup, &ui, true), "rule");
    assert_start(next(&mut startup, &ui, true), "server");
    assert!(next(&mut startup, &ui, true).is_none());
    ui.details.get_mut("default-off").unwrap().auto_start = true;
    assert!(next(&mut startup, &ui, true).is_none());
    let mut reopened = Instances::new(&Launch::default());
    ui.details.get_mut("rule").unwrap().auto_start = false;
    assert_start(next(&mut reopened, &ui, true), "default-off");
}

#[test]
fn rechecks_running_and_removed_instances_before_dispatch() {
    let mut ui = Ui::new(Session::default());
    ui.rows = vec![
        row("first", Kind::Proxy, None),
        row("bound", Kind::Rule, None),
        row("removed", Kind::Proxy, None),
        row("last", Kind::Server, None),
    ];
    enable_all(&mut ui);
    let mut startup = Instances::new(&Launch::default());
    assert_start(next(&mut startup, &ui, true), "first");
    ui.rows[1].pid = Some(24);
    ui.rows.retain(|row| row.instance.name != "removed");
    assert_start(next(&mut startup, &ui, true), "last");
    assert!(next(&mut startup, &ui, true).is_none());
}

#[test]
fn disabling_cancels_pending_starts_and_reenabling_does_not_rearm() {
    let mut ui = Ui::new(Session::default());
    ui.rows = vec![
        row("first", Kind::Proxy, None),
        row("second", Kind::Rule, None),
    ];
    enable_all(&mut ui);
    let mut startup = Instances::new(&Launch::default());
    assert_start(next(&mut startup, &ui, true), "first");
    ui.details.get_mut("second").unwrap().auto_start = false;
    assert!(next(&mut startup, &ui, true).is_none());
    ui.details.get_mut("second").unwrap().auto_start = true;
    assert!(next(&mut startup, &ui, true).is_none());
}

#[test]
fn a_recovery_prompt_pauses_pending_starts_until_dismissed() {
    let mut ui = Ui::new(Session::default());
    ui.rows = vec![
        row("first", Kind::Proxy, None),
        row("second", Kind::Rule, None),
    ];
    enable_all(&mut ui);
    let mut startup = Instances::new(&Launch::default());
    assert_start(next(&mut startup, &ui, true), "first");
    ui.fail(
        "Invalid saved configuration",
        Resume::Request(Action::Start("first".into())),
    );
    assert!(next(&mut startup, &ui, true).is_none());
    ui.recovery = None;
    assert_start(next(&mut startup, &ui, true), "second");
}

#[test]
fn prioritizes_selected_rule_plans_and_does_not_retry_them_through_bound_proxies() {
    let mut ui = Ui::new(Session::default());
    ui.rows = vec![
        row("rule", Kind::Rule, None),
        row("proxy", Kind::Proxy, None),
        row("opted-out-proxy", Kind::Proxy, None),
    ];
    ui.details.insert(
        "rule".into(),
        InstanceDetails {
            bindings: vec!["proxy".into(), "opted-out-proxy".into()],
            ..Default::default()
        },
    );
    for state in [
        "Unreachable",
        "Unresponsive",
        "Exited: 1",
        "Session recovery required",
    ] {
        let mut row = row(state, Kind::Proxy, None);
        row.state = state.into();
        ui.rows.push(row);
    }
    enable_all(&mut ui);
    ui.details.get_mut("opted-out-proxy").unwrap().auto_start = false;
    let mut startup = Instances::new(&Launch::default());
    // The rule's normal plan includes both proxies, including its opted-out
    // dependency. Starting only "proxy" would omit the other bound proxy.
    assert_start(next(&mut startup, &ui, true), "rule");
    // A failed rule plan must not be retried by starting "proxy" independently.
    assert!(next(&mut startup, &ui, true).is_none());
}

#[test]
fn a_rule_disabled_before_dispatch_does_not_hide_a_selected_proxy() {
    let mut ui = Ui::new(Session::default());
    ui.rows = vec![
        row("rule", Kind::Rule, None),
        row("proxy", Kind::Proxy, None),
    ];
    enable_all(&mut ui);
    ui.details.get_mut("rule").unwrap().bindings = vec!["proxy".into()];
    let mut startup = Instances::new(&Launch::default());
    assert!(startup.prepare(&ui, true));
    ui.details.get_mut("rule").unwrap().auto_start = false;
    assert_start(next(&mut startup, &ui, true), "proxy");
    assert!(next(&mut startup, &ui, true).is_none());
}

#[test]
fn a_rule_binding_removed_before_dispatch_keeps_the_selected_proxy() {
    let mut ui = Ui::new(Session::default());
    ui.rows = vec![
        row("rule", Kind::Rule, None),
        row("proxy", Kind::Proxy, None),
    ];
    enable_all(&mut ui);
    ui.details.get_mut("rule").unwrap().bindings = vec!["proxy".into()];
    let mut startup = Instances::new(&Launch::default());
    assert!(startup.prepare(&ui, true));
    ui.details.get_mut("rule").unwrap().bindings.clear();
    assert_start(next(&mut startup, &ui, true), "proxy");
    assert_start(next(&mut startup, &ui, true), "rule");
}

#[test]
fn new_rule_bindings_are_prioritized_before_a_proxy_can_start_only_part_of_the_plan() {
    let mut ui = Ui::new(Session::default());
    ui.rows = vec![
        row("proxy", Kind::Proxy, None),
        row("rule", Kind::Rule, None),
    ];
    enable_all(&mut ui);
    let mut startup = Instances::new(&Launch::default());
    assert!(startup.prepare(&ui, true));
    ui.details.get_mut("rule").unwrap().bindings = vec!["proxy".into(), "opted-out-proxy".into()];
    assert_start(next(&mut startup, &ui, true), "rule");
    assert!(next(&mut startup, &ui, true).is_none());
}

#[test]
fn configuration_review_pauses_remaining_starts() {
    let mut ui = Ui::new(Session::default());
    ui.rows = vec![
        row("first", Kind::Proxy, None),
        row("second", Kind::Proxy, None),
    ];
    enable_all(&mut ui);
    let mut startup = Instances::new(&Launch::default());
    assert_start(next(&mut startup, &ui, true), "first");
    ui.editor = Some(Editor::new("first".into(), "standalone=true\n".into()));
    assert!(next(&mut startup, &ui, true).is_none());
    ui.editor = None;
    assert_start(next(&mut startup, &ui, true), "second");
}

fn snapshot(rows: Vec<Row>) -> astra_management::v2::ResponseBody {
    use astra_management::v2 as v;
    v::ResponseBody::Instances(
        rows.into_iter()
            .map(|row| v::Instance {
                id: row.instance.name.clone(),
                name: row.instance.name,
                kind: match row.instance.kind {
                    Kind::Proxy => v::InstanceKind::Proxy,
                    Kind::Rule => v::InstanceKind::Rule,
                    Kind::Server => v::InstanceKind::Server,
                },
                config: row.instance.config,
                pid: row.pid,
                state: row.state,
                uptime_secs: "0".into(),
                capabilities: vec![],
                changed: false,
                details: Some(InstanceDetails {
                    auto_start: true,
                    ..Default::default()
                }),
            })
            .collect(),
    )
}

async fn wait_refresh(startup: &mut Instances) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !startup.refresh.as_ref().unwrap().is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn refresh_after_each_start_skips_bound_instances_even_when_ui_feed_is_stale() {
    use astra_management::v2 as v;
    let fixture = crate::import::tests::Fixture::new();
    let socket = fixture.0.join("management.sock");
    let listener = astra_platform::platform::LocalListener::bind(&socket).unwrap();
    let initial = vec![
        row("first", Kind::Proxy, None),
        row("bound", Kind::Rule, None),
    ];
    let snapshots = [
        initial.clone(),
        vec![
            row("first", Kind::Proxy, Some(1)),
            row("bound", Kind::Rule, Some(2)),
        ],
    ];
    let server = tokio::spawn(async move {
        for rows in snapshots {
            let (stream, request) = test_rpc::next(&listener).await;
            assert!(matches!(
                request.body,
                v::RequestBody::Query(v::Query::Instances)
            ));
            test_rpc::reply(stream, request, snapshot(rows)).await;
        }
    });
    let mut ui = Ui::new(Session::default());
    ui.rows = initial;
    enable_all(&mut ui);
    let mut startup = Instances::new(&Launch::default());
    assert!(startup.poll(&mut ui, &socket, true).await.is_none());
    wait_refresh(&mut startup).await;
    assert_start(startup.poll(&mut ui, &socket, true).await, "first");
    // The normal job has completed, but the feed still has both stopped rows.
    assert!(startup.poll(&mut ui, &socket, true).await.is_none());
    wait_refresh(&mut startup).await;
    assert!(startup.poll(&mut ui, &socket, true).await.is_none());
    assert!(startup.pending.is_empty());
    assert!(ui.rows.iter().all(|row| row.pid.is_none()));
    server.await.unwrap();
    astra_platform::platform::remove_socket(&socket);
}

#[tokio::test]
async fn refresh_failure_cancels_remaining_startup_requests() {
    use astra_management::v2 as v;
    let fixture = crate::import::tests::Fixture::new();
    let socket = fixture.0.join("management.sock");
    let listener = astra_platform::platform::LocalListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (stream, request) = test_rpc::next(&listener).await;
        assert!(matches!(
            request.body,
            v::RequestBody::Query(v::Query::Instances)
        ));
        test_rpc::error(stream, request, "manager unavailable").await;
    });
    let mut ui = Ui::new(Session::default());
    ui.rows = vec![
        row("first", Kind::Proxy, None),
        row("second", Kind::Proxy, None),
    ];
    enable_all(&mut ui);
    let mut startup = Instances::new(&Launch::default());
    assert!(startup.poll(&mut ui, &socket, true).await.is_none());
    wait_refresh(&mut startup).await;
    assert!(startup.poll(&mut ui, &socket, true).await.is_none());
    assert!(startup.pending.is_empty());
    assert!(ui.recovery.is_some());
    assert!(ui
        .recovery
        .as_ref()
        .unwrap()
        .message
        .contains("manager unavailable"));
    ui.recovery = None;
    assert!(startup.poll(&mut ui, &socket, true).await.is_none());
    assert!(startup.refresh.is_none());
    server.await.unwrap();
    astra_platform::platform::remove_socket(&socket);
}

#[tokio::test]
async fn refreshed_config_cancels_start_even_when_the_ui_still_shows_auto_start() {
    use astra_management::v2 as v;
    let fixture = crate::import::tests::Fixture::new();
    let socket = fixture.0.join("management.sock");
    let listener = astra_platform::platform::LocalListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (stream, request) = test_rpc::next(&listener).await;
        let mut response = snapshot(vec![row("proxy", Kind::Proxy, None)]);
        if let v::ResponseBody::Instances(rows) = &mut response {
            rows[0].details.as_mut().unwrap().auto_start = false;
        }
        test_rpc::reply(stream, request, response).await;
    });
    let mut ui = Ui::new(Session::default());
    ui.rows = vec![row("proxy", Kind::Proxy, None)];
    enable_all(&mut ui);
    let mut startup = Instances::new(&Launch::default());
    assert!(startup.poll(&mut ui, &socket, true).await.is_none());
    wait_refresh(&mut startup).await;
    assert!(startup.poll(&mut ui, &socket, true).await.is_none());
    assert!(startup.pending.is_empty());
    assert!(ui.details["proxy"].auto_start);
    server.await.unwrap();
    astra_platform::platform::remove_socket(&socket);
}

fn silent_context(home: &std::path::Path) -> crate::windows::Context {
    crate::windows::Context {
        executable: home.join(astra_platform::platform::executable("astra-tui")),
        home: home.into(),
        config: home.join("config"),
        bin: home.join("bin"),
        language: crate::i18n::Language::English,
    }
}

#[test]
fn startup_log_creates_missing_state_directory_for_early_errors() {
    let fixture = crate::import::tests::Fixture::new();
    let home = fixture.0.join("new").join("state");
    log_startup(&home, "Initialization failed").unwrap();
    assert!(std::fs::read_to_string(home.join("startup.log"))
        .unwrap()
        .contains("Initialization failed"));
}

#[tokio::test]
async fn silent_start_ignores_ui_preferences_and_session_when_no_instances_opt_in() {
    use astra_management::v2 as v;
    let fixture = crate::import::tests::Fixture::new();
    let session = "this saved session is not read in silent mode";
    fixture.write("ui-session.json", session);
    fixture.write(settings::FILE, "not valid json");
    let socket = fixture.0.join("management.sock");
    let listener = astra_platform::platform::LocalListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (stream, request) = test_rpc::next(&listener).await;
        assert!(matches!(
            request.body,
            v::RequestBody::Query(v::Query::Instances)
        ));
        let mut response = snapshot(vec![row("default-off", Kind::Proxy, None)]);
        if let v::ResponseBody::Instances(rows) = &mut response {
            rows[0].details = Some(InstanceDetails::default());
        }
        test_rpc::reply(stream, request, response).await;
    });
    run_silent(socket.clone(), silent_context(&fixture.0))
        .await
        .unwrap();
    server.await.unwrap();
    let log = std::fs::read_to_string(fixture.0.join("startup.log")).unwrap();
    assert!(log.contains("No stopped instances have auto_start enabled"));
    assert_eq!(
        std::fs::read_to_string(fixture.0.join("ui-session.json")).unwrap(),
        session
    );
    assert_eq!(
        std::fs::read_to_string(fixture.0.join(settings::FILE)).unwrap(),
        "not valid json"
    );
    astra_platform::platform::remove_socket(&socket);
}

#[tokio::test]
async fn silent_start_rechecks_dependencies_and_never_falls_back_to_interactive_commands() {
    use astra_management::v2 as v;
    use std::sync::{Arc, Mutex};
    let fixture = crate::import::tests::Fixture::new();
    fixture.write(settings::FILE, r#"{"auto_start_instances":false}"#);
    let socket = fixture.0.join("management.sock");
    let listener = astra_platform::platform::LocalListener::bind(&socket).unwrap();
    let starts = Arc::new(Mutex::new(Vec::new()));
    let recorded = starts.clone();
    let server = tokio::spawn(async move {
        let rows = Arc::new(Mutex::new(vec![
            row("already-running", Kind::Proxy, Some(9)),
            row("default-off", Kind::Proxy, None),
            row("first", Kind::Proxy, None),
            row("disabled-later", Kind::Proxy, None),
            row("bound-rule", Kind::Rule, None),
            row("unsupported", Kind::Proxy, None),
            row("last", Kind::Server, None),
        ]));
        let mut peers = tokio::task::JoinSet::new();
        loop {
            // execute_id polls progress on a separate connection while a
            // command is in flight. Like the real manager, accept and read
            // those peers concurrently rather than blocking on one payload.
            let (mut stream, _) = tokio::select! {
                peer = listener.accept() => peer.unwrap(),
                done = peers.join_next(), if !peers.is_empty() => {
                    done.unwrap().unwrap();
                    continue;
                }
            };
            let rows = rows.clone();
            let recorded = recorded.clone();
            peers.spawn(async move {
                let request: v::Request = astra_management::client::receive(&mut stream).await.unwrap();
                let response = match &request.body {
                    v::RequestBody::Query(v::Query::Instances) => {
                        let rows = rows.lock().unwrap();
                        let mut response = snapshot(rows.clone());
                        if let v::ResponseBody::Instances(instances) = &mut response {
                            for instance in instances {
                                if instance.name == "default-off"
                                    || (instance.name == "disabled-later"
                                        && rows.iter().any(|row| row.instance.name == "first" && row.pid.is_some()))
                                {
                                    instance.details.as_mut().unwrap().auto_start = false;
                                }
                            }
                        }
                        Ok(response)
                    }
                    v::RequestBody::Query(v::Query::Events { .. }) => {
                        Ok(v::ResponseBody::Events(v::EventBatch {
                            events: vec![], cursor: "0".into(), resync_required: false,
                        }))
                    }
                    v::RequestBody::Execute { command: v::Command::StartSilent { instance }, .. } => {
                        recorded.lock().unwrap().push(instance.clone());
                        if instance == "unsupported" {
                            Err("Unsupported start_silent: update manager")
                        } else {
                            for row in rows.lock().unwrap().iter_mut() {
                                if row.instance.name == *instance || (instance == "first" && row.instance.name == "bound-rule") {
                                    row.pid = Some(11);
                                    row.state = "Running".into();
                                }
                            }
                            Ok(v::ResponseBody::Done { message: "Started".into() })
                        }
                    }
                    other => panic!("Silent mode must not prepare/create configs or use interactive starts: {other:?}"),
                };
                match response {
                    Ok(response) => test_rpc::reply(stream, request, response).await,
                    Err(message) => test_rpc::error(stream, request, message).await,
                }
            });
        }
    });
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        run_silent(socket.clone(), silent_context(&fixture.0)),
    )
    .await;
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
    let error = result.unwrap().unwrap_err();
    assert!(error.to_string().contains("Unsupported start_silent"));
    assert_eq!(*starts.lock().unwrap(), ["first", "unsupported", "last"]);
    let log = std::fs::read_to_string(fixture.0.join("startup.log")).unwrap();
    assert!(log.contains("Started first"));
    assert!(log.contains("Failed unsupported"));
    assert!(log.contains("Started last"));
    assert!(log.contains("Skipped bound-rule"));
    assert!(log.contains("Skipped disabled-later"));
    assert!(!log.contains("Starting default-off"));
    assert!(!fixture.0.join("ui-session.json").exists());
    assert!(!fixture.0.join("unsupported.toml").exists());
    astra_platform::platform::remove_socket(&socket);
}
