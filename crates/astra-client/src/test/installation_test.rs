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
