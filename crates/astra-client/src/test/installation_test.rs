use super::*;
fn fixture() -> Client {
    let mut ui = Client::new(crate::protocol::Kind::Proxy);
    ui.installation.open = true;
    let destination = v::InstallationDestination {
        directory: Some("/home/user/.local/share/astra/client".into()),
        entry: Some("/home/user/.local/bin/astra-tui".into()),
        installed: false,
        requires_elevation: false,
        unavailable_reason: None,
    };
    ui.installation.status = Some(v::InstallationStatus {
        target: "selected-agent".into(),
        account: "selected-user".into(),
        packages: vec![v::InstallationPackage {
            id: "package-client".into(),
            role: "client".into(),
            release_id: "release-1".into(),
            source: "/remote/current-client".into(),
            user: destination.clone(),
            system: v::InstallationDestination {
                directory: Some("/opt/astra/client".into()),
                requires_elevation: true,
                ..destination
            },
        }],
        unavailable_roles: [
            ("server".into(), "No server bundle".into()),
            ("controller".into(), "No controller bundle".into()),
            ("web".into(), "No web bundle".into()),
        ]
        .into(),
        warnings: vec![],
    });
    ui
}

async fn next(
    listener: &astra_platform::platform::LocalListener,
) -> (astra_platform::platform::LocalStream, v::Request) {
    let (mut stream, _) = listener.accept().await.unwrap();
    let request = astra_management::client::receive(&mut stream)
        .await
        .unwrap();
    (stream, request)
}
async fn reply(
    mut stream: astra_platform::platform::LocalStream,
    request: v::Request,
    body: v::ResponseBody,
) {
    astra_management::client::send(&mut stream, &v::Response::success(request.id, body))
        .await
        .unwrap();
}
#[test]
fn first_notice_is_dismissible_and_remembered_per_ui_kind_and_target() {
    let mut ui = fixture();
    ui.installation.open = false;
    ui.installation.notice = true;
    ui.installation.notice_key = Some("client:target-a".into());
    ui.installation_action(InstallationAction::Dismiss);
    assert!(!ui.installation.visible());
    assert!(ui
        .preferences
        .installation_notices
        .contains("client:target-a"));
    assert!(!ui
        .preferences
        .installation_notices
        .contains("server:target-a"));
    assert!(!ui
        .preferences
        .installation_notices
        .contains("client:target-b"));
    ui.installation.notice = true;
    ui.installation.notice_accept = true;
    ui.installation.notice_key = Some("server:target-a".into());
    ui.installation_action(InstallationAction::Confirm);
    assert!(ui.installation.open);
    assert_eq!(ui.page, crate::navigation::SETTINGS);
    assert!(ui
        .preferences
        .installation_notices
        .contains("server:target-a"));
    let saved = serde_json::to_string(&ui.preferences).unwrap();
    let restored: crate::preferences::Preferences = serde_json::from_str(&saved).unwrap();
    assert_eq!(
        restored.installation_notices,
        ui.preferences.installation_notices
    );
}

#[test]
fn detached_and_resumed_windows_do_not_schedule_first_notice() {
    let context = crate::windows::Context {
        executable: "astra-tui".into(),
        home: "state".into(),
        config: "config".into(),
        bin: "bin".into(),
        language: crate::i18n::Language::English,
    };
    for (launch, resumed) in [
        (
            crate::windows::Launch {
                window_id: Some("detached".into()),
                ..Default::default()
            },
            false,
        ),
        (crate::windows::Launch::default(), true),
    ] {
        let mut ui = Client::new(crate::protocol::Kind::Proxy);
        ui.initialize_installation(
            &context,
            std::path::Path::new("management.sock"),
            &launch,
            resumed,
        );
        assert!(ui.installation.notice_key.is_none());
        assert!(!ui.installation.refresh);
    }
}

#[tokio::test]
async fn supported_target_reminds_once_even_without_a_complete_package() {
    let mut ui = fixture();
    ui.installation.open = false;
    ui.installation.notice_key = Some("client:incomplete-target".into());
    ui.installation.status.as_mut().unwrap().packages.clear();
    assert!(
        ui.poll_installation(std::path::Path::new("unused.sock"), true)
            .await
    );
    assert!(ui.installation.notice);
    assert!(ui.installation.query.is_none());
    ui.installation_action(InstallationAction::Dismiss);
    assert!(
        !ui.poll_installation(std::path::Path::new("unused.sock"), true)
            .await
    );
    assert!(!ui.installation.notice);
    assert!(ui
        .preferences
        .installation_notices
        .contains("client:incomplete-target"));
}

