use super::*;
use crate::protocol::Kind as CoreKind;
use astra_app::schema::Row;
use astra_client::input::{KeyCode, KeyEvent, KeyModifiers};
use astra_client::text_input::TextInput;
use astra_model::document::DocumentPath as Path;
use ratatui::{backend::TestBackend, Terminal};

mod node_selection_tests {
    use super::*;

    fn editor() -> Editor {
        Editor::new(
            "proxy".into(),
            format!(
                "standalone=true\n{}",
                ["first", "second", "third"]
                    .into_iter()
                    .enumerate()
                    .map(|(index, tag)| format!(
                        "[[outbounds]]\ntag='{tag}'\nkind='socks5'\nserver='127.0.0.1:{}'\n",
                        1234 + index
                    ))
                    .collect::<String>()
            ),
        )
    }

    fn focus(editor: &mut Editor, index: usize) {
        let doc = editor.document().unwrap();
        editor.focus_item(&doc, "outbounds", index);
    }

    fn mark(editor: &mut Editor, index: usize) {
        focus(editor, index);
        press(editor, KeyCode::Char('v'));
    }

    fn exported_tags(editor: &Editor) -> Vec<String> {
        let document: toml::Value =
            toml::from_str(&editor.export_selected_node().unwrap()).unwrap();
        document["outbounds"]
            .as_array()
            .unwrap()
            .iter()
            .map(|node| node["tag"].as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn node_selection_exports_marked_set_in_source_order_and_toggles_all() {
        let mut editor = editor();
        let original = editor.text();
        mark(&mut editor, 2);
        mark(&mut editor, 0);
        assert_eq!(editor.form.marked_nodes, ["third", "first"]);
        focus(&mut editor, 1);
        assert_eq!(editor.selected_node_tags().unwrap(), ["first", "third"]);
        assert_eq!(exported_tags(&editor), ["first", "third"]);
        let links = editor.export_selected_node_share().unwrap();
        let links: Vec<_> = links.lines().collect();
        assert_eq!(links.len(), 2);
        assert!(links[0].ends_with("#first"), "{}", links[0]);
        assert!(links[1].ends_with("#third"), "{}", links[1]);

        press(&mut editor, KeyCode::Char('V'));
        assert_eq!(exported_tags(&editor), ["first", "second", "third"]);
        press(&mut editor, KeyCode::Char('V'));
        assert!(editor.form.marked_nodes.is_empty());
        assert_eq!(exported_tags(&editor), ["second"]);
        press(&mut editor, KeyCode::Char('v'));
        press(&mut editor, KeyCode::Char('v'));
        assert!(editor.form.marked_nodes.is_empty());
        assert_eq!(editor.text(), original, "Selection must not edit the draft");
    }

    #[test]
    fn node_selection_survives_folding_but_does_not_mark_child_rows() {
        let mut editor = editor();
        mark(&mut editor, 2);
        press(&mut editor, KeyCode::Enter);
        press(&mut editor, KeyCode::Down);
        assert!(editor.selected_node_tag().is_err());
        press(&mut editor, KeyCode::Char('v'));
        press(&mut editor, KeyCode::Char('V'));
        assert_eq!(editor.form.marked_nodes, ["third"]);
        assert_eq!(exported_tags(&editor), ["third"]);
        focus(&mut editor, 2);
        press(&mut editor, KeyCode::Enter);
        assert_eq!(editor.form.marked_nodes, ["third"]);
    }

    #[test]
    fn node_selection_clears_after_edits_renames_reordering_duplication_and_deletion() {
        for operation in ["edit", "rename", "move", "duplicate", "delete"] {
            let mut editor = editor();
            mark(&mut editor, 2);
            mark(&mut editor, 0);
            match operation {
                "edit" | "rename" => {
                    editor.form.expanded_nodes.push(0);
                    editor.form.path = vec![Part::Key("outbounds".into()), Part::Index(0)];
                    if operation == "edit" {
                        field(&mut editor, "server", "127.0.0.1:9876");
                    } else {
                        field(&mut editor, "tag", "renamed");
                    }
                }
                "move" => {
                    focus(&mut editor, 2);
                    press(&mut editor, KeyCode::Char('K'));
                }
                "duplicate" => press(&mut editor, KeyCode::Char('y')),
                "delete" => press(&mut editor, KeyCode::Char('d')),
                _ => unreachable!(),
            }
            assert!(editor.form.marked_nodes.is_empty(), "{operation}");
        }
    }

    #[test]
    fn node_selection_clears_after_undo_redo_import_and_session_restore() {
        let mut editor = editor();
        mark(&mut editor, 2);
        press(&mut editor, KeyCode::Char('K'));
        mark(&mut editor, 0);
        editor.restore(false).unwrap();
        assert!(editor.form.marked_nodes.is_empty());
        mark(&mut editor, 0);
        editor.restore(true).unwrap();
        assert!(editor.form.marked_nodes.is_empty());
        mark(&mut editor, 0);
        let restored: Editor =
            serde_json::from_str(&serde_json::to_string(&editor).unwrap()).unwrap();
        assert!(restored.form.marked_nodes.is_empty());
        editor
            .import(editor.text().replace("first", "imported"))
            .unwrap();
        assert!(editor.form.marked_nodes.is_empty());
    }

    #[test]
    fn node_selection_rejects_missing_tags_instead_of_exporting_the_cursor() {
        let mut editor = editor();
        focus(&mut editor, 0);
        editor.form.marked_nodes = vec!["third".into(), "removed".into()];
        assert!(editor.selected_node_tags().is_err());
        assert!(editor.export_selected_node().is_err());
        assert!(editor.export_selected_node_share().is_err());
    }

    #[test]
    fn node_selection_renders_count_markers_and_selection_help() {
        crate::i18n::with_language(crate::i18n::Language::English, || {
            let mut editor = editor();
            mark(&mut editor, 2);
            mark(&mut editor, 0);
            let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
            terminal
                .draw(|frame| editor.render(frame, frame.area()))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let rows: Vec<String> = (0..40)
                .map(|y| (0..120).map(|x| buffer[(x, y)].symbol()).collect())
                .collect();
            let text = rows.join("\n");
            assert!(text.contains("2 nodes selected"), "{text}");
            assert!(text.contains("V all/clear"), "{text}");
            let marked: Vec<_> = rows.iter().filter(|row| row.contains('✓')).collect();
            assert_eq!(marked.len(), 2, "{text}");
            assert!(marked.iter().any(|row| row.contains("first")));
            assert!(marked.iter().any(|row| row.contains("third")));
        });
    }
}

#[test]
fn rule_set_management_renames_references_toggles_and_blocks_dangling_deletion() {
    let mut editor = Editor::new("rules".into(), "socket='r'\ndefault_action='direct'\nrules=[{kind='rule_set',value='ads',action='block'},{kind='rule_set',value='ads',action='direct'}]\n[rule_sets.ads]\nbehavior='domain'\npayload=['+.ads.test']\n[rule_sets.other]\npayload=[]\n".into());
    let original = editor.text();
    assert!(editor.rename_rule_set("ads", "other").is_err());
    assert_eq!(editor.text(), original);
    let doc = editor.document().unwrap();
    let set_path = vec![Part::Key("rule_sets".into()), Part::Key("ads".into())];
    editor.focus_field(&doc, &set_path, "enabled");
    press(&mut editor, KeyCode::Char('r'));
    editor.paste("renamed");
    let saved = serde_json::to_string(&editor).unwrap();
    editor = serde_json::from_str(&saved).unwrap();
    press(&mut editor, KeyCode::Enter);
    let doc = editor.document().unwrap();
    assert!(doc["rule_sets"].get("ads").is_none());
    assert_eq!(rule_set_references(&doc, Some("renamed")), 2);
    assert_eq!(doc["rules"][0]["action"].as_str(), Some("block"));
    press(&mut editor, KeyCode::Char('e'));
    assert_eq!(
        editor.document().unwrap()["rule_sets"]["renamed"]["enabled"].as_bool(),
        Some(false)
    );
    let doc = editor.document().unwrap();
    editor.form.selected = editor.visible_rows(&doc).iter().position(|row| matches!(row, FlatRow::Section { path, .. } if rule_set_name(path) == Some("renamed"))).unwrap();
    assert!(editor.key(key(KeyCode::Char('d'))).is_err());
    assert_eq!(
        rule_set_references(&editor.document().unwrap(), Some("renamed")),
        2
    );
    press(&mut editor, KeyCode::Char('r'));
    editor.paste("cancelled");
    press(&mut editor, KeyCode::Esc);
    assert!(editor.document().unwrap()["rule_sets"]
        .get("renamed")
        .is_some());
    for language in [
        crate::i18n::Language::English,
        crate::i18n::Language::Simplified,
        crate::i18n::Language::Traditional,
    ] {
        crate::i18n::with_language(language, || {
            let mut terminal = Terminal::new(TestBackend::new(56, 12)).unwrap();
            terminal
                .draw(|frame| editor.render(frame, frame.area()))
                .unwrap();
        });
    }
}
fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}
fn press(editor: &mut Editor, code: KeyCode) {
    editor.key(key(code)).unwrap();
}
fn field(editor: &mut Editor, key: &str, text: &str) {
    let doc = editor.document().unwrap();
    let path = editor.form.path.clone();
    editor.focus_field(&doc, &path, key);
    assert!(
        matches!(editor.visible_rows(&doc).get(editor.form.selected), Some(FlatRow::Field{path:p,spec,..}) if p == &path && spec.key == key)
    );
    if let Some(FlatRow::Field { spec, .. }) = editor.visible_rows(&doc).get(editor.form.selected) {
        if matches!(spec.kind, Type::Boolean) {
            let current = table_at(doc.as_table(), &path)
                .and_then(|t| t.get(key))
                .map(scalar)
                .unwrap_or(spec.default.clone());
            if current != text {
                press(editor, KeyCode::Enter);
            }
            return;
        }
    }
    press(editor, KeyCode::Enter);
    editor.paste(text);
    press(editor, KeyCode::Enter);
}
fn native() -> Editor {
    Editor::new(
        "proxy".into(),
        include_str!("../../../../configs/native.toml").into(),
    )
}
fn node_path() -> Path {
    vec![Part::Key("outbounds".into()), Part::Index(0)]
}
fn open_field(editor: &mut Editor, name: &str) {
    let doc = editor.document().unwrap();
    let path = editor.form.path.clone();
    editor.focus_field(&doc, &path, name);
    press(editor, KeyCode::Enter);
}
#[test]
fn proxy_group_picker_selection_renaming_and_references_survive_draft_restore() {
    let mut e = Editor::new("p".into(), "standalone=true\nstandalone_outbound='main'\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\ntag='a'\nkind='socks5'\nserver='127.0.0.1:1081'\n[[outbounds]]\ntag='b'\nkind='socks5'\nserver='127.0.0.1:1082'\n[proxy_groups.main]\nmode='select'\nmembers=['a','b']\nselected='b'".into());
    e.form.path = vec![Part::Key("proxy_groups".into()), Part::Key("main".into())];
    for flag in [
        "disconnect_on_manual",
        "disconnect_on_auto",
        "disconnect_on_failure",
        "disconnect_on_recovery",
        "disconnect_on_config_change",
    ] {
        field(&mut e, flag, "true");
        assert_eq!(
            e.document().unwrap()["proxy_groups"]["main"][flag].as_bool(),
            Some(true)
        );
    }
    field(&mut e, "selected", "a");
    assert_eq!(
        e.document().unwrap()["proxy_groups"]["main"]["selected"].as_str(),
        Some("a")
    );
    field(&mut e, "mode", "auto");
    assert!(!e
        .rows(&e.document().unwrap())
        .iter()
        .any(|row| matches!(row,Row::Field(s) if s.key=="selected")));
    field(&mut e, "mode", "select");
    let doc = e.document().unwrap();
    let path = e.form.path.clone();
    e.focus_field(&doc, &path, "members");
    press(&mut e, KeyCode::Enter);
    if let Mode::Members {
        options, cursor, ..
    } = &mut e.form.mode
    {
        *cursor = options.iter().position(|v| v == "a").unwrap();
    } else {
        panic!("expected member picker");
    }
    press(&mut e, KeyCode::Char(' '));
    let encoded = serde_json::to_string(&e).unwrap();
    let mut e: Editor = serde_json::from_str(&encoded).unwrap();
    e.normalize();
    press(&mut e, KeyCode::Enter);
    assert_eq!(
        e.document().unwrap()["proxy_groups"]["main"]["selected"].as_str(),
        Some("b")
    );
    press(&mut e, KeyCode::Char('r'));
    e.paste("renamed");
    press(&mut e, KeyCode::Enter);
    assert_eq!(
        e.document().unwrap()["standalone_outbound"].as_str(),
        Some("renamed")
    );
    assert!(e.document().unwrap()["proxy_groups"].get("main").is_none());
    e.form.path = vec![Part::Key("outbounds".into()), Part::Index(1)];
    field(&mut e, "tag", "new-b");
    assert_eq!(
        e.document().unwrap()["proxy_groups"]["renamed"]["members"][0].as_str(),
        Some("new-b")
    );
    assert_eq!(
        e.document().unwrap()["proxy_groups"]["renamed"]["selected"].as_str(),
        Some("new-b")
    );
    let path = vec![
        Part::Key("proxy_groups".into()),
        Part::Key("renamed".into()),
    ];
    e.form.selected = e
        .visible_rows(&e.document().unwrap())
        .iter()
        .position(|r| r.path() == path)
        .unwrap();
    assert!(
        e.key(key(KeyCode::Char('d'))).is_err(),
        "default group cannot be deleted"
    );
    for language in [
        crate::i18n::Language::English,
        crate::i18n::Language::Simplified,
        crate::i18n::Language::Traditional,
    ] {
        crate::i18n::with_language(language, || {
            let doc = e.document().unwrap();
            e.focus_field(&doc, &path, "members");
            press(&mut e, KeyCode::Enter);
            for (width, height) in [(100, 24), (30, 8)] {
                let mut terminal =
                    ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                        .unwrap();
                terminal
                    .draw(|frame| e.render(frame, frame.area()))
                    .unwrap();
            }
            press(&mut e, KeyCode::Esc);
        });
    }
}
#[test]
fn reference_popup_keeps_selected_candidate_visible_in_all_locales() {
    let mut e = Editor::new(
        "rules".into(),
        "socket='r'\ndefault_action='direct'\n".into(),
    )
    .with_kind(CoreKind::Rule);
    e.set_node_tags((0..30).map(|i| format!("node-{i:02}")).collect());
    open_field(&mut e, "default_action");
    for _ in 0..25 {
        press(&mut e, KeyCode::Down);
    }
    let candidate = match &e.form.mode {
        Mode::Field { input, .. } => input.candidate.clone().unwrap(),
        _ => unreachable!(),
    };
    for lang in [
        crate::i18n::Language::English,
        crate::i18n::Language::Simplified,
        crate::i18n::Language::Traditional,
    ] {
        crate::i18n::with_language(lang, || {
            for (width, height) in [(100, 28), (45, 14), (20, 8)] {
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal.draw(|f| e.render(f, f.area())).unwrap();
                if width == 100 {
                    let text: String = terminal
                        .backend()
                        .buffer()
                        .content
                        .iter()
                        .map(|c| c.symbol())
                        .collect();
                    assert!(text.contains(&candidate), "{text}");
                }
            }
        });
    }
}
fn path(keys: &[&str]) -> Path {
    keys.iter().map(|k| Part::Key((*k).into())).collect()
}

#[test]
fn egress_adapter_picker_selects_physical_virtual_and_automatic() {
    let mut editor = native();
    editor.form.path = path(&["tun"]);
    let doc = editor.document().unwrap();
    editor.focus_field(&doc, &path(&["tun"]), "interface");
    press(&mut editor, KeyCode::Enter);
    // Use deterministic names to exercise both physical and virtual adapters.
    editor.interfaces = vec!["Ethernet".into(), "虚拟网卡 VPN".into()];
    let doc = editor.document().unwrap();
    let values = editor
        .suggestions(&doc, "interface", &TextInput::new(String::new()))
        .unwrap();
    assert!(values.contains(&"Ethernet".into()));
    assert!(values.contains(&"虚拟网卡 VPN".into()));
    editor.paste("虚拟");
    press(&mut editor, KeyCode::Down);
    press(&mut editor, KeyCode::Enter);
    let doc = editor.document().unwrap();
    assert_eq!(doc["tun"]["interface"].as_str(), Some("虚拟网卡 VPN"));
    assert_eq!(doc["tun"]["auto_interface"].as_bool(), Some(false));

    editor.focus_field(&doc, &path(&["tun"]), "interface");
    press(&mut editor, KeyCode::Enter);
    editor.paste("not-a-local-adapter");
    assert!(editor.key(key(KeyCode::Enter)).is_err());
    press(&mut editor, KeyCode::Esc);
    assert_eq!(
        editor.document().unwrap()["tun"]["interface"].as_str(),
        Some("虚拟网卡 VPN")
    );

    let doc = editor.document().unwrap();
    editor.focus_field(&doc, &path(&["tun"]), "interface");
    press(&mut editor, KeyCode::Enter);
    editor
        .key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL))
        .unwrap();
    for language in [
        crate::i18n::Language::English,
        crate::i18n::Language::Simplified,
        crate::i18n::Language::Traditional,
    ] {
        crate::i18n::with_language(language, || {
            let mut terminal = Terminal::new(TestBackend::new(76, 20)).unwrap();
            terminal
                .draw(|frame| editor.render(frame, frame.area()))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
            let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
            let label: String = t("Automatic interface")
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect();
            assert!(compact.contains(&label), "{text}");
        });
    }
    press(&mut editor, KeyCode::Enter);
    let doc = editor.document().unwrap();
    assert!(doc["tun"].get("interface").is_none());
    assert_eq!(doc["tun"]["auto_interface"].as_bool(), Some(true));
}

