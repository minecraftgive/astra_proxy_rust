use super::*;
fn press(ui: &mut Ui, code: KeyCode) -> Input {
    ui.key(event::KeyEvent::new(code, KeyModifiers::NONE))
}
fn rendered(ui: &Ui) -> String {
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| ui.render(frame)).unwrap();
    terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}
#[test]
fn global_overlay_keys_replace_visible_layers_and_preserve_underlying_input() {
    crate::i18n::with_language(crate::i18n::Language::English, || {
        let mut ui = Ui::new(Session::default());
        ui.editor = Some(Editor::new("draft".into(), "standalone=true\n".into()));
        ui.prompt(Prompt::AddName);
        ui.paste_prompt("pending name");
        let editor = serde_json::to_value(ui.editor.as_ref().unwrap()).unwrap();
        press(&mut ui, KeyCode::F(2));
        assert!(rendered(&ui).contains("Commands"));
        press(&mut ui, KeyCode::F(6));
        assert!(ui.palette.is_none());
        assert!(rendered(&ui).contains("Task details · F6"));
        press(&mut ui, KeyCode::F(1));
        assert!(!ui.tasks.showing_details());
        assert!(rendered(&ui).contains("Page operations"));
        ui.key(event::KeyEvent::new(
            KeyCode::Char('p'),
            KeyModifiers::CONTROL,
        ));
        assert!(ui.help_scroll.is_some());
        assert!(ui.palette.is_none()); // A pending prompt prevents changing configuration.
        press(&mut ui, KeyCode::F(2));
        assert!(ui.help_scroll.is_none());
        assert!(rendered(&ui).contains("Commands"));
        for ch in ['s', 'z', 'y'] {
            assert!(matches!(
                ui.key(event::KeyEvent::new(
                    KeyCode::Char(ch),
                    KeyModifiers::CONTROL
                )),
                Input::Idle
            ));
        }
        assert!(
            matches!(press(&mut ui,KeyCode::F(7)),Input::Run(Action::Logs(name),_) if name=="@tui")
        );
        assert!(ui.palette.is_none());
        assert!(ui.help_scroll.is_none());
        // Jobs installs this placeholder before the next frame while its subscription opens.
        ui.tui_viewer = Some(logs::Viewer::new("@tui".into(), Default::default()));
        assert!(rendered(&ui).contains("@tui"));
        press(&mut ui, KeyCode::F(1));
        assert!(ui.tui_viewer.is_none());
        assert!(rendered(&ui).contains("Page operations"));
        press(&mut ui, KeyCode::F(2));
        press(&mut ui, KeyCode::Esc);
        assert_eq!(ui.input, "pending name");
        assert!(matches!(ui.prompt, Some(Prompt::AddName)));
        assert_eq!(
            serde_json::to_value(ui.editor.as_ref().unwrap()).unwrap(),
            editor
        );
        ui.prompt = None;
        press(&mut ui, KeyCode::F(6));
        ui.key(event::KeyEvent::new(
            KeyCode::Char('p'),
            KeyModifiers::CONTROL,
        ));
        assert!(!ui.tasks.showing_details());
        assert!(rendered(&ui).contains("Switch profile"));
        ui.key(event::KeyEvent::new(
            KeyCode::Char('k'),
            KeyModifiers::CONTROL,
        ));
        assert!(ui.palette.is_none());
        assert_eq!(
            serde_json::to_value(ui.editor.as_ref().unwrap()).unwrap(),
            editor
        );
    });
}
#[test]
fn ordinary_text_edits_at_unicode_cursor_and_palette_paste_stays_local() {
    let mut ui = Ui::new(Session::default());
    ui.prompt(Prompt::AddName);
    ui.paste_prompt("a界c");
    press(&mut ui, KeyCode::Left);
    press(&mut ui, KeyCode::Backspace);
    press(&mut ui, KeyCode::Char('b'));
    assert_eq!(ui.input, "abc");
    press(&mut ui, KeyCode::Home);
    press(&mut ui, KeyCode::Delete);
    assert_eq!(ui.input, "bc");
    press(&mut ui, KeyCode::Esc);
    ui.editor = Some(Editor::new("a".into(), "standalone=true\n".into()));
    press(&mut ui, KeyCode::F(2));
    ui.paste("Motion");
    assert_eq!(ui.palette.as_ref().unwrap().query, "Motion");
    assert!(ui.prompt.is_none());
    assert_eq!(ui.editor.as_ref().unwrap().text(), "standalone=true\n");
    press(&mut ui, KeyCode::Home);
    press(&mut ui, KeyCode::Delete);
    assert_eq!(ui.palette.as_ref().unwrap().query, "otion");
}
#[test]
fn focus_navigation_and_quit_do_not_apply_or_lose_a_draft() {
    let mut ui = Ui::new(Session::default());
    ui.editor = Some(Editor::new("a".into(), "standalone=true\n".into()));
    ui.editor
        .as_mut()
        .unwrap()
        .import("standalone=false\n".into())
        .unwrap();
    press(&mut ui, KeyCode::Tab);
    assert_eq!(ui.focus.region, focus::Region::Controls);
    assert_eq!(ui.page, 0);
    assert!(matches!(press(&mut ui, KeyCode::Char('q')), Input::Quit));
    assert_eq!(
        ui.drafts.values().next().unwrap().editor.text(),
        "standalone=false\n"
    );
    let before = ui.editor.as_ref().unwrap().text();
    let mut key = event::KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
    key.kind = KeyEventKind::Repeat;
    assert!(matches!(ui.key(key), Input::Idle));
    assert_eq!(ui.editor.as_ref().unwrap().text(), before);
}
#[test]
fn connection_search_cycles_filtered_results_by_identity_and_wraps() {
    use astra_model::runtime::Connection;
    use astra_model::runtime::Snapshot;
    let mut ui = Ui::new(Session::default());
    ui.page = shell::CONNECTIONS;
    ui.rows.push(Row {
        instance: Instance {
            name: "proxy".into(),
            kind: Kind::Proxy,
            config: "proxy.toml".into(),
        },
        pid: Some(42),
        state: "Running".into(),
        uptime: 0,
        changed: false,
    });
    let make = |id, host: &str| Connection {
        id,
        host: host.into(),
        source: None,
        port: 443,
        network: "tcp".into(),
        inbound: "socks".into(),
        outbound: "DIRECT".into(),
        upload: 0,
        download: 0,
        elapsed_ms: 0,
    };
    ui.accept_runtime(
        "proxy".into(),
        Snapshot {
            pid: 42,
            connections: vec![
                make(11, "match.one"),
                make(22, "other"),
                make(33, "match.two"),
            ],
            ..Default::default()
        },
    );
    ui.connection_filter = "match".into();
    ui.select_connection(0);
    press(&mut ui, KeyCode::F(3));
    assert!(ui.connection_is_selected(33));
    press(&mut ui, KeyCode::F(3));
    assert!(ui.connection_is_selected(11));
    ui.key(event::KeyEvent::new(KeyCode::F(3), KeyModifiers::SHIFT));
    assert!(ui.connection_is_selected(33));
    ui.connection_filter = "no matches".into();
    press(&mut ui, KeyCode::F(3));
    assert!(ui.visible_connections().is_empty());
}
#[test]
fn repeat_space_does_not_toggle_member_picker_but_repeated_text_inserts() {
    let mut ui = Ui::new(Session::default());
    let mut value =
        serde_json::to_value(Editor::new("draft".into(), "standalone=true\n".into())).unwrap();
    value["form"]["mode"] =
        serde_json::json!({"Members":{"options":["DIRECT"],"chosen":[],"cursor":0,"find":null}});
    ui.editor = Some(serde_json::from_value(value).unwrap());
    let before = serde_json::to_value(ui.editor.as_ref().unwrap()).unwrap();
    let mut repeated = event::KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE);
    repeated.kind = KeyEventKind::Repeat;
    assert!(matches!(ui.key(repeated), Input::Idle));
    assert_eq!(
        serde_json::to_value(ui.editor.as_ref().unwrap()).unwrap(),
        before
    );
    ui.editor = None;
    ui.prompt(Prompt::AddName);
    repeated.code = KeyCode::Char('a');
    ui.key(repeated);
    ui.key(repeated);
    assert_eq!(ui.input, "aa");
}
#[test]
fn command_registry_has_unique_ids_and_consistent_repeat_policy() {
    for (index, spec) in SPECS.iter().enumerate() {
        assert!(!SPECS[..index].iter().any(|other| other.id == spec.id));
    }
    assert!(repeatable(event::KeyEvent::new(
        KeyCode::F(3),
        KeyModifiers::NONE
    )));
    assert!(!repeatable(event::KeyEvent::new(
        KeyCode::Char('s'),
        KeyModifiers::CONTROL
    )));
}
