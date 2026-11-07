use super::*;
use crate::state::Session;
#[test]
fn every_field_round_trips_each_choice() {
    for field in ALL_FIELDS {
        for index in 0..field.choices().len() {
            let mut p = Preferences::default();
            field.set(&mut p, index);
            assert_eq!(field.get(&p), index, "{field:?}");
        }
    }
}

#[test]
fn legacy_instance_startup_preference_is_ignored_and_not_saved() {
    let old: Preferences =
        serde_json::from_str(r#"{"auto_start_instances":true,"mouse":false}"#).unwrap();
    assert!(!old.mouse);
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    assert!(!old.startup_enabled);
    assert!(serde_json::to_value(old)
        .unwrap()
        .get("auto_start_instances")
        .is_none());
}

#[test]
fn information_tokens_preserve_frontend_identity_and_strip_control_characters() {
    crate::i18n::with_language(crate::i18n::Language::English, || {
        let mut client = Client::new(crate::protocol::Kind::Proxy);
        client.program_info.local_error = Some("unavailable\u{1b}\npath".into());
        let mut build = astra_platform::build_info!();
        build.name = "test-desktop".into();
        let rows = client.settings_program_info(&build, None);
        let text = rows
            .iter()
            .flatten()
            .map(|token| token.content.as_str())
            .collect::<String>();
        assert!(text.contains("test-desktop"));
        assert!(text.contains("unavailablepath"));
        assert!(!text.contains(" Terminal:"));
        assert!(!text.chars().any(char::is_control));
        let terminal = client.settings_program_info(&build, Some(Some(u16::MAX)));
        assert!(terminal
            .iter()
            .flatten()
            .any(|token| token.content.contains("24-bit")));
    });
}

fn settings_ui() -> (Client, crate::import::tests::Fixture) {
    let dir = crate::import::tests::Fixture::new();
    let mut client = Client::new(crate::protocol::Kind::Proxy);
    client.load_preferences(dir.0.join(FILE));
    client.navigate(crate::navigation::SETTINGS);
    (client, dir)
}
#[test]
fn failed_preference_save_keeps_previous_choice_and_error() {
    let (mut ui, dir) = settings_ui();
    // A directory cannot be replaced with the preferences file.
    std::fs::create_dir(dir.0.as_path().join(FILE)).unwrap();
    let previous = ui.preferences.mouse;
    ui.choose_setting(Field::Mouse, usize::from(previous));
    assert_eq!(ui.preferences.mouse, previous);
    assert!(ui
        .message
        .contains(t("Cannot save preferences: {}").split("{}").next().unwrap()));
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
#[test]
fn startup_enabled_is_read_from_os_and_registration_failure_is_visible() {
    let old: Preferences = serde_json::from_str(r#"{"startup_enabled":true}"#).unwrap();
    assert!(
        !old.startup_enabled,
        "JSON cannot silently register OS startup"
    );
    assert!(serde_json::to_value(old)
        .unwrap()
        .get("startup_enabled")
        .is_none());
    let (mut ui, _) = settings_ui();
    ui.choose_setting(Field::StartupRegistration, 0);
    assert!(!ui.preferences.startup_enabled);
    assert!(ui.message.contains("Startup context is unavailable"));
}

#[cfg(any(windows, target_os = "macos"))]
#[test]
fn silent_startup_is_opt_in_and_persists_without_registering_startup() {
    let old: Preferences = serde_json::from_str("{}").unwrap();
    assert!(!old.silent_startup);
    let (mut ui, dir) = settings_ui();
    ui.choose_setting(Field::SilentStartup, 0);
    assert!(ui.preferences.silent_startup);
    assert!(!ui.preferences.startup_enabled);
    let (loaded, error) = load(&dir.0.as_path().join(FILE), &Session::default());
    assert!(error.is_none());
    assert!(loaded.silent_startup);
    ui.choose_setting(Field::SilentStartup, 1);
    assert!(
        !load(&dir.0.as_path().join(FILE), &Session::default())
            .0
            .silent_startup
    );
}

#[cfg(any(windows, target_os = "macos"))]
#[test]
fn silent_shortcut_update_failure_keeps_previous_preference() {
    let (mut ui, dir) = settings_ui();
    // No context means no real OS registration is read or changed.
    ui.preferences.startup_enabled = true;
    ui.choose_setting(Field::SilentStartup, 0);
    assert!(!ui.preferences.silent_startup);
    assert!(ui.preferences.startup_enabled);
    assert!(ui.message.contains("Startup context is unavailable"));
    assert!(!dir.0.as_path().join(FILE).exists());
}

#[cfg(windows)]
#[test]
fn privilege_service_is_os_state_and_missing_context_cannot_install_it() {
    let loaded: Preferences = serde_json::from_str(r#"{"windows_service":true}"#).unwrap();
    assert!(!loaded.windows_service);
    assert!(serde_json::to_value(loaded)
        .unwrap()
        .get("windows_service")
        .is_none());
    let (mut ui, dir) = settings_ui();
    ui.choose_setting(Field::WindowsService, 0);
    assert!(!ui.preferences.windows_service);
    assert!(ui.service_change.is_none());
    assert!(ui.message.contains("Startup context is unavailable"));
    assert!(!dir.0.as_path().join(FILE).exists());
}

#[cfg(windows)]
#[tokio::test]
async fn privilege_service_result_updates_without_persisting_authorization() {
    let (mut ui, dir) = settings_ui();
    ui.service_change = Some(tokio::spawn(async { Ok(true) }));
    while !ui.service_change.as_ref().unwrap().is_finished() {
        tokio::task::yield_now().await;
    }
    assert!(ui.poll_service_change().await);
    assert!(ui.preferences.windows_service);
    assert!(ui.service_change.is_none());
    assert!(!dir.0.as_path().join(FILE).exists());
}
#[test]
fn missing_file_keeps_session_choices_and_damaged_file_reports() {
    let dir = crate::import::tests::Fixture::new();
    let session = Session {
        motion: crate::motion::MotionMode::Off,
        vim_keys: true,
        ..Default::default()
    };
    let (p, error) = load(&dir.0.as_path().join(FILE), &session);
    assert!(error.is_none());
    assert_eq!(p.motion, crate::motion::MotionMode::Off);
    assert!(p.vim_keys);
    std::fs::write(dir.0.as_path().join(FILE), b"{ not json").unwrap();
    let (p, error) = load(&dir.0.as_path().join(FILE), &session);
    assert_eq!(p, Preferences::default());
    assert!(error.is_some());
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
#[test]
fn startup_interface_is_persisted_without_registering_and_failed_save_rolls_back() {
    let old: Preferences = serde_json::from_str(r#"{"mouse":false}"#).unwrap();
    assert_eq!(old.startup_frontend, StartupFrontend::Tui);
    let (mut client, dir) = settings_ui();
    client.choose_setting(Field::StartupFrontend, 1);
    assert_eq!(client.preferences.startup_frontend, StartupFrontend::Gui);
    assert!(!client.preferences.startup_enabled);
    assert_eq!(
        load(&dir.0.join(FILE), &Session::default())
            .0
            .startup_frontend,
        StartupFrontend::Gui
    );
    client.preferences_path = Some(dir.0.clone());
    client.choose_setting(Field::StartupFrontend, 0);
    assert_eq!(client.preferences.startup_frontend, StartupFrontend::Gui);
    assert!(client
        .message
        .contains(t("Cannot save preferences: {}").split("{}").next().unwrap()));
    client.preferences_path = Some(dir.0.join(FILE));
    client.preferences.startup_enabled = true;
    client.choose_setting(Field::StartupFrontend, 0);
    assert_eq!(client.preferences.startup_frontend, StartupFrontend::Gui);
    assert!(client.message.contains("Startup context is unavailable"));
}
