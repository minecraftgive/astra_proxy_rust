use super::*;

const NODES: &str = "[[outbounds]]\ntag='first'\nkind='socks5'\nserver='127.0.0.1:1080'\n[[outbounds]]\ntag='second'\nkind='socks5'\nserver='127.0.0.1:1081'\n";

fn key(code: KeyCode) -> event::KeyEvent {
    event::KeyEvent::new(code, KeyModifiers::NONE)
}

fn ui() -> Ui {
    let mut ui = Ui::new(Session::default());
    ui.page = PROFILES;
    let mut editor =
        Editor::new("proxy".into(), format!("standalone=true\n{NODES}")).with_kind(Kind::Proxy);
    for _ in 0..100 {
        if editor.selected_node_tag().ok().as_deref() == Some("first") {
            break;
        }
        editor.key(crate::input::key(key(KeyCode::Down))).unwrap();
    }
    assert_eq!(editor.selected_node_tag().unwrap(), "first");
    editor
        .key(crate::input::key(key(KeyCode::Char('V'))))
        .unwrap();
    ui.editor = Some(editor);
    ui
}

#[test]
fn node_transfer_exports_selected_nodes_to_new_folders_and_never_overwrites() {
    let fixture = crate::import::tests::Fixture::new();
    let mut ui = ui();
    let original = ui.editor.as_ref().unwrap().text();
    ui.key(event::KeyEvent::new(
        KeyCode::Char('e'),
        KeyModifiers::CONTROL,
    ));
    assert!(matches!(
        ui.prompt,
        Some(Prompt::ExportNodeDestination {
            clipboard: false,
            ..
        })
    ));
    ui.key(key(KeyCode::Enter));
    let documents = astra_platform::platform::documents_dir().unwrap();
    assert_eq!(ui.working_dir, documents);
    assert_eq!(PathBuf::from(&ui.input), documents.join("nodes.toml"));
    assert!(ui.prefilled_input);
    let prompt = ui.prompt.take().unwrap();
    assert!(matches!(prompt, Prompt::ExportNode { share: false, .. }));
    ui.working_dir = fixture.0.clone();
    ui.submit(prompt.clone(), "\"新目录/with spaces\"".into())
        .unwrap();
    let file = fixture.0.join("新目录/with spaces/nodes.toml");
    let text = std::fs::read_to_string(&file).unwrap();
    let value: toml::Value = toml::from_str(&text).unwrap();
    assert_eq!(value["outbounds"].as_array().unwrap().len(), 2);
    assert!(ui.submit(prompt, file.display().to_string()).is_err());
    assert_eq!(std::fs::read_to_string(file).unwrap(), text);
    assert_eq!(ui.editor.as_ref().unwrap().text(), original);
    assert_eq!(
        export_path("new/folder/", &fixture.0, true).unwrap(),
        fixture.0.join("new/folder/nodes.txt")
    );
}