#[tokio::test]
async fn old_target_does_not_prompt_or_query_unsupported_installation() {
    let fixture = crate::import::tests::Fixture::new();
    let socket = fixture.0.join("management.sock");
    let listener = astra_platform::platform::LocalListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (stream, request) = next(&listener).await;
        assert!(matches!(
            request.body,
            v::RequestBody::Query(v::Query::Hello)
        ));
        reply(
            stream,
            request,
            v::ResponseBody::Hello(v::Hello {
                version: 2,
                role: "agent".into(),
                epoch: "test".into(),
                build_id: "old-build".into(),
                capabilities: vec![],
            }),
        )
        .await;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
    });
    let error = read_status(socket.clone()).await.unwrap_err();
    let mut ui = Client::new(crate::protocol::Kind::Proxy);
    ui.installation.notice_key = Some("client:old-target".into());
    ui.installation.query_error = Some(error);
    assert!(!ui.poll_installation(&socket, true).await);
    assert!(!ui.installation.visible());
    assert!(ui.message.is_empty());
    server.await.unwrap();
}

#[tokio::test]
async fn semantic_request_blocks_stale_status_and_ready_poll_never_waits() {
    let mut ui = fixture();
    ui.installation.refresh = true;
    assert!(matches!(ui.installation_request(), Input::Idle));
    ui.installation.refresh = false;
    let (finish, pending) = tokio::sync::oneshot::channel();
    ui.installation.query = Some(tokio::spawn(async move { pending.await.unwrap() }));
    assert!(matches!(ui.installation_request(), Input::Idle));
    assert!(!ui.poll_installation_ready(std::path::Path::new("unused.sock"), true));
    finish.send(Err("target disconnected".into())).unwrap();
    while !ui.installation.query.as_ref().unwrap().is_finished() {
        tokio::task::yield_now().await;
    }
    assert!(ui.poll_installation_ready(std::path::Path::new("unused.sock"), true));
    assert!(matches!(ui.installation_request(), Input::Idle));
    ui.installation.query_error = None;
    assert!(matches!(ui.installation_request(), Input::Install(_)));
}

#[test]
fn installation_details_include_missing_roles_and_selected_destination() {
    crate::i18n::with_language(crate::i18n::Language::English, || {
        let mut client = fixture();
        client.installation.scope = v::InstallationScope::System;
        let details = client.installation.details();
        assert!(matches!(
            details[0].kind,
            DetailKind::Package {
                index: 0,
                selected: true
            }
        ));
        let text = details
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("server · Unavailable: No server bundle"));
        assert!(text.contains("/opt/astra/client"));
        assert!(text.contains("Administrator permission is required for this location."));
        assert_eq!(client.installation.scope_label(), "All users");
        assert_eq!(
            client.installation.target_label(),
            "selected-agent · selected-user"
        );
    });
}

#[tokio::test]
async fn successful_installation_freezes_the_package_for_launch_not_the_current_selection() {
    let mut ui = fixture();
    let package = ui.installation.selected().unwrap().clone();
    ui.installation.pending_package = Some((package.clone(), v::InstallationScope::User));
    ui.installation.operation = Some(tokio::spawn(async {
        Ok(v::InstallationResult {
            role: "client".into(),
            directory: "/installed/new".into(),
            entry: "/installed/entry".into(),
            already_installed: false,
            message: "Installed".into(),
            launch_command: "never execute IPC shell text".into(),
        })
    }));
    // Changing the panel while installation runs must not redirect the launch.
    ui.installation.status.as_mut().unwrap().packages[0].source = "/different/source".into();
    ui.installation.scope = v::InstallationScope::System;
    while !ui.installation.operation.as_ref().unwrap().is_finished() {
        tokio::task::yield_now().await;
    }
    ui.poll_installation_ready(std::path::Path::new("unused.sock"), true);
    let completed = ui.installation.completed.as_ref().unwrap();
    assert_eq!(completed.source, package.source);
    assert_eq!(completed.release_id, package.release_id);
    assert_eq!(completed.scope, v::InstallationScope::User);
    assert_eq!(completed.directory, PathBuf::from("/installed/new"));
    assert_eq!(ui.installation.percent(), Some(100));
    assert_eq!(ui.installation.activation, Some(Activation::Waiting));
    ui.installation.query.take().unwrap().abort();
}

#[tokio::test]
async fn failed_installation_never_arms_a_launch() {
    let mut ui = fixture();
    ui.installation.pending_package = Some((
        ui.installation.selected().unwrap().clone(),
        v::InstallationScope::User,
    ));
    ui.installation.operation = Some(tokio::spawn(async { Err("copy failed".into()) }));
    while !ui.installation.operation.as_ref().unwrap().is_finished() {
        tokio::task::yield_now().await;
    }
    ui.poll_installation_ready(std::path::Path::new("unused.sock"), true);
    assert!(ui.installation.completed.is_none());
    assert!(!ui.update_launch.pending());
    ui.installation.query.take().unwrap().abort();
}

