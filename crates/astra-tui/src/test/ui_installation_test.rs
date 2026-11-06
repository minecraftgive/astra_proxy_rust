use super::*;
use ratatui::{backend::TestBackend, Terminal};

fn fixture() -> Ui {
    let mut ui = Ui::new(Session::default());
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

fn press(ui: &mut Ui, code: KeyCode) -> Input {
    ui.key(event::KeyEvent::new(code, KeyModifiers::NONE))
}

#[test]
fn installation_defaults_to_user_and_requests_selected_target_package() {
    let mut ui = fixture();
    let Input::Install(request) = press(&mut ui, KeyCode::Enter) else {
        panic!("Expected installation");
    };
    assert_eq!(request.package_id, "package-client");
    assert_eq!(request.expected_release_id, "release-1");
    assert_eq!(request.scope, v::InstallationScope::User);
    assert!(!request.elevate);
    press(&mut ui, KeyCode::Tab);
    assert_eq!(ui.installation.scope, v::InstallationScope::System);
    press(&mut ui, KeyCode::Esc);
    assert!(!ui.installation.visible());
}

#[test]
fn missing_destination_and_failed_status_cannot_request_installation() {
    let mut ui = fixture();
    ui.installation.status.as_mut().unwrap().packages[0]
        .user
        .unavailable_reason = Some("Cannot determine user account".into());
    assert!(matches!(press(&mut ui, KeyCode::Enter), Input::Idle));
    assert!(ui
        .installation
        .error
        .as_deref()
        .unwrap()
        .contains("user account"));
    ui.installation.query_error = Some("Target unavailable".into());
    assert!(matches!(press(&mut ui, KeyCode::Enter), Input::Idle));
    ui.installation.query_error = None;
    ui.installation.status.as_mut().unwrap().packages[0]
        .user
        .unavailable_reason = None;
    press(&mut ui, KeyCode::Char('r'));
    assert!(matches!(press(&mut ui, KeyCode::Enter), Input::Idle));
}

#[test]
fn first_notice_is_dismissible_and_remembered_per_ui_kind_and_target() {
    let mut ui = fixture();
    ui.installation.open = false;
    ui.installation.notice = true;
    ui.installation.notice_key = Some("client:target-a".into());
    press(&mut ui, KeyCode::Esc);
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
    press(&mut ui, KeyCode::Enter);
    assert!(ui.installation.open);
    assert_eq!(ui.page, shell::SETTINGS);
    assert!(ui
        .preferences
        .installation_notices
        .contains("server:target-a"));
    let saved = serde_json::to_string(&ui.preferences).unwrap();
    let restored: settings::Preferences = serde_json::from_str(&saved).unwrap();
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
        let mut ui = Ui::new(Session::default());
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
        ui.poll_installation(std::path::Path::new("unused.sock"))
            .await
    );
    assert!(ui.installation.notice);
    assert!(ui.installation.query.is_none());
    press(&mut ui, KeyCode::Esc);
    assert!(
        !ui.poll_installation(std::path::Path::new("unused.sock"))
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
        let (stream, request) = crate::ui::test_rpc::next(&listener).await;
        assert!(matches!(
            request.body,
            v::RequestBody::Query(v::Query::Hello)
        ));
        crate::ui::test_rpc::reply(
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
    let mut ui = Ui::new(Session::default());
    ui.installation.notice_key = Some("client:old-target".into());
    ui.installation.query_error = Some(error);
    assert!(!ui.poll_installation(&socket).await);
    assert!(!ui.installation.visible());
    assert!(ui.message.is_empty());
    server.await.unwrap();
}

#[test]
fn installation_panel_renders_all_roles_target_and_result_on_small_terminals() {
    crate::i18n::with_language(crate::i18n::Language::English, || {
        let mut ui = fixture();
        ui.installation.result = Some(v::InstallationResult {
            role: "client".into(),
            directory: "/installed/client".into(),
            entry: "/installed/astra-tui".into(),
            already_installed: false,
            message: "Installation completed".into(),
            launch_command: "astra-tui --state-dir /existing/state".into(),
        });
        let mut terminal = Terminal::new(TestBackend::new(110, 30)).unwrap();
        terminal
            .draw(|frame| ui.render_installation(frame))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        for expected in [
            "selected-agent",
            "selected-user",
            "client",
            "server",
            "controller",
            "web",
            "/remote/current-client",
            "Current user",
            "Installation completed",
            "astra-tui --state-dir /existing/state",
        ] {
            assert!(text.contains(expected), "Missing {expected}: {text}");
        }
        ui.installation.error = Some("Target does not support installation".into());
        for (width, height) in [(24, 8), (8, 4), (1, 1)] {
            let mut small = Terminal::new(TestBackend::new(width, height)).unwrap();
            small.draw(|frame| ui.render_installation(frame)).unwrap();
        }
        press(&mut ui, KeyCode::PageDown);
        assert_eq!(ui.installation.scroll, 5);
    });
}