#[test]
fn node_transfer_clipboard_uses_selected_share_links_and_reports_failure() {
    let mut ui = ui();
    ui.key(event::KeyEvent::new(
        KeyCode::Char('e'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    ));
    assert!(matches!(
        ui.prompt,
        Some(Prompt::ExportNodeDestination { share: true, .. })
    ));
    ui.key(key(KeyCode::Tab));
    let mut copied = String::new();
    ui.key_node_transfer_with(key(KeyCode::Enter), |text| {
        copied = text.into();
        Ok(())
    });
    assert_eq!(copied.lines().count(), 2);
    assert!(copied.lines().all(|line| line.starts_with("socks5://")));
    assert!(ui.prompt.is_none());
    assert_eq!(ui.message, "Selected nodes copied to clipboard");

    ui.start_node_export(false);
    ui.key(key(KeyCode::Tab));
    ui.message.clear();
    ui.key_node_transfer_with(key(KeyCode::Enter), |_| anyhow::bail!("clipboard busy"));
    assert!(ui
        .recovery
        .as_ref()
        .unwrap()
        .message
        .contains("clipboard busy"));
    assert!(ui.message.is_empty());
    ui.recover_key(KeyCode::Char('c'));
    assert!(matches!(
        ui.prompt,
        Some(Prompt::ExportNodeDestination {
            clipboard: true,
            ..
        })
    ));
    ui.key(key(KeyCode::Esc));
    assert!(ui.prompt.is_none());
}

#[test]
fn node_transfer_multiline_import_reviews_selection_and_keeps_cancel_and_undo() {
    let mut ui = ui();
    ui.editor = Some(
        Editor::new(
            "proxy".into(),
            "standalone=true\n[[outbounds]]\ntag='first'\nkind='direct'\n".into(),
        )
        .with_kind(Kind::Proxy),
    );
    let before = ui.editor.as_ref().unwrap().text();
    ui.prompt(Prompt::ImportNode(crate::import::MergeMode::Append));
    ui.paste_prompt(NODES);
    assert_eq!(ui.input, NODES);
    ui.key(key(KeyCode::Enter));
    assert!(
        matches!(&ui.prompt, Some(Prompt::ImportNodeSelection { selected, .. }) if selected == &[0, 1])
    );
    assert_eq!(ui.editor.as_ref().unwrap().text(), before);
    ui.key(key(KeyCode::Esc));
    assert_eq!(ui.editor.as_ref().unwrap().text(), before);

    ui.prepare_node_import(NODES, crate::import::MergeMode::Append)
        .unwrap();
    ui.key(key(KeyCode::Enter));
    assert!(
        ui.recovery.is_some(),
        "conflicting selected tag must be rejected"
    );
    assert_eq!(ui.editor.as_ref().unwrap().text(), before);
    ui.recover_key(KeyCode::Char('c'));
    ui.key(key(KeyCode::Char(' ')));
    ui.key(key(KeyCode::Enter));
    assert!(ui.recovery.is_none());
    let doc: toml::Value = toml::from_str(&ui.editor.as_ref().unwrap().text()).unwrap();
    assert_eq!(doc["outbounds"].as_array().unwrap().len(), 2);
    assert_eq!(doc["outbounds"][0]["kind"].as_str(), Some("direct"));
    assert_eq!(doc["outbounds"][1]["tag"].as_str(), Some("second"));
    ui.editor
        .as_mut()
        .unwrap()
        .key(crate::input::key(event::KeyEvent::new(
            KeyCode::Char('z'),
            KeyModifiers::CONTROL,
        )))
        .unwrap();
    assert_eq!(ui.editor.as_ref().unwrap().text(), before);
}

#[test]
fn quoted_reality_batch_pastes_and_imports_selected_nodes_from_both_shortcuts() {
    const PUBLIC_KEY: &str = "AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA";
    let encoded_labels = [
        "%F0%9F%8C%8F%20%E9%A6%99%E6%B8%AF",
        "%F0%9F%9A%80%20%E6%97%A5%E6%9C%AC",
        "%F0%9F%8C%9F%20%E6%96%B0%E5%8A%A0%E5%9D%A1",
    ];
    let lines = encoded_labels.iter().enumerate().map(|(index, label)| {
        let number = index + 1;
        format!("vless://00000000-0000-4000-8000-00000000000{number}@edge{number}.example.invalid:443?encryption=none&headerType=&spx=%2F&fp=chrome&type=tcp&flow=xtls-rprx-vision&security=reality&sni=front.example.invalid&sid=abcd&pbk={PUBLIC_KEY}#{label}")
    }).collect::<Vec<_>>().join("\n");
    let batch = format!("\"{lines}\"");
    for shortcut in ['i', 'o'] {
        let mut ui = ui();
        let before = ui.editor.as_ref().unwrap().text();
        ui.key(event::KeyEvent::new(
            KeyCode::Char(shortcut),
            KeyModifiers::CONTROL,
        ));
        assert!(matches!(
            (&ui.prompt, shortcut),
            (Some(Prompt::ImportNode(_)), 'i') | (Some(Prompt::Import), 'o')
        ));
        ui.paste_prompt(&batch);
        assert_eq!(
            ui.input, batch,
            "Ctrl-{shortcut} must preserve all line breaks"
        );
        ui.key(key(KeyCode::Enter));
        assert!(ui.recovery.is_none(), "Ctrl-{shortcut}: {}", ui.message);
        let Some(Prompt::ImportNodeSelection {
            candidates,
            selected,
            ..
        }) = &ui.prompt
        else {
            panic!("Ctrl-{shortcut} did not open the node selection dialog");
        };
        assert_eq!(candidates.tags, ["🌏 香港", "🚀 日本", "🌟 新加坡"]);
        assert_eq!(selected, &[0, 1, 2]);
        let preview: toml::Value = toml::from_str(&candidates.text).unwrap();
        assert_eq!(ui.editor.as_ref().unwrap().text(), before);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| ui.render(frame)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        let rendered = rendered.replace(' ', "");
        for label in ["🌏香港", "🚀日本", "🌟新加坡"] {
            assert!(rendered.contains(label), "{rendered}");
        }

        ui.key(key(KeyCode::Down));
        ui.key(key(KeyCode::Char(' ')));
        ui.key(key(KeyCode::Enter));
        assert!(
            ui.prompt.is_none() && ui.recovery.is_none(),
            "{}",
            ui.message
        );
        let document: toml::Value = toml::from_str(&ui.editor.as_ref().unwrap().text()).unwrap();
        let nodes = document["outbounds"].as_array().unwrap();
        assert_eq!(nodes.len(), 4);
        assert_eq!(nodes[0]["tag"].as_str(), Some("first"));
        assert_eq!(nodes[1]["tag"].as_str(), Some("second"));
        assert_eq!(nodes[2], preview["outbounds"][0]);
        assert_eq!(nodes[3], preview["outbounds"][2]);
    }
}

#[test]
fn node_transfer_picklists_render_in_chinese_and_reject_empty_or_changed_targets() {
    crate::i18n::with_language(crate::i18n::Language::Simplified, || {
        let mut ui = ui();
        ui.start_node_export(false);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(90, 24)).unwrap();
        terminal.draw(|frame| ui.render(frame)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.replace(' ', "").contains("剪贴板"), "{screen}");
        ui.prepare_node_import(NODES, crate::import::MergeMode::Append)
            .unwrap();
        for (width, height) in [(20, 6), (90, 24)] {
            let mut terminal =
                Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| ui.render(frame)).unwrap();
        }
        ui.key(key(KeyCode::Char('a')));
        let prompt = ui.prompt.as_ref().unwrap().clone();
        assert!(ui.apply_node_import(&prompt).is_err());
        ui.key(key(KeyCode::Char('a')));
        let prompt = ui.prompt.as_ref().unwrap().clone();
        ui.editor.as_mut().unwrap().name = "different".into();
        assert!(ui
            .apply_node_import(&prompt)
            .unwrap_err()
            .to_string()
            .contains("target changed"));
    });
}