#[test]
fn progress_tracks_steps_percent_and_failed_step() {
    let mut ui = fixture();
    assert!(!ui.installation.progress_visible());
    let (sender, receiver) = tokio::sync::watch::channel(None);
    ui.installation.progress = Some(receiver);
    let send = |step, index, current: u64, total: u64| {
        let payload = v::InstallationProgress {
            step,
            step_index: index,
            step_count: 5,
            current: current.to_string(),
            total: total.to_string(),
        };
        sender.send_replace(Some(astra_ipc::progress::Progress::new(
            "installation_event",
            serde_json::to_string(&payload).unwrap(),
        )));
    };
    send(
        v::InstallationStep::Copying,
        1,
        1024 * 1024,
        4 * 1024 * 1024,
    );
    let socket = std::path::Path::new("/nonexistent");
    ui.installation.refresh = false;
    assert!(ui.poll_installation_ready(socket, false));
    assert!(ui.installation.progress_visible());
    assert_eq!(ui.installation.percent(), Some(25));
    assert!(ui
        .installation
        .progress_label()
        .contains("1.0 MiB / 4.0 MiB"));
    let states = ui
        .installation
        .step_rows()
        .iter()
        .map(|row| row.state)
        .collect::<Vec<_>>();
    assert_eq!(
        states,
        [
            StepState::Done,
            StepState::Active,
            StepState::Pending,
            StepState::Pending,
            StepState::Pending
        ]
    );
    // A stale earlier step never moves progress backwards.
    send(v::InstallationStep::Preparing, 0, 0, 0);
    ui.poll_installation_ready(socket, false);
    assert_eq!(ui.installation.percent(), Some(25));
    // Unrelated operation events are ignored.
    sender.send_replace(Some(astra_ipc::progress::Progress::new(
        "operation_started",
        "Executing",
    )));
    ui.poll_installation_ready(socket, false);
    assert_eq!(
        ui.installation.latest.unwrap().step,
        v::InstallationStep::Copying
    );
    ui.installation.error = Some("copy failed".into());
    assert_eq!(ui.installation.step_rows()[1].state, StepState::Failed);
}

#[test]
fn byte_sizes_are_human_readable() {
    assert_eq!(format_bytes(512), "512 B");
    assert_eq!(format_bytes(1536), "1.5 KiB");
    assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
}

#[tokio::test]
async fn opened_package_wins_over_a_resident_old_manager_and_uses_its_own_helper() {
    let root = crate::import::tests::Fixture::new();
    let local = crate::package_choice::tests::package(&root.0.join("new"), b"new payload");
    let socket = root.0.join("management.sock");
    let listener = astra_platform::platform::LocalListener::bind(&socket).unwrap();
    let old_status = fixture().installation.status.unwrap();
    let server = tokio::spawn(async move {
        let (stream, request) = next(&listener).await;
        reply(
            stream,
            request,
            v::ResponseBody::Hello(v::Hello {
                version: 2,
                role: "agent".into(),
                epoch: "old".into(),
                build_id: "old".into(),
                capabilities: vec!["installation".into()],
            }),
        )
        .await;
        let (stream, request) = next(&listener).await;
        assert!(matches!(
            request.body,
            v::RequestBody::Query(v::Query::InstallationStatus)
        ));
        reply(
            stream,
            request,
            v::ResponseBody::InstallationStatus(old_status),
        )
        .await;
        // Preparing a local system install must not ask the old manager to choose a package.
        assert!(
            tokio::time::timeout(Duration::from_millis(150), listener.accept())
                .await
                .is_err()
        );
    });
    let catalog = read_catalog(socket.clone(), Some(local.directory.clone()), None)
        .await
        .unwrap();
    let mut ui = Client::new(crate::protocol::Kind::Proxy);
    ui.installation.accept_catalog(catalog);
    assert_eq!(ui.installation.selected().unwrap().id, local.id());
    let Input::Install(mut request) = ui.installation_request() else {
        panic!("local install")
    };
    assert!(matches!(&request.origin, Origin::FrontendPackage(p) if p == &local));
    request.scope = v::InstallationScope::System;
    // Selection changes after authorization cannot redirect the frozen request.
    ui.installation.selected = 1;
    let launch = prepare_elevation(&socket, &request).await.unwrap();
    assert_eq!(
        launch.program,
        local
            .directory
            .join(astra_platform::platform::executable("astra-host"))
    );
    assert_eq!(launch.arguments[2], local.release_id);
    assert_eq!(launch.arguments[3], local.manifest_sha256);
    server.await.unwrap();
}

