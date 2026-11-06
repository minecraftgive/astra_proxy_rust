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
fn installation_settings_entry_stays_available_after_dismissal() {
    let (mut ui, _) = settings_ui();
    assert!(fields(&ui.preferences).contains(&Field::Installation));
    ui.preferences
        .installation_notices
        .insert("client:target".into());
    ui.choose_setting(Field::Installation, 0);
    assert!(ui.installation.visible());
    press(&mut ui, KeyCode::Esc);
    assert!(!ui.installation.visible());
    ui.choose_setting(Field::Installation, 0);
    assert!(ui.installation.visible());
}

fn terminal_candidate(id: &str, label: &str) -> crate::terminals::Candidate {
    crate::terminals::Candidate {
        id: id.into(),
        label: label.into(),
        executable: Some(std::path::PathBuf::from("installed-terminal")),
    }
}

#[test]
fn terminal_preference_defaults_to_auto_and_preserves_missing_selections() {
    let old: Preferences = serde_json::from_str(r#"{"mouse":false}"#).unwrap();
    assert_eq!(old.terminal, "auto");
    let (mut ui, dir) = settings_ui();
    assert!(ui.change_preferences(|p| p.terminal = "uninstalled-terminal".into()));
    let choices = ui.terminal_choices();
    assert_eq!(choices[0].id, "auto");
    assert_eq!(
        choices.iter().any(|choice| choice.id == "native"),
        cfg!(windows)
    );
    let selected = &choices[ui.setting_index(Field::Terminal)];
    assert_eq!(selected.id, "uninstalled-terminal");
    assert!(!selected.available);
    assert!(selected.label.contains(t("Unavailable")));
    assert_eq!(
        super::super::preferred_terminal(dir.path()),
        "uninstalled-terminal"
    );
    let before = ui.preferences.clone();
    ui.choose_setting(Field::Terminal, ui.setting_index(Field::Terminal));
    assert_eq!(ui.preferences, before);
    assert!(ui.message.contains(t(
        "This terminal is unavailable. Scan again or choose another terminal."
    )));
}

#[test]
fn terminal_picker_saves_ids_handles_escape_and_keeps_choice_on_save_failure() {
    let (mut ui, dir) = settings_ui();
    ui.terminals.candidates = vec![terminal_candidate("wezterm", "WezTerm")];
    let index = ui
        .terminal_choices()
        .iter()
        .position(|choice| choice.id == "wezterm")
        .unwrap();
    ui.settings_cursor = ALL_FIELDS
        .iter()
        .position(|field| *field == Field::Terminal)
        .unwrap();
    let page = ui.page;
    press(&mut ui, KeyCode::Enter);
    assert!(ui.terminals.picker.is_some());
    press(&mut ui, KeyCode::Tab);
    assert_eq!(
        ui.focus.region,
        focus::Region::Main,
        "dropdown owns local focus keys"
    );
    let selected = ui.terminals.picker.clone();
    press(&mut ui, KeyCode::Char('j'));
    assert_eq!(ui.terminals.picker, selected, "Vim aliases remain opt-in");
    press(&mut ui, KeyCode::Esc);
    assert!(ui.terminals.picker.is_none());
    assert_eq!(
        ui.page, page,
        "Escape closes the dropdown before page history"
    );
    press(&mut ui, KeyCode::Enter);
    press(&mut ui, KeyCode::End);
    press(&mut ui, KeyCode::Enter);
    assert_eq!(ui.preferences.terminal, "wezterm");
    assert!(ui.terminals.picker.is_none());
    assert_eq!(
        load(&dir.path().join(FILE), &Session::default()).0.terminal,
        "wezterm"
    );
    // Save failures must keep both the displayed and persisted choice intact.
    ui.preferences_path = Some(dir.path().to_owned());
    ui.choose_setting(Field::Terminal, 0);
    assert_eq!(ui.preferences.terminal, "wezterm");
    assert_eq!(ui.setting_index(Field::Terminal), index);
    assert!(ui
        .message
        .contains(t("Cannot save preferences: {}").split("{}").next().unwrap()));
    press(&mut ui, KeyCode::Char('r'));
    assert!(ui.terminals.pending);
    assert_eq!(ui.preferences.terminal, "wezterm");
}

#[tokio::test]
async fn terminal_scanning_runs_on_entry_and_refresh_preserves_saved_id() {
    let (mut ui, _dir) = settings_ui();
    assert!(ui.poll_terminal_scan().await);
    assert!(ui.terminals.inside_settings);
    assert!(!ui.terminals.pending);
    // Complete the bounded PATH scan before substituting a deterministic result.
    ui.terminals.scan.take().unwrap().await.unwrap();
    assert!(
        !ui.poll_terminal_scan().await,
        "polling does not scan every frame"
    );
    ui.preferences.terminal = "wezterm".into();
    ui.terminals.scan = Some(tokio::spawn(async {
        vec![terminal_candidate("alacritty", "Alacritty")]
    }));
    while !ui.terminals.scan.as_ref().unwrap().is_finished() {
        tokio::task::yield_now().await;
    }
    assert!(ui.poll_terminal_scan().await);
    assert_eq!(ui.preferences.terminal, "wezterm");
    assert!(!ui.terminal_choices()[ui.setting_index(Field::Terminal)].available);
    assert!(!ui.poll_terminal_scan().await);
    ui.page = shell::OVERVIEW;
    assert!(!ui.poll_terminal_scan().await);
    ui.page = shell::SETTINGS;
    assert!(
        ui.poll_terminal_scan().await,
        "returning to settings scans again"
    );
    ui.terminals.scan.take().unwrap().await.unwrap();
}

#[test]
fn terminal_dropdown_renders_and_mouse_selects_only_visible_choices() {
    let (mut ui, _dir) = settings_ui();
    ui.terminals.candidates = vec![terminal_candidate("wezterm", "WezTerm")];
    ui.open_terminal_picker();
    let index = ui
        .terminal_choices()
        .iter()
        .position(|choice| choice.id == "wezterm")
        .unwrap();
    let mut terminal = Terminal::new(TestBackend::new(110, 40)).unwrap();
    terminal.draw(|frame| ui.render(frame)).unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(text.contains("WezTerm"));
    let rect = ui
        .hits
        .borrow()
        .iter()
        .find_map(|(rect, target)| match target {
            Target::Setting(_, Some(choice)) if *choice == index => Some(*rect),
            _ => None,
        })
        .unwrap();
    ui.mouse(event::MouseEvent {
        kind: event::MouseEventKind::Down(event::MouseButton::Left),
        column: rect.x,
        row: rect.y,
        modifiers: KeyModifiers::NONE,
    });
    assert_eq!(ui.preferences.terminal, "wezterm");
    assert!(ui.terminals.picker.is_none());
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
