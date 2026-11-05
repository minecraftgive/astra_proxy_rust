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
fn palette_searches_current_language_and_english() {
    for language in [
        Language::English,
        Language::Simplified,
        Language::Traditional,
    ] {
        crate::i18n::with_language(language, || {
            let mut ui = Ui::new(Session::default());
            ui.palette = Some(Palette::default());
            assert_eq!(ui.palette_matches().len(), ui.palette_entries().len());
            for source in [
                "Overview",
                "Full restart",
                "Command palette",
                "Import local update package",
            ] {
                for query in [t(source).to_owned(), source.to_uppercase()] {
                    ui.palette.as_mut().unwrap().query = query;
                    let entries = ui.palette_matches();
                    assert_eq!(entries.len(), 1, "{language:?}: {source}");
                    assert_eq!(entries[0].label, t(source));
                }
            }
            for query in [t("General"), "GENERAL"] {
                ui.palette.as_mut().unwrap().query = query.into();
                let entries = ui.palette_matches();
                assert!(!entries.is_empty());
                assert!(entries.iter().all(|entry| entry.scope == "General"));
            }
            for query in [
                format!("{} {}", t("Go to"), t("Overview")),
                "GO TO OVERVIEW".into(),
            ] {
                ui.palette.as_mut().unwrap().query = query;
                let entries = ui.palette_matches();
                assert_eq!(entries.len(), 1);
                assert!(matches!(entries[0].run, Run::Page(OVERVIEW)));
            }
            ui.palette.as_mut().unwrap().query = "ctrl-k".into();
            assert_eq!(ui.palette_matches().len(), 1);
            ui.palette.as_mut().unwrap().query = "no matching command".into();
            assert!(ui.palette_matches().is_empty());
        });
    }
}
#[test]
fn palette_searches_dynamic_labels_in_both_languages() {
    for language in [
        Language::English,
        Language::Simplified,
        Language::Traditional,
    ] {
        crate::i18n::with_language(language, || {
            let mut ui = profile_ui();
            ui.rows[0].pid = Some(42);
            for profiles in [false, true] {
                ui.palette = Some(Palette {
                    profiles,
                    ..Default::default()
                });
                for (name, status) in [("proxy-a", "Running"), ("proxy-b", "Stopped")] {
                    for query in [
                        format!("{} {name} · {}", t("Profile"), t(status)),
                        format!("PROFILE {name} · {}", status.to_uppercase()),
                    ] {
                        ui.palette.as_mut().unwrap().query = query;
                        let entries = ui.palette_matches();
                        assert_eq!(entries.len(), 1);
                        assert_eq!(entries[0].label, format!("{name} · {}", t(status)));
                        assert!(
                            matches!(&entries[0].run, Run::Activate { name: target, .. } if target == name)
                        );
                    }
                }
            }
            ui.palette = Some(Palette::default());
            for query in [
                format!("{} · proxy-b", t("Force start")),
                "FORCE START · proxy-b".into(),
            ] {
                ui.palette.as_mut().unwrap().query = query;
                let entries = ui.palette_matches();
                assert_eq!(entries.len(), 1);
                assert!(matches!(entries[0].run, Run::ForceStart));
                assert_eq!(entries[0].label, format!("{} · proxy-b", t("Force start")));
            }
            ui.palette_run(0);
            assert!(matches!(&ui.prompt, Some(Prompt::ForceStart(name)) if name == "proxy-b"));
        });
    }
}
#[test]
fn force_start_palette_targets_the_selected_core_or_active_profile() {
    let mut ui = profile_ui();
    for (page, kind, expected) in [
        (PROFILES, Kind::Rule, "rules-a"),
        (PROFILES, Kind::Proxy, "proxy-a"),
        (OVERVIEW, Kind::Proxy, "proxy-b"),
    ] {
        ui.page = page;
        ui.kind = kind;
        ui.palette = Some(Palette::default());
        let entries = ui.palette_matches();
        let index = entries
            .iter()
            .position(|entry| matches!(entry.run, Run::ForceStart))
            .unwrap();
        assert!(entries[index].label.ends_with(expected));
        assert!(matches!(ui.palette_run(index), Input::Idle));
        assert!(matches!(&ui.prompt, Some(Prompt::ForceStart(name)) if name == expected));
        assert!(!ui.confirmation_accept);
        press(&mut ui, KeyCode::Esc);
    }
    ui.rows.iter_mut().for_each(|row| row.pid = Some(42));
    assert!(!ui
        .palette_entries()
        .iter()
        .any(|entry| matches!(entry.run, Run::ForceStart)));
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