#[tokio::test]
async fn changed_local_source_fails_without_falling_back_to_the_manager() {
    let root = crate::import::tests::Fixture::new();
    let package = crate::package_choice::tests::package(&root.0.join("source"), b"initial");
    crate::package_choice::tests::package(&package.directory, b"replaced");
    let result = read_catalog(
        root.0.join("unused.sock"),
        Some(package.directory.clone()),
        Some(package),
    )
    .await;
    assert!(result.unwrap_err().contains("selected package changed"));
}

#[tokio::test]
async fn local_package_remains_installable_when_old_manager_has_no_installation_api() {
    let root = crate::import::tests::Fixture::new();
    let package = crate::package_choice::tests::package(&root.0.join("new"), b"new payload");
    let socket = root.0.join("management.sock");
    let listener = astra_platform::platform::LocalListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (stream, request) = next(&listener).await;
        assert!(matches!(
            request.body,
            v::RequestBody::Query(v::Query::Hello)
        ));
        reply(
            stream,
            request,
            v::ResponseBody::Hello(v::Hello {
                version: 2,
                role: "agent".into(),
                epoch: "old".into(),
                build_id: "old".into(),
                capabilities: vec![],
            }),
        )
        .await;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
    });
    let catalog = read_catalog(socket, Some(package.directory.clone()), None)
        .await
        .unwrap();
    assert_eq!(catalog.status.packages.len(), 1);
    assert_eq!(catalog.status.warnings.len(), 1);
    let mut ui = Client::new(crate::protocol::Kind::Proxy);
    ui.installation.accept_catalog(catalog);
    assert!(ui.installation.can_install());
    let Input::Install(request) = ui.installation_request() else {
        panic!("local install")
    };
    assert!(matches!(request.origin, Origin::FrontendPackage(p) if p == package));
    server.await.unwrap();
}

#[tokio::test]
async fn changed_source_is_rejected_by_install_and_elevation_before_any_mutation() {
    let root = crate::import::tests::Fixture::new();
    let package = crate::package_choice::tests::package(&root.0.join("new"), b"original");
    let request = Request {
        origin: Origin::FrontendPackage(package.clone()),
        package: Box::new(local::candidate(&package).unwrap()),
        package_id: package.id(),
        expected_release_id: package.release_id.clone(),
        scope: v::InstallationScope::User,
        elevate: false,
    };
    let context = crate::windows::Context {
        executable: package
            .directory
            .join(astra_platform::platform::executable("astra-tui")),
        home: root.0.join("state"),
        config: root.0.join("config"),
        bin: package.directory.clone(),
        language: crate::i18n::Language::English,
    };
    crate::package_choice::tests::package(&package.directory, b"changed after confirmation");
    let (progress, receiver) = watch::channel(None);
    let error = local::install(&package, &request, &context, progress).unwrap_err();
    assert!(error.to_string().contains("selected package changed"));
    assert!(receiver.borrow().is_none());
    assert!(!context.home.exists());
    let system = Request {
        scope: v::InstallationScope::System,
        elevate: true,
        ..request
    };
    assert!(prepare_elevation(&root.0.join("unused.sock"), &system)
        .await
        .unwrap_err()
        .to_string()
        .contains("selected package changed"));
}

#[test]
fn refreshing_package_order_preserves_identity_and_missing_selection_never_falls_back() {
    let mut ui = fixture();
    let chosen = ui.installation.selected().unwrap().id.clone();
    let mut status = ui.installation.status.clone().unwrap();
    let mut other = status.packages[0].clone();
    other.id = "another".into();
    status.packages.insert(0, other.clone());
    ui.installation.accept_catalog(Catalog {
        status,
        frontend: None,
    });
    assert_eq!(ui.installation.selected().unwrap().id, chosen);
    let mut status = ui.installation.status.clone().unwrap();
    status.packages = vec![other];
    ui.installation.accept_catalog(Catalog {
        status,
        frontend: None,
    });
    assert!(ui.installation.selected().is_none());
    assert!(!ui.installation.can_install());
    assert!(matches!(ui.installation_request(), Input::Idle));
}

#[test]
fn local_engine_progress_uses_the_same_steps_as_manager_progress() {
    use astra_update::installation::{InstallProgress, InstallStep};
    let event = local::progress(InstallProgress {
        step: InstallStep::Copying,
        current: 2,
        total: 4,
    });
    let view = ProgressView::from_event(&event).unwrap();
    assert_eq!(view.step, v::InstallationStep::Copying);
    assert_eq!(view.current, 2);
    assert_eq!(view.total, 4);
    assert_eq!(view.percent(), 45);
}
