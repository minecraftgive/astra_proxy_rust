use super::*;
fn profile_ui() -> Ui {
    let mut ui = Ui::new(Session::default());
    for (name, kind) in [
        ("proxy-a", Kind::Proxy),
        ("proxy-b", Kind::Proxy),
        ("rules-a", Kind::Rule),
        ("rules-b", Kind::Rule),
    ] {
        ui.rows.push(Row {
            instance: Instance {
                name: name.into(),
                kind,
                config: format!("{name}.toml").into(),
            },
            pid: None,
            state: "Stopped".into(),
            uptime: 0,
            changed: false,
        });
    }
    ui.active_profile = Some("proxy-b".into());
    ui
}
fn press(ui: &mut Ui, code: KeyCode) -> Input {
    ui.key(event::KeyEvent::new(code, KeyModifiers::NONE))
}
#[test]
fn configuration_type_control_selects_rules_without_changing_active_proxy() {
    let mut ui = profile_ui();
    ui.editor =
        Some(Editor::new("proxy-a".into(), "standalone=true\n".into()).with_kind(Kind::Proxy));
    ui.key(event::KeyEvent::new(
        KeyCode::Char('p'),
        KeyModifiers::CONTROL,
    ));
    assert_eq!(ui.palette_matches().len(), 2);
    press(&mut ui, KeyCode::Tab);
    assert_eq!(ui.palette.as_ref().unwrap().focus, PaletteFocus::Type);
    press(&mut ui, KeyCode::Right);
    assert_eq!(ui.palette.as_ref().unwrap().profile_kind, Kind::Rule);
    assert!(ui
        .palette_matches()
        .iter()
        .all(|entry| entry.label.starts_with("rules-")));
    press(&mut ui, KeyCode::Enter);
    assert_eq!(ui.palette.as_ref().unwrap().focus, PaletteFocus::List);
    press(&mut ui, KeyCode::Down);
    press(&mut ui, KeyCode::Enter);
    assert!(ui.palette.is_none());
    assert_eq!(ui.page, PROFILES);
    assert_eq!(ui.kind, Kind::Rule);
    assert_eq!(ui.selected().unwrap().instance.name, "rules-b");
    assert_eq!(ui.active_proxy().unwrap().instance.name, "proxy-b");
    assert_eq!(ui.drafts.len(), 1);
    ui.run_command(commands::Id::Profiles);
    press(&mut ui, KeyCode::Tab);
    press(&mut ui, KeyCode::Left);
    press(&mut ui, KeyCode::Enter);
    press(&mut ui, KeyCode::Enter);
    assert_eq!(ui.active_proxy().unwrap().instance.name, "proxy-a");
    assert_eq!(ui.kind, Kind::Proxy);
}
#[test]
fn palette_page_keys_follow_visible_rows_after_resize() {
    use ratatui::backend::TestBackend;
    let mut ui = profile_ui();
    for index in 0..30 {
        let mut row = ui.rows[0].clone();
        row.instance.name = format!("extra-{index}");
        ui.rows.push(row);
    }
    for profiles in [false, true] {
        ui.palette = Some(if profiles {
            Palette::profiles(Kind::Proxy)
        } else {
            Palette::default()
        });
        for height in [10, 6] {
            let mut terminal = Terminal::new(TestBackend::new(70, height)).unwrap();
            terminal.draw(|frame| ui.render_palette(frame)).unwrap();
            let visible = usize::from(ui.palette.as_ref().unwrap().viewport_height.get());
            assert_eq!(visible, usize::from(height - 4));
            let before = ui.palette.as_ref().unwrap().selected;
            press(&mut ui, KeyCode::PageDown);
            assert_eq!(ui.palette.as_ref().unwrap().selected, before + visible);
            press(&mut ui, KeyCode::PageUp);
            assert_eq!(ui.palette.as_ref().unwrap().selected, before);
        }
    }
}
#[test]
fn multilingual_columns_keep_the_shortcut_column_inside_the_popup() {
    for width in [0, 1, 9, 24, 51] {
        for text in ["通用", "Command palette", "右列为本页可直接使用的按键"] {
            assert_eq!(Line::from(palette_column(text, width)).width(), width);
        }
    }
    let line = Line::from(vec![
        Span::raw(palette_column("通用", 9)),
        Span::raw(palette_column("命令面板", 51)),
        Span::raw(" "),
        Span::raw(palette_column("Ctrl-K / F2", 11)),
    ]);
    assert_eq!(line.width(), 72);
}