#[test]
fn localized_forms_keep_credentials_hidden_and_handle_small_screens() {
    use crate::i18n::{with_language, Language};
    for (language, title, host) in [
        (Language::English, "Configuration", "Server host"),
        (Language::Simplified, "编辑", "服务器主机"),
        (Language::Traditional, "編輯", "伺服器主機"),
    ] {
        with_language(language, || {
            let mut e = native();
            e.form.path = node_path();
            e.form.path.extend(path(&["config"]));
            field(&mut e, "uuid", "do-not-show-this-secret");
            let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
            terminal
                .draw(|frame| e.render(frame, frame.area()))
                .unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            assert!(
                text.replace(' ', "").contains(&title.replace(' ', "")),
                "{text}"
            );
            let doc = e.document().unwrap();
            assert!(e.schema_rows(&doc, &e.form.path).iter().any(
                |row| matches!(row, Row::Field(spec) if spec.key == "server" && spec.label == host)
            ));
            assert!(!text.contains("do-not-show-this-secret"));
            let mut tiny = Terminal::new(TestBackend::new(20, 8)).unwrap();
            tiny.draw(|frame| e.render(frame, frame.area())).unwrap();
        });
    }
}

#[test]
fn visual_form_masks_credentials_and_supports_small_terminals() {
    let mut e = native();
    e.form.path = node_path();
    e.form.path.extend(path(&["config"]));
    field(&mut e, "uuid", "private-credential-for-display-test");
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    terminal.draw(|f| e.render(f, f.area())).unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(text.replace(' ', "").contains("编辑"));
    assert!(text.replace(' ', "").contains("服务器主机"));
    assert!(!text.contains("private-credential"));
    let doc = e.document().unwrap();
    let path = e.form.path.clone();
    e.focus_field(&doc, &path, "uuid");
    press(&mut e, KeyCode::Enter);
    terminal.draw(|f| e.render(f, f.area())).unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(!text.contains("private-credential"));
    press(&mut e, KeyCode::F(2));
    terminal.draw(|f| e.render(f, f.area())).unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(text.contains("private-credential"));
    let mut tiny = Terminal::new(TestBackend::new(12, 6)).unwrap();
    tiny.draw(|f| e.render(f, f.area())).unwrap();
}
#[tokio::test]
#[ignore = "run after cargo build --workspace"]
async fn live_visual_outbound_templates_pass_core_validation() {
    let directory = std::env::temp_dir().join(format!("astra-form-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    astra_platform::platform::set_private(&directory, 0o700).unwrap();
    let bin =
        crate::manager::test_binaries().join(astra_platform::platform::executable("astra-proxy"));
    for (selected, kind) in OUTBOUND_KINDS.iter().enumerate() {
        let mut e = Editor::new(
            "proxy".into(),
            include_str!("../../../../configs/proxy.toml").into(),
        );
        e.form.path = path(&["outbounds"]);
        e.form.mode = Mode::NewOutbound(selected);
        press(&mut e, KeyCode::Enter);
        let file = directory.join(format!("node-{selected}.toml"));
        std::fs::write(&file, e.prepare_save().unwrap()).unwrap();
        let output = tokio::process::Command::new(&bin)
            .arg("--check")
            .arg(&file)
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}: {}{}",
            kind,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(test)]
mod runtime_profile_tests {
    use super::*;
    #[test]
    fn saving_removes_runtime_switch_but_keeps_network_parameters_and_conflict_base() {
        let source="standalone=true\nallow_lan=true\nhttp_listen='127.0.0.1:8080'\n[system_proxy]\n[tun]\nmtu=1400\n";
        let mut editor = Editor::new("proxy".into(), source.into()).with_kind(CoreKind::Proxy);
        let doc = editor.document().unwrap();
        assert!(!editor
            .schema_rows(&doc, &[])
            .iter()
            .any(|row| matches!(row,Row::Field(spec)if spec.key=="allow_lan")));
        let text = editor.prepare_save().unwrap();
        let saved: toml::Value = toml::from_str(&text).unwrap();
        assert!(saved.get("allow_lan").is_none());
        assert_eq!(saved["tun"]["mtu"].as_integer(), Some(1400));
        assert!(saved.get("system_proxy").is_some());
        assert_eq!(editor.original, source);
    }
}
#[cfg(test)]
mod interaction_tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};
    fn press(editor: &mut Editor, code: KeyCode) {
        editor.key(KeyEvent::new(code, KeyModifiers::NONE)).unwrap();
    }
    fn ctrl(editor: &mut Editor, c: char) {
        editor
            .key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
            .unwrap();
    }
    fn screen(editor: &Editor, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| editor.render(f, f.area())).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .replace(' ', "")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    fn proxy() -> Editor {
        Editor::new(
            "proxy".into(),
            include_str!("../../../../configs/native.toml").into(),
        )
        .with_kind(CoreKind::Proxy)
    }
    #[test]
    fn booleans_toggle_in_place_and_every_edit_can_be_undone_and_redone() {
        let mut e = proxy();
        let original = e.text();
        let doc = e.document().unwrap();
        e.focus_field(&doc, &[], "watch_config");
        press(&mut e, KeyCode::Char(' '));
        assert!(matches!(e.form.mode, Mode::Browse));
        assert_eq!(e.document().unwrap()["watch_config"].as_bool(), Some(true));
        press(&mut e, KeyCode::Enter);
        assert_eq!(e.document().unwrap()["watch_config"].as_bool(), Some(false));
        press(&mut e, KeyCode::Char('u'));
        assert_eq!(e.document().unwrap()["watch_config"].as_bool(), Some(true));
        ctrl(&mut e, 'z');
        assert_eq!(e.text(), original);
        press(&mut e, KeyCode::Char('u'));
        assert_eq!(e.text(), original);
        ctrl(&mut e, 'y');
        ctrl(&mut e, 'y');
        assert_eq!(e.document().unwrap()["watch_config"].as_bool(), Some(false));
        // A new edit drops the redo branch.
        press(&mut e, KeyCode::Char('u'));
        press(&mut e, KeyCode::Enter);
        ctrl(&mut e, 'y');
        assert_eq!(e.document().unwrap()["watch_config"].as_bool(), Some(false));
    }
    #[test]
    fn deleting_is_immediate_and_reverts_with_undo() {
        let mut e = proxy();
        let original = e.text();
        let doc = e.document().unwrap();
        e.focus_field(&doc, &[], "connect_timeout_secs");
        press(&mut e, KeyCode::Char('d'));
        assert!(matches!(e.form.mode, Mode::Browse));
        assert!(e.document().unwrap().get("connect_timeout_secs").is_none());
        assert!(screen(&e, 100, 30).contains("u/Ctrl-Z"));
        press(&mut e, KeyCode::Char('u'));
        assert_eq!(e.text(), original);
        // A blocked delete leaves no confirmation dialog behind.
        let doc = e.document().unwrap();
        e.focus_field(&doc, &[], "rule_socket");
        assert!(e
            .key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
            .is_err());
        assert!(matches!(e.form.mode, Mode::Browse));
    }
    #[test]
    fn new_node_walks_through_template_values_then_stops() {
        let mut e = proxy();
        e.form.path = vec![Part::Key("outbounds".into())];
        press(&mut e, KeyCode::Char('n'));
        press(&mut e, KeyCode::Down);
        press(&mut e, KeyCode::Down);
        press(&mut e, KeyCode::Enter);
        let mut opened = Vec::new();
        for value in [
            "hk-1",
            "hk.example.net",
            "8443",
            "11111111-2222-3333-4444-555555555555",
            "hk.example.net",
        ] {
            let Mode::Field { key, .. } = &e.form.mode else {
                panic!("guided filling stopped early after {opened:?}");
            };
            opened.push(key.clone());
            e.paste(value);
            press(&mut e, KeyCode::Enter);
        }
        assert_eq!(opened, ["tag", "server", "port", "uuid", "server_name"]);
        assert!(matches!(e.form.mode, Mode::Browse));
        assert!(e.form.guide.is_none());
        let doc = e.document().unwrap();
        let node = &doc["outbounds"][1];
        assert_eq!(node["tag"].as_str(), Some("hk-1"));
        assert_eq!(node["config"]["server"].as_str(), Some("hk.example.net"));
        assert_eq!(
            node["config"]["tls"]["server_name"].as_str(),
            Some("hk.example.net")
        );
        // Esc ends the walk and keeps what was entered so far.
        press(&mut e, KeyCode::Char('n'));
        press(&mut e, KeyCode::Enter);
        e.paste("socks-a");
        press(&mut e, KeyCode::Enter);
        assert!(matches!(&e.form.mode, Mode::Field { key, .. } if key == "server"));
        press(&mut e, KeyCode::Esc);
        assert!(e.form.guide.is_none());
        assert_eq!(
            e.document().unwrap()["outbounds"][2]["tag"].as_str(),
            Some("socks-a")
        );
    }
    #[test]
    fn new_rule_opens_kind_value_and_action_in_turn() {
        let mut e = Editor::new(
            "rules".into(),
            include_str!("../../../../configs/rule.toml").into(),
        )
        .with_kind(CoreKind::Rule);
        e.form.path = vec![Part::Key("rules".into())];
        press(&mut e, KeyCode::Char('n'));
        press(&mut e, KeyCode::Enter);
        e.paste("video.example");
        press(&mut e, KeyCode::Enter);
        e.paste("block");
        press(&mut e, KeyCode::Enter);
        assert!(matches!(e.form.mode, Mode::Browse));
        let doc = e.document().unwrap();
        let rules = doc["rules"].as_array_of_tables().unwrap();
        let rule = rules.get(rules.len() - 1).unwrap();
        assert_eq!(rule["kind"].as_str(), Some("domain_suffix"));
        assert_eq!(rule["value"].as_str(), Some("video.example"));
        assert_eq!(rule["action"].as_str(), Some("block"));
    }
    #[test]
    fn duplicating_a_node_inserts_a_unique_copy_below_it() {
        let mut e = proxy();
        let doc = e.document().unwrap();
        e.focus_item(&doc, "outbounds", 0);
        press(&mut e, KeyCode::Char('y'));
        press(&mut e, KeyCode::Char('y'));
        let doc = e.document().unwrap();
        let nodes = doc["outbounds"].as_array_of_tables().unwrap();
        let tags: Vec<_> = nodes.iter().map(|n| n["tag"].as_str().unwrap()).collect();
        assert_eq!(tags, ["edge", "edge-2", "edge-2-2"]);
        assert_eq!(
            nodes.get(2).unwrap()["config"]["uuid"].as_str(),
            nodes.get(0).unwrap()["config"]["uuid"].as_str()
        );
        assert!(matches!(
            e.visible_rows(&doc).get(e.form.selected),
            Some(FlatRow::Node { index: 2, .. })
        ));
    }
    #[test]
    fn field_values_and_the_input_line_are_highlighted() {
        use ratatui::style::Color;
        let mut e = Editor::new(
            "rules".into(),
            "socket='/tmp/r.sock'\ndefault_action='direct'\n[[rules]]\nkind='ip_cidr'\nvalue='10.0.0.0/8'\naction='proxy:PROXY'\n".into(),
        )
        .with_kind(CoreKind::Rule);
        let line_colors = |e: &Editor, needle: &str| {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
            terminal.draw(|f| e.render(f, f.area())).unwrap();
            let buffer = terminal.backend().buffer().clone();
            // The rule's heading repeats the value in one color; check the last match, the field.
            (0..30u16)
                .filter_map(|y| {
                    let cells: Vec<_> = (0..100u16).map(|x| buffer[(x, y)].clone()).collect();
                    let text: String = cells.iter().map(|c| c.symbol()).collect();
                    let at = text.find(needle)?;
                    // Byte offset to cell index: every cell holds one symbol here.
                    let start = text[..at].chars().count();
                    Some(
                        cells[start..start + needle.chars().count()]
                            .iter()
                            .map(|c| c.fg)
                            .collect::<Vec<_>>(),
                    )
                })
                .next_back()
                .unwrap_or_else(|| panic!("{needle} not on screen"))
        };
        press(&mut e, KeyCode::Char('/'));
        e.paste("10.0.0.0");
        press(&mut e, KeyCode::Enter);
        press(&mut e, KeyCode::Char('/'));
        press(&mut e, KeyCode::Enter);
        // Browsing: address, separator and prefix length use different colors.
        let colors = line_colors(&e, "10.0.0.0/8");
        assert_eq!(colors[0], Color::Cyan);
        assert_eq!(colors[8], Color::Gray);
        assert_eq!(colors[9], Color::Yellow);
        let action = line_colors(&e, "proxy:PROXY");
        assert_eq!(action[0], Color::Gray);
        assert_eq!(action[6], Color::LightBlue);
        // Editing: the typed text keeps its colors with the cursor at the end.
        press(&mut e, KeyCode::Enter);
        let colors = line_colors(&e, "> 10.0.0.0/8▏");
        assert_eq!(colors[2 + 8], Color::Gray);
        assert_eq!(colors[2 + 9], Color::Yellow);
    }
    #[test]
    fn search_reaches_fields_inside_collapsed_entries_and_wraps() {
        let mut e = Editor::new(
            "rules".into(),
            include_str!("../../../../configs/rule.toml").into(),
        )
        .with_kind(CoreKind::Rule);
        assert!(!e.form.expanded_collections.contains(&"rules".into()));
        press(&mut e, KeyCode::Char('/'));
        e.paste("blocked.test");
        press(&mut e, KeyCode::Enter);
        let doc = e.document().unwrap();
        assert!(matches!(
            e.visible_rows(&doc).get(e.form.selected),
            Some(FlatRow::Rule { index: 1, .. })
        ));
        // `/` reopens with the previous query; Enter moves to the next match.
        press(&mut e, KeyCode::Char('/'));
        press(&mut e, KeyCode::Enter);
        assert!(matches!(
            e.visible_rows(&doc).get(e.form.selected),
            Some(FlatRow::Field { spec, .. }) if spec.key == "value"
        ));
        press(&mut e, KeyCode::Char('/'));
        e.paste("no-such-thing");
        press(&mut e, KeyCode::Enter);
        assert!(screen(&e, 100, 30).contains("no-such-thing"));
    }
    #[test]
    fn form_aligns_values_marks_changes_and_shows_hints_in_three_languages() {
        let mut e = proxy();
        let doc = e.document().unwrap();
        e.focus_field(&doc, &[], "connect_timeout_secs");
        press(&mut e, KeyCode::Enter);
        e.paste("30");
        press(&mut e, KeyCode::Enter);
        for (language, hint, unsaved) in [
            (crate::i18n::Language::English, "dresettodefault", "unsaved"),
            (crate::i18n::Language::Simplified, "d恢复默认", "未保存"),
            (crate::i18n::Language::Traditional, "d恢復預設", "未儲存"),
        ] {
            crate::i18n::with_language(language, || {
                let text = screen(&e, 100, 30);
                assert!(text.contains(hint), "{text}");
                assert!(text.contains(unsaved), "{text}");
                let marked: Vec<_> = text.lines().filter(|l| l.contains('●')).collect();
                assert!(marked.iter().any(|l| l.contains("30")), "{text}");
                assert_eq!(marked.len(), 2, "{text}");
                for (w, h) in [(40, 10), (12, 6)] {
                    screen(&e, w, h);
                }
            });
        }
    }
}
