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

#[test]
fn installation_launch_controls_support_keys_and_ignore_key_repeat() {
    let mut ui = fixture();
    assert!(ui.client.update_launch.automatic);
    press(&mut ui, KeyCode::Char('a'));
    assert!(!ui.client.update_launch.automatic);
    ui.installation_key(event::KeyEvent::new_with_kind(
        KeyCode::Char('a'),
        KeyModifiers::NONE,
        KeyEventKind::Repeat,
    ));
    assert!(!ui.client.update_launch.automatic);
    press(&mut ui, KeyCode::Char('a'));
    assert!(ui.client.update_launch.automatic);
    assert!(matches!(press(&mut ui, KeyCode::Char('l')), Input::Idle));
    assert!(!ui.client.update_launch.pending());
}