#[test]
fn node_transfer_selection_is_bound_to_config_path_and_ignores_repeat_toggles() {
    let mut ui = ui();
    ui.rows.push(Row {
        instance: Instance {
            name: "proxy".into(),
            kind: Kind::Proxy,
            config: "first.toml".into(),
        },
        pid: None,
        state: "Stopped".into(),
        uptime: 0,
        changed: false,
    });
    ui.prepare_node_import(NODES, crate::import::MergeMode::Append)
        .unwrap();
    let mut held = key(KeyCode::Char(' '));
    held.kind = KeyEventKind::Repeat;
    ui.key(held);
    assert!(
        matches!(&ui.prompt, Some(Prompt::ImportNodeSelection { selected, .. }) if selected == &[0, 1])
    );
    assert!(matches!(
        ui.key(event::KeyEvent::new(
            KeyCode::Char('s'),
            KeyModifiers::CONTROL
        )),
        Input::Idle
    ));
    assert!(matches!(
        ui.prompt,
        Some(Prompt::ImportNodeSelection { .. })
    ));
    let prompt = ui.prompt.as_ref().unwrap().clone();
    ui.rows[0].instance.config = "different.toml".into();
    assert!(ui
        .apply_node_import(&prompt)
        .unwrap_err()
        .to_string()
        .contains("target changed"));
    ui.key(key(KeyCode::Esc));
    crate::i18n::with_language(crate::i18n::Language::English, || {
        let help = ui.command_help();
        assert!(help.contains("Import nodes"));
        assert!(help.contains("Export selected nodes (TOML)"));
        assert!(help.contains("Export selected nodes (share links)"));
    });
}
