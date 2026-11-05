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

fn enabled_ui() -> Ui {
    let mut ui = Ui::new(Session::default());
    ui.preferences.auto_start_instances = true;
    ui
}

fn next(startup: &mut Instances, ui: &Ui, ready: bool) -> Option<Action> {
    startup
        .prepare(ui, ready)
        .then(|| startup.next_action(&ui.rows))
        .flatten()
}

fn assert_start(action: Option<Action>, name: &str) {
    assert!(matches!(action, Some(Action::Start(actual)) if actual == name));
}

#[test]
fn waits_for_initial_snapshot_and_only_starts_stopped_instances_once() {
    let mut ui = enabled_ui();
    let mut startup = Instances::new(true, &Launch::default());
    assert!(next(&mut startup, &ui, false).is_none());
    ui.rows = vec![
        row("running", Kind::Proxy, Some(12)),
        row("proxy", Kind::Proxy, None),
        row("rule", Kind::Rule, None),
        row("server", Kind::Server, None),
    ];
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
fn detached_windows_and_disabled_launches_never_queue_starts() {
    let mut ui = enabled_ui();
    ui.rows.push(row("proxy", Kind::Proxy, None));
    let launch = Launch::detached(crate::windows::View::Dashboard, None, Kind::Proxy);
    let mut detached = Instances::new(true, &launch);
    assert!(next(&mut detached, &ui, true).is_none());
    let mut disabled = Instances::new(false, &Launch::default());
    assert!(next(&mut disabled, &ui, true).is_none());
}

#[test]
fn rechecks_running_and_removed_instances_before_dispatch() {
    let mut ui = enabled_ui();
    ui.rows = vec![
        row("first", Kind::Proxy, None),
        row("bound", Kind::Rule, None),
        row("removed", Kind::Proxy, None),
        row("last", Kind::Server, None),
    ];
    let mut startup = Instances::new(true, &Launch::default());
    assert_start(next(&mut startup, &ui, true), "first");
    ui.rows[1].pid = Some(24);
    ui.rows.retain(|row| row.instance.name != "removed");
    assert_start(next(&mut startup, &ui, true), "last");
    assert!(next(&mut startup, &ui, true).is_none());
}

#[test]
fn disabling_cancels_pending_starts_and_reenabling_does_not_rearm() {
    let mut ui = enabled_ui();
    ui.rows = vec![
        row("first", Kind::Proxy, None),
        row("second", Kind::Rule, None),
    ];
    let mut startup = Instances::new(true, &Launch::default());
    assert_start(next(&mut startup, &ui, true), "first");
    ui.preferences.auto_start_instances = false;
    assert!(next(&mut startup, &ui, true).is_none());
    ui.preferences.auto_start_instances = true;
    assert!(next(&mut startup, &ui, true).is_none());
}

#[test]
fn a_recovery_prompt_pauses_pending_starts_until_dismissed() {
    let mut ui = enabled_ui();
    ui.rows = vec![
        row("first", Kind::Proxy, None),
        row("second", Kind::Rule, None),
    ];
    let mut startup = Instances::new(true, &Launch::default());
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
fn skips_unsafe_states_and_bound_rule_roots_that_would_retry_proxies() {
    let mut ui = enabled_ui();
    ui.rows = vec![
        row("rule", Kind::Rule, None),
        row("proxy", Kind::Proxy, None),
    ];
    ui.details.insert(
        "rule".into(),
        InstanceDetails {
            bindings: vec!["proxy".into()],
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
    let mut startup = Instances::new(true, &Launch::default());
    assert_start(next(&mut startup, &ui, true), "proxy");
    // A failed proxy remains stopped; starting its bound rule would retry it.
    assert!(next(&mut startup, &ui, true).is_none());
}

#[test]
fn configuration_review_pauses_remaining_starts() {
    let mut ui = enabled_ui();
    ui.rows = vec![
        row("first", Kind::Proxy, None),
        row("second", Kind::Proxy, None),
    ];
    let mut startup = Instances::new(true, &Launch::default());
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
                details: None,
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
    let mut ui = enabled_ui();
    ui.rows = initial;
    let mut startup = Instances::new(true, &Launch::default());
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
    let mut ui = enabled_ui();
    ui.rows = vec![
        row("first", Kind::Proxy, None),
        row("second", Kind::Proxy, None),
    ];
    let mut startup = Instances::new(true, &Launch::default());
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

fn silent_context(home: &std::path::Path) -> crate::windows::Context {
    crate::windows::Context {
        executable: home.join("astra-tui.exe"),
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
async fn silent_start_disabled_does_not_connect_or_restore_a_ui_session() {
    let fixture = crate::import::tests::Fixture::new();
    fixture.write(
        "ui-session.json",
        "this saved session is not read in silent mode",
    );
    run_silent(
        fixture.0.join("no-manager.sock"),
        silent_context(&fixture.0),
    )
    .await
    .unwrap();
    let log = std::fs::read_to_string(fixture.0.join("startup.log")).unwrap();
    assert!(log.contains("Automatic instance startup is disabled"));
    assert_eq!(
        std::fs::read_to_string(fixture.0.join("ui-session.json")).unwrap(),
        "this saved session is not read in silent mode"
    );
}

#[tokio::test]
async fn silent_start_logs_malformed_preferences_instead_of_falling_back() {
    let fixture = crate::import::tests::Fixture::new();
    fixture.write(settings::FILE, "not valid json");
    let error = run_silent(
        fixture.0.join("no-manager.sock"),
        silent_context(&fixture.0),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("cannot load preferences"));
    let log = std::fs::read_to_string(fixture.0.join("startup.log")).unwrap();
    assert!(log.contains("Silent startup error"));
    assert!(!fixture.0.join("ui-session.json").exists());
}

#[tokio::test]
async fn silent_start_rechecks_dependencies_and_never_falls_back_to_interactive_commands() {
    use astra_management::v2 as v;
    use std::sync::{Arc, Mutex};
    let fixture = crate::import::tests::Fixture::new();
    fixture.write(settings::FILE, r#"{"auto_start_instances":true}"#);
    let socket = fixture.0.join("management.sock");
    let listener = astra_platform::platform::LocalListener::bind(&socket).unwrap();
    let starts = Arc::new(Mutex::new(Vec::new()));
    let recorded = starts.clone();
    let server = tokio::spawn(async move {
        let rows = Arc::new(Mutex::new(vec![
            row("already-running", Kind::Proxy, Some(9)),
            row("first", Kind::Proxy, None),
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
                        Ok(snapshot(rows.lock().unwrap().clone()))
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
    assert!(!fixture.0.join("ui-session.json").exists());
    assert!(!fixture.0.join("unsupported.toml").exists());
    astra_platform::platform::remove_socket(&socket);
}
