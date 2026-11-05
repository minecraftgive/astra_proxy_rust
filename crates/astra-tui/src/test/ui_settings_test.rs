use super::*;
use ratatui::{backend::TestBackend, Terminal};

fn settings_ui() -> (Ui, tempdir::Dir) {
    let dir = tempdir::Dir::new();
    let mut ui = Ui::new(Session::default());
    ui.load_preferences(dir.path().join(FILE));
    ui.navigate(shell::SETTINGS);
    (ui, dir)
}
mod tempdir {
    pub(super) struct Dir(std::path::PathBuf);
    impl Dir {
        pub(super) fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "astra-settings-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        pub(super) fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
fn press(ui: &mut Ui, code: KeyCode) {
    ui.key(event::KeyEvent::new(code, KeyModifiers::NONE));
}

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
fn startup_defaults_are_off_and_instance_choice_persists() {
    let old: Preferences = serde_json::from_str(r#"{"mouse":false}"#).unwrap();
    assert!(!old.auto_start_instances);
    #[cfg(windows)]
    assert!(!old.windows_startup);
    let (mut ui, dir) = settings_ui();
    let row = ALL_FIELDS
        .iter()
        .position(|f| *f == Field::AutoStartInstances)
        .unwrap();
    ui.click_setting(row, Some(0));
    let (loaded, error) = load(&dir.path().join(FILE), &Session::default());
    assert!(error.is_none());
    assert!(loaded.auto_start_instances);
    ui.settings_cursor = row;
    press(&mut ui, KeyCode::Delete);
    let (loaded, _) = load(&dir.path().join(FILE), &Session::default());
    assert!(!loaded.auto_start_instances);
}

#[test]
fn failed_startup_preference_save_keeps_previous_choice_and_error() {
    let (mut ui, dir) = settings_ui();
    // A directory cannot be replaced with the preferences file.
    std::fs::create_dir(dir.path().join(FILE)).unwrap();
    ui.choose_setting(Field::AutoStartInstances, 0);
    assert!(!ui.preferences.auto_start_instances);
    assert!(ui
        .message
        .contains(t("Cannot save preferences: {}").split("{}").next().unwrap()));
}

#[cfg(windows)]
#[test]
fn windows_startup_is_read_from_os_and_registration_failure_is_visible() {
    let old: Preferences = serde_json::from_str(r#"{"windows_startup":true}"#).unwrap();
    assert!(
        !old.windows_startup,
        "JSON cannot silently register Windows startup"
    );
    assert!(serde_json::to_value(old)
        .unwrap()
        .get("windows_startup")
        .is_none());
    let (mut ui, _) = settings_ui();
    ui.choose_setting(Field::WindowsStartup, 0);
    assert!(!ui.preferences.windows_startup);
    assert!(ui
        .message
        .contains("Windows startup context is unavailable"));
}

#[cfg(windows)]
#[test]
fn silent_startup_is_opt_in_and_persists_without_registering_startup() {
    let old: Preferences = serde_json::from_str("{}").unwrap();
    assert!(!old.silent_startup);
    let (mut ui, dir) = settings_ui();
    ui.choose_setting(Field::SilentStartup, 0);
    assert!(ui.preferences.silent_startup);
    assert!(!ui.preferences.windows_startup);
    let (loaded, error) = load(&dir.path().join(FILE), &Session::default());
    assert!(error.is_none());
    assert!(loaded.silent_startup);
    ui.choose_setting(Field::SilentStartup, 1);
    assert!(
        !load(&dir.path().join(FILE), &Session::default())
            .0
            .silent_startup
    );
}

#[cfg(windows)]
#[test]
fn silent_shortcut_update_failure_keeps_previous_preference() {
    let (mut ui, dir) = settings_ui();
    // No context means no real OS registration is read or changed.
    ui.preferences.windows_startup = true;
    ui.choose_setting(Field::SilentStartup, 0);
    assert!(!ui.preferences.silent_startup);
    assert!(ui.preferences.windows_startup);
    assert!(ui
        .message
        .contains("Windows startup context is unavailable"));
    assert!(!dir.path().join(FILE).exists());
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
    assert!(ui
        .message
        .contains("Windows startup context is unavailable"));
    assert!(!dir.path().join(FILE).exists());
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
    assert!(!dir.path().join(FILE).exists());
}
#[test]
fn numeric_marquee_settings_step_clamp_and_read_old_presets() {
    let (mut ui, _dir) = settings_ui();
    ui.change_preferences(|p| p.marquee.unlocked = true);
    let speed = ALL_FIELDS
        .iter()
        .position(|f| *f == Field::MarqueeSpeed)
        .unwrap();
    ui.settings_cursor = speed;
    press(&mut ui, KeyCode::Right);
    assert_eq!(ui.preferences.marquee.speed, 170);
    ui.key(event::KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT));
    assert_eq!(ui.preferences.marquee.speed, 70);
    for _ in 0..3 {
        ui.key(event::KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT));
    }
    assert_eq!(ui.preferences.marquee.speed, 0, "clamped at the minimum");
    assert_eq!(ui.animations.marquee().speed, 0.0);
    press(&mut ui, KeyCode::Delete);
    assert_eq!(ui.preferences.marquee.speed, SPEED.default);
    // Width: clicks on the arrows.
    ui.click_setting(speed + 1, Some(1));
    assert_eq!(ui.preferences.marquee.width, 31);
    ui.click_setting(speed + 1, Some(0));
    ui.click_setting(speed + 1, Some(0));
    assert_eq!(ui.preferences.marquee.width, 29);
    // Frame rate reaches the animation clock.
    ui.settings_cursor = speed + 2;
    assert_eq!(ui.settings_field(), Field::MarqueeFps);
    ui.key(event::KeyEvent::new(KeyCode::Right, KeyModifiers::SHIFT));
    assert_eq!(ui.preferences.marquee.fps, 30);
    assert_eq!(ui.animations.marquee().fps, 30);
    let mut p = Preferences::default();
    Field::MarqueeWidth.set(&mut p, 10_000);
    assert_eq!(p.marquee.width, WIDTH.max);
    Field::MarqueeFps.set(&mut p, 0);
    assert_eq!(p.marquee.fps, FPS.min);
    // Earlier builds saved preset names; numbers out of range are clamped.
    let old: MarqueePreferences =
        serde_json::from_str(r#"{"speed":"fast","width":"extra_wide"}"#).unwrap();
    assert_eq!((old.speed, old.width, old.fps), (320, 120, FPS.default));
    let big: MarqueePreferences = serde_json::from_str(r#"{"speed":99999,"width":1}"#).unwrap();
    assert_eq!((big.speed, big.width), (SPEED.max, WIDTH.min));
    // The row renders the value with its unit.
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    ui.settings_cursor = speed;
    terminal.draw(|f| ui.render(f)).unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(text.contains("160"), "speed value is shown");
}
#[test]
fn keys_change_apply_and_persist_preferences() {
    let (mut ui, dir) = settings_ui();
    // Motion is the fourth row.
    for _ in 0..3 {
        press(&mut ui, KeyCode::Down);
    }
    press(&mut ui, KeyCode::Right);
    assert_eq!(ui.preferences.motion, animation::MotionMode::Reduced);
    assert_eq!(ui.animations.mode(), animation::MotionMode::Reduced);
    assert_eq!(ui.session.motion, animation::MotionMode::Reduced);
    press(&mut ui, KeyCode::Down);
    press(&mut ui, KeyCode::Enter);
    assert!(ui.vim_keys);
    press(&mut ui, KeyCode::Down);
    press(&mut ui, KeyCode::Enter);
    assert_eq!(ui.mouse_request, Some(false));
    let saved = Preferences {
        motion: animation::MotionMode::Reduced,
        vim_keys: true,
        mouse: false,
        ..Default::default()
    };
    let (loaded, error) = load(&dir.path().join(FILE), &Session::default());
    assert!(error.is_none());
    assert_eq!(loaded, saved);
    // Delete restores the default of the selected row.
    press(&mut ui, KeyCode::Delete);
    assert!(ui.preferences.mouse);
    assert_eq!(ui.page, shell::SETTINGS, "keys stay on the page");
}
#[test]
fn missing_file_keeps_session_choices_and_damaged_file_reports() {
    let dir = tempdir::Dir::new();
    let session = Session {
        motion: animation::MotionMode::Off,
        vim_keys: true,
        ..Default::default()
    };
    let (p, error) = load(&dir.path().join(FILE), &session);
    assert!(error.is_none());
    assert_eq!(p.motion, animation::MotionMode::Off);
    assert!(p.vim_keys);
    std::fs::write(dir.path().join(FILE), b"{ not json").unwrap();
    let (p, error) = load(&dir.path().join(FILE), &session);
    assert_eq!(p, Preferences::default());
    assert!(error.is_some());
}
#[test]
fn page_renders_at_any_size_and_choices_are_clickable() {
    let (ui, _dir) = settings_ui();
    for (width, height) in [(30, 8), (80, 24), (140, 40)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| ui.render(f)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            text.contains("Settings") || text.contains("设置"),
            "{width}x{height}"
        );
    }
    let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
    terminal.draw(|f| ui.render(f)).unwrap();
    assert!(ui
        .hits
        .borrow()
        .iter()
        .any(|(_, hit)| matches!(hit, Target::Setting(1, Some(5)))));
}
#[test]
fn accent_and_language_choices_apply() {
    let (mut ui, _dir) = settings_ui();
    ui.click_setting(1, Some(2));
    assert_eq!(ui.preferences.accent, Accent::Green);
    crate::i18n::with_language(crate::i18n::Language::English, || {
        assert_eq!(Field::Language.choice_label(2), "简体中文");
    });
}
#[test]
fn indicator_marquee_only_marks_healthy_states() {
    let (mut ui, _dir) = settings_ui();
    ui.change_preferences(|p| p.marquee.indicator = true);
    assert_eq!(
        ui.indicator_color(Color::Green),
        Color::Green,
        "locked: no effect"
    );
    ui.change_preferences(|p| p.marquee.unlocked = true);
    assert_eq!(ui.indicator_color(Color::Green), animation::STATUS_MARK);
    assert_eq!(ui.indicator_color(Color::Red), Color::Red);
    assert_eq!(ui.indicator_color(MUTED), MUTED);
}
#[test]
fn holding_ctrl_home_on_accent_unlocks_and_hides_the_marquee() {
    let (mut ui, dir) = settings_ui();
    let ctrl_home = event::KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL);
    // Not on the accent row: the chord does nothing here.
    assert!(ui.marquee_hold_key(ctrl_home).is_none());
    press(&mut ui, KeyCode::Down);
    assert_eq!(ui.settings_field(), Field::Accent);
    assert_eq!(
        fields(&ui.preferences).len(),
        MARQUEE_ROW,
        "hidden by default"
    );
    let start = Instant::now();
    let hold = |ui: &mut Ui, until: u64| {
        for ms in (0..=until).step_by(50) {
            ui.marquee_hold_at(start + Duration::from_millis(ms));
        }
    };
    // A pause longer than the repeat gap starts over.
    hold(&mut ui, 2500);
    ui.marquee_hold_at(start + Duration::from_millis(3600));
    assert!(!ui.preferences.marquee.unlocked);
    // Another key in between starts over too.
    ui.marquee_hold = None;
    hold(&mut ui, 2900);
    assert!(ui
        .marquee_hold_key(event::KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
        .is_none());
    assert!(ui.marquee_hold.is_none());
    ui.settings_cursor = 1;
    hold(&mut ui, 3000);
    assert!(ui.preferences.marquee.unlocked);
    assert!(ui.animations.marquee().border);
    assert_eq!(ui.settings_field(), Field::MarqueeBorder);
    assert_eq!(fields(&ui.preferences).len(), ALL_FIELDS.len());
    let (loaded, _) = load(&dir.path().join(FILE), &Session::default());
    assert!(loaded.marquee.unlocked && loaded.marquee.border);
    // Still holding after unlocking does not toggle it straight back.
    ui.marquee_hold_at(start + Duration::from_millis(6500));
    assert!(ui.preferences.marquee.unlocked);
    // A new hold on the accent row hides and turns everything off.
    ui.settings_cursor = 1;
    ui.marquee_hold = None;
    let start = start + Duration::from_secs(10);
    for ms in (0..=3000).step_by(50) {
        ui.marquee_hold_at(start + Duration::from_millis(ms));
    }
    assert_eq!(ui.preferences.marquee, MarqueePreferences::default());
    assert!(!ui.animations.marquee().border);
}
