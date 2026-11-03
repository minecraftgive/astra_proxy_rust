use super::*;
use ratatui::{backend::TestBackend, Terminal};
#[test]
fn subscription_update_proxy_offers_groups_nodes_and_direct() {
    let mut editor = Editor::new("proxy".into(), "standalone=true\n[[outbounds]]\ntag='node'\nkind='http'\nserver='127.0.0.1:1234'\n[subscriptions.remote]\nurl='https://example.com/sub'\n[proxy_groups.custom]\nmembers=['node']\n".into());
    editor.form.path = vec![
        Part::Key("subscriptions".into()),
        Part::Key("remote".into()),
    ];
    let doc = editor.document().unwrap();
    let choices = editor.reference_values(&doc, "proxy").unwrap();
    for target in ["PROXY", "DIRECT", "node", "custom"] {
        assert!(choices.contains(&target.to_owned()), "{target}");
    }
}

#[test]
fn selected_node_export_uses_the_visible_node_row() {
    let mut editor = Editor::new(
        "proxy".into(),
        "standalone=true\n[[outbounds]]\ntag='first'\nkind='http'\nserver='127.0.0.1:1234'\n[[outbounds]]\ntag='second'\nkind='socks5'\nserver='127.0.0.1:2345'\n".into(),
    );
    let doc = editor.document().unwrap();
    editor.form.selected = editor
        .visible_rows(&doc)
        .iter()
        .position(|row| matches!(row, FlatRow::Node { label, .. } if label.contains("second")))
        .unwrap();

    assert_eq!(editor.selected_node_tag().unwrap(), "second");
    let output = editor.export_selected_node().unwrap();
    let exported: toml_edit::DocumentMut = output.parse().unwrap();
    let nodes = exported["outbounds"].as_array_of_tables().unwrap();
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes.get(0).unwrap()["tag"].as_str(), Some("second"));
    assert!(!output.contains("first"));
}

#[test]
fn selected_rule_export_uses_the_visible_rule_row() {
    let mut editor = Editor::new(
        "rules".into(),
        "socket='r'\ndefault_action='direct'\n[[rules]]\nkind='rule_set'\nvalue='ads'\naction='block'\n[[rules]]\nkind='domain_suffix'\nvalue='example.com'\naction='direct'\n[rule_sets.ads]\nbehavior='domain'\npayload=['+.ads.test']\n".into(),
    );
    let doc = editor.document().unwrap();
    editor.form.expanded_collections.push("rules".into());
    editor.form.expanded_rule_groups = collection(&doc, &[Part::Key("rules".into())])
        .map(|rules| {
            rule_groups(rules)
                .into_iter()
                .map(|group| group.0)
                .collect()
        })
        .unwrap();
    let doc = editor.document().unwrap();
    editor.form.selected = editor
        .visible_rows(&doc)
        .iter()
        .position(|row| matches!(row, FlatRow::Rule { index: 1, .. }))
        .unwrap();

    assert_eq!(editor.selected_rule_index().unwrap(), 1);
    let output = editor.export_selected_rule().unwrap();
    let exported: toml_edit::DocumentMut = output.parse().unwrap();
    let rules = exported["rules"].as_array_of_tables().unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules.get(0).unwrap()["value"].as_str(), Some("example.com"));
    assert!(exported.get("rule_sets").is_none());
}

#[test]
fn selected_export_rejects_non_item_visible_rows() {
    let mut editor = Editor::new("proxy".into(), "standalone=true\n".into());
    let doc = editor.document().unwrap();
    editor.form.selected = editor
        .visible_rows(&doc)
        .iter()
        .position(|row| matches!(row, FlatRow::Section { .. }))
        .unwrap();
    assert!(editor.selected_node_tag().is_err());
    assert!(editor.selected_rule_index().is_err());
}

#[test]
fn selected_export_rejects_child_rows() {
    let mut node_editor = Editor::new(
        "proxy".into(),
        "standalone=true\n[[outbounds]]\ntag='node'\nkind='http'\nserver='127.0.0.1:1234'\n".into(),
    );
    node_editor.form.expanded_nodes.push(0);
    let doc = node_editor.document().unwrap();
    node_editor.form.selected = node_editor
        .visible_rows(&doc)
        .iter()
        .position(|row| matches!(row, FlatRow::Field { path, .. } if node_index(path).is_some()))
        .unwrap();
    assert!(node_editor.selected_node_tag().is_err());

    let mut rule_editor = Editor::new(
        "rules".into(),
        "socket='r'\ndefault_action='direct'\n[[rules]]\nkind='domain_suffix'\nvalue='example.com'\naction='direct'\n".into(),
    );
    rule_editor.form.expanded_collections.push("rules".into());
    let doc = rule_editor.document().unwrap();
    let start = rule_editor
        .visible_rows(&doc)
        .iter()
        .find_map(|row| match row {
            FlatRow::RuleGroup { start, .. } => Some(*start),
            _ => None,
        })
        .unwrap();
    // Expanding the group shows each rule's row followed by its fields.
    rule_editor.form.expanded_rule_groups.push(start);
    let doc = rule_editor.document().unwrap();
    rule_editor.form.selected = rule_editor
        .visible_rows(&doc)
        .iter()
        .position(|row| matches!(row, FlatRow::Field { path, .. } if item_index(path).is_some_and(|(kind, index)| kind == "rules" && index == start)))
        .unwrap();
    assert!(rule_editor.selected_rule_index().is_err());
}

#[test]
fn nested_subscription_picker_exposes_nodes_and_update_entry() {
    let mut editor = Editor::new("proxy".into(), "standalone=true\n[subscriptions.remote]\nurl='https://example.com/sub'\n[proxy_groups.parent]\nmembers=['remote']\n".into());
    editor.set_subscription_nodes(
        [(
            "remote".into(),
            crate::protocol::SubscriptionNodes {
                url: "https://example.com/sub".into(),
                tags: vec!["node-a".into(), "node-b".into()],
            },
        )]
        .into_iter()
        .collect(),
    );
    editor.form.path = vec![Part::Key("proxy_groups".into()), Part::Key("parent".into())];
    let doc = editor.document().unwrap();
    assert_eq!(
        editor.reference_values(&doc, "selected").unwrap(),
        ["remote", "node-a", "node-b"]
    );
    assert_eq!(
        editor.reference_label(&doc, "selected", "node-b"),
        "remote › node-b"
    );
    assert!(editor
        .requests_subscription_update(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)));
    let rows = editor.visible_rows(&doc);
    editor.form.selected = rows
        .iter()
        .position(|row| matches!(row, FlatRow::UpdateSubscriptions))
        .unwrap();
    assert!(editor.requests_subscription_update(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
    let mut doc = doc;
    doc["subscriptions"]["remote"]["url"] = value("https://example.com/changed");
    assert_eq!(
        editor.reference_values(&doc, "selected").unwrap(),
        ["remote"]
    );
}
#[test]
fn subscription_nodes_are_read_only_source_scoped_and_hide_after_draft_url_change() {
    let mut editor=Editor::new("proxy".into(),"standalone=true\n[subscriptions.a]\nurl='https://example.com/a'\n[subscriptions.b]\nurl='https://example.com/b'\n".into());
    let original = editor.text();
    editor.set_subscription_nodes(
        [
            (
                "a".into(),
                crate::protocol::SubscriptionNodes {
                    url: "https://example.com/a".into(),
                    tags: vec!["a-node".into()],
                },
            ),
            (
                "b".into(),
                crate::protocol::SubscriptionNodes {
                    url: "https://example.com/b".into(),
                    tags: vec!["b-node".into()],
                },
            ),
        ]
        .into_iter()
        .collect(),
    );
    let doc = editor.document().unwrap();
    let rows = editor.visible_rows(&doc);
    let at=rows.iter().position(|row|matches!(row,FlatRow::Info {path,label} if label.contains("a-node") && path==&vec![Part::Key("subscriptions".into()),Part::Key("a".into())])).unwrap();
    editor.form.selected = at;
    assert!(editor.subscription_page());
    press(&mut editor, KeyCode::Enter);
    press(&mut editor, KeyCode::Char('d'));
    assert_eq!(editor.text(), original);
    assert!(matches!(editor.form.mode, Mode::Browse));
    let mut doc = editor.document().unwrap();
    doc["subscriptions"]["a"]["url"] = value("https://example.com/new");
    editor.update(doc);
    let rows = editor.visible_rows(&editor.document().unwrap());
    assert!(!rows
        .iter()
        .any(|row| matches!(row,FlatRow::Info {label,..} if label.contains("a-node"))));
    assert!(rows
        .iter()
        .any(|row| matches!(row,FlatRow::Info {label,..} if label.contains("b-node"))));
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
#[test]
fn subscription_list_creates_multiple_named_sources_and_keeps_ownership_internal() {
    let mut editor = Editor::new("proxy".into(), "socks5_listen='127.0.0.1:1080'\n".into())
        .with_kind(CoreKind::Proxy);
    for name in ["primary", "backup"] {
        let doc = editor.document().unwrap();
        editor.form.selected = editor.visible_rows(&doc).iter().position(|row| matches!(row, FlatRow::AddHeader { path } if last_key(path) == Some("subscriptions"))).unwrap();
        press(&mut editor, KeyCode::Enter);
        editor.paste(name);
        press(&mut editor, KeyCode::Enter);
        assert!(matches!(&editor.form.mode, Mode::Field { key, .. } if key == "url"));
        editor.paste(&format!("https://example.com/{name}"));
        press(&mut editor, KeyCode::Enter);
    }
    let doc = editor.document().unwrap();
    assert_eq!(
        doc["subscriptions"]["primary"]["url"].as_str(),
        Some("https://example.com/primary")
    );
    assert_eq!(
        doc["subscriptions"]["backup"]["interval"].as_integer(),
        Some(3600)
    );
    assert!(editor
        .visible_rows(&doc)
        .iter()
        .any(|row| matches!(row, FlatRow::Field { spec, .. } if spec.key == "import_groups")));
    assert_eq!(
        doc["proxy_groups"]["primary"]["subscription"].as_str(),
        Some("primary")
    );
    assert_eq!(
        doc["proxy_groups"]["primary"]["include_in_proxy"].as_bool(),
        Some(true)
    );
    let path = vec![
        Part::Key("proxy_groups".into()),
        Part::Key("primary".into()),
    ];
    assert!(editor
        .schema_rows(&doc, &path)
        .iter()
        .any(|row| matches!(row, Row::Field(spec) if spec.key == "include_in_proxy")));
    let mut saved: Editor = serde_json::from_str(&serde_json::to_string(&editor).unwrap()).unwrap();
    saved.normalize();
    assert_eq!(saved.text(), editor.text());
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
fn rules() -> Editor {
    Editor::new(
        "rules".into(),
        include_str!("../../../../configs/rule.toml").into(),
    )
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
fn node_references_filter_complete_select_cancel_and_follow_draft_tags() {
    let mut e = Editor::new("proxy".into(), "standalone=true\nstandalone_outbound='edge-a'\n[[outbounds]]\nkind='socks5'\ntag='edge-a'\nserver='127.0.0.1:1'\n[[outbounds]]\nkind='socks5'\ntag='中文节点'\nserver='127.0.0.1:2'\n".into()).with_kind(CoreKind::Proxy);
    open_field(&mut e, "standalone_outbound");
    e.paste("中文");
    press(&mut e, KeyCode::Tab);
    assert!(matches!(&e.form.mode, Mode::Field {input,..} if input.text=="中文节点"));
    press(&mut e, KeyCode::Esc);
    assert_eq!(
        e.document().unwrap()["standalone_outbound"].as_str(),
        Some("edge-a")
    );
    open_field(&mut e, "standalone_outbound");
    e.paste("中文");
    press(&mut e, KeyCode::Down);
    let mut restored: Editor = serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
    let saved = restored.prepare_save().unwrap();
    assert_eq!(
        saved.parse::<DocumentMut>().unwrap()["standalone_outbound"].as_str(),
        Some("中文节点")
    );
    restored.form.path = node_path();
    field(&mut restored, "tag", "renamed");
    restored.form.path.clear();
    let values = restored
        .reference_values(&restored.document().unwrap(), "standalone_outbound")
        .unwrap();
    assert!(values.contains(&"renamed".into()));
    assert!(!values.contains(&"edge-a".into()));
}
#[test]
fn route_actions_accept_picked_or_short_tags_and_keep_explicit_manual_values() {
    let mut e = Editor::new("rules".into(), "socket='r'\ndefault_action='direct'\n[[rules]]\nkind='domain_exact'\nvalue='a.test'\naction='direct'\n".into()).with_kind(CoreKind::Rule);
    e.set_node_tags(vec!["edge".into(), "proxy:edge".into(), "other".into()]);
    open_field(&mut e, "default_action");
    e.paste("ed");
    press(&mut e, KeyCode::Down);
    press(&mut e, KeyCode::Enter);
    assert_eq!(
        e.document().unwrap()["default_action"].as_str(),
        Some("proxy:edge")
    );
    e.form.path = vec![Part::Key("rules".into()), Part::Index(0)];
    field(&mut e, "action", "other");
    assert_eq!(
        e.document().unwrap()["rules"][0]["action"].as_str(),
        Some("proxy:other")
    );
    field(&mut e, "action", "proxy:edge");
    assert_eq!(
        e.document().unwrap()["rules"][0]["action"].as_str(),
        Some("proxy:edge")
    );
    field(&mut e, "action", "proxy:external-unregistered");
    assert_eq!(
        e.document().unwrap()["rules"][0]["action"].as_str(),
        Some("proxy:external-unregistered")
    );
    let text = e.text();
    e.import(text).unwrap();
    assert_eq!(e.node_tags.len(), 3);
}
#[test]
fn default_proxy_dynamic_candidates_and_empty_explicit_members_are_editable() {
    let mut e=Editor::new("p".into(),"standalone=true\nstandalone_outbound='PROXY'\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\nkind='socks5'\ntag='JP-a'\nserver='127.0.0.1:1081'\n[[outbounds]]\nkind='socks5'\ntag='US-b'\nserver='127.0.0.1:1082'".into());
    e.form.path = vec![Part::Key("proxy_groups".into()), Part::Key("PROXY".into())];
    let doc = e.document().unwrap();
    assert_eq!(
        doc["proxy_groups"]["PROXY"]["include_all"].as_bool(),
        Some(true)
    );
    assert_eq!(
        e.reference_values(&doc, "selected").unwrap(),
        ["JP-a", "US-b"]
    );
    field(&mut e, "filter", "^JP-");
    assert_eq!(
        e.reference_values(&e.document().unwrap(), "selected")
            .unwrap(),
        ["JP-a"]
    );
    field(&mut e, "selected", "JP-a");
    let doc = e.document().unwrap();
    let path = e.form.path.clone();
    e.focus_field(&doc, &path, "members");
    press(&mut e, KeyCode::Enter);
    press(&mut e, KeyCode::Enter);
    assert!(e.document().unwrap()["proxy_groups"]["PROXY"]["members"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(
        e.document().unwrap()["proxy_groups"]["PROXY"]["selected"].as_str(),
        Some("JP-a")
    );
    assert!(e.key(key(KeyCode::Char('r'))).is_err());
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
fn member_picker_finds_members_without_toggling_them() {
    let mut e = Editor::new("p".into(), "standalone=true\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\nkind='socks5'\ntag='tokyo-1'\nserver='127.0.0.1:1081'\n[[outbounds]]\nkind='socks5'\ntag='osaka'\nserver='127.0.0.1:1082'\n[[outbounds]]\nkind='socks5'\ntag='tokyo-2'\nserver='127.0.0.1:1083'".into());
    let path = vec![Part::Key("proxy_groups".into())];
    e.form.selected = e
        .visible_rows(&e.document().unwrap())
        .iter()
        .position(|r| r.path() == path)
        .unwrap();
    press(&mut e, KeyCode::Char('n'));
    e.paste("jp");
    press(&mut e, KeyCode::Enter);
    let at = |e: &Editor| match &e.form.mode {
        Mode::Members {
            options, cursor, ..
        } => options[*cursor].clone(),
        _ => panic!("expected picker"),
    };
    press(&mut e, KeyCode::Char('/'));
    // j, k and Space are query text while finding.
    for c in "osaka".chars() {
        press(&mut e, KeyCode::Char(c));
    }
    assert_eq!(at(&e), "osaka");
    assert!(matches!(&e.form.mode, Mode::Members { find: Some(q), .. } if q == "osaka"));
    for _ in 0.."osaka".len() {
        press(&mut e, KeyCode::Backspace);
    }
    for c in "tokyo".chars() {
        press(&mut e, KeyCode::Char(c));
    }
    assert_eq!(at(&e), "tokyo-1");
    press(&mut e, KeyCode::Down);
    assert_eq!(at(&e), "tokyo-2");
    press(&mut e, KeyCode::Enter);
    // The picker stays open; Space now toggles the found member.
    press(&mut e, KeyCode::Char(' '));
    press(&mut e, KeyCode::Enter);
    assert_eq!(
        e.document().unwrap()["proxy_groups"]["jp"]["members"][0].as_str(),
        Some("tokyo-2")
    );
}
#[test]
fn new_proxy_group_opens_member_picker_and_is_available_as_default() {
    let mut e=Editor::new("p".into(),"standalone=true\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\nkind='socks5'\ntag='a'\nserver='127.0.0.1:1081'".into());
    let path = vec![Part::Key("proxy_groups".into())];
    e.form.selected = e
        .visible_rows(&e.document().unwrap())
        .iter()
        .position(|r| r.path() == path)
        .unwrap();
    press(&mut e, KeyCode::Char('n'));
    e.paste("my-group");
    press(&mut e, KeyCode::Enter);
    if let Mode::Members {
        options, cursor, ..
    } = &mut e.form.mode
    {
        *cursor = options.iter().position(|v| v == "a").unwrap();
    } else {
        panic!("expected picker");
    }
    press(&mut e, KeyCode::Char(' '));
    press(&mut e, KeyCode::Enter);
    e.form.path.clear();
    assert!(e
        .reference_values(&e.document().unwrap(), "standalone_outbound")
        .unwrap()
        .contains(&"my-group".into()));
    field(&mut e, "standalone_outbound", "my-group");
    assert_eq!(
        e.document().unwrap()["proxy_groups"]["my-group"]["members"][0].as_str(),
        Some("a")
    );
}
#[test]
fn direct_and_reject_groups_are_built_in_editable_and_protected() {
    let mut e = Editor::new("p".into(), "standalone=true\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\nkind='socks5'\ntag='a'\nserver='127.0.0.1:1081'".into())
        .with_kind(CoreKind::Proxy);
    let doc = e.document().unwrap();
    assert_eq!(
        doc["proxy_groups"]["DIRECT"]["members"][0].as_str(),
        Some("DIRECT")
    );
    assert_eq!(
        doc["proxy_groups"]["REJECT"]["members"][0].as_str(),
        Some("REJECT")
    );
    let direct = vec![Part::Key("proxy_groups".into()), Part::Key("DIRECT".into())];
    e.reveal(&doc, &direct, true);
    let header = e
        .visible_rows(&doc)
        .iter()
        .position(|row| matches!(row, FlatRow::Section { path, .. } if path == &direct))
        .unwrap();
    for code in [KeyCode::Char('r'), KeyCode::Char('d')] {
        e.form.selected = header;
        assert!(e.key(key(code)).is_err());
        e.focus_field(&doc, &direct, "members");
        assert!(e.key(key(code)).is_err());
        assert_eq!(
            e.document().unwrap()["proxy_groups"]["DIRECT"]["members"][0].as_str(),
            Some("DIRECT")
        );
    }
    // Its own terminal outbound stays selectable next to the nodes.
    let doc = e.document().unwrap();
    e.focus_field(&doc, &direct, "members");
    press(&mut e, KeyCode::Enter);
    let Mode::Members {
        options, cursor, ..
    } = &mut e.form.mode
    else {
        panic!("expected picker");
    };
    assert!(options.contains(&"DIRECT".into()) && options.contains(&"a".into()));
    *cursor = options.iter().position(|v| v == "a").unwrap();
    press(&mut e, KeyCode::Char(' '));
    press(&mut e, KeyCode::Enter);
    assert_eq!(
        e.document().unwrap()["proxy_groups"]["DIRECT"]["members"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    // Rule actions keep direct / block rather than repeating proxy:DIRECT.
    let mut r = rules();
    r.set_node_tags(vec!["DIRECT".into(), "edge".into()]);
    r.form.path = vec![Part::Key("rules".into()), Part::Index(0)];
    let values = r
        .reference_values(&r.document().unwrap(), "action")
        .unwrap();
    assert!(values.contains(&"proxy:edge".into()) && !values.contains(&"proxy:DIRECT".into()));
}
#[test]
fn changing_rule_kind_uses_real_references_and_resets_incompatible_values() {
    let mut e = Editor::new("rules".into(), "socket='r'\ndefault_action='direct'\n[[rules]]\nkind='domain_regex'\nvalue='^rx[0-9]+$'\naction='block'\n[rule_sets.ads]\npayload=[]\n".into());
    e.form.path = vec![Part::Key("rules".into()), Part::Index(0)];
    field(&mut e, "kind", "domain_exact");
    assert_eq!(
        e.document().unwrap()["rules"][0]["value"].as_str(),
        Some("example.com")
    );
    field(&mut e, "kind", "domain_pattern");
    assert_eq!(
        e.document().unwrap()["rules"][0]["value"].as_str(),
        Some("*.example.com")
    );
    field(&mut e, "kind", "rule_set");
    assert_eq!(
        e.document().unwrap()["rules"][0]["value"].as_str(),
        Some("ads")
    );
    assert_eq!(
        e.document().unwrap()["rules"][0]["action"].as_str(),
        Some("block")
    );
    let mut e = Editor::new("rules".into(), "socket='r'\ndefault_action='direct'\n[[rules]]\nkind='domain_exact'\nvalue='example.com'\naction='block'".into());
    e.form.path = vec![Part::Key("rules".into()), Part::Index(0)];
    let before = e.text();
    open_field(&mut e, "kind");
    e.paste("rule_set");
    assert!(e.commit_field().is_err());
    assert_eq!(e.text(), before);
}
#[test]
fn rule_set_network_and_enum_fields_share_filter_and_completion() {
    let mut e = Editor::new("rules".into(), "socket='r'\ndefault_action='direct'\n[[rules]]\nkind='rule_set'\nvalue='old'\naction='direct'\n[rule_sets.local]\npayload=[]\n[rule_sets.remote]\npayload=[]\n".into());
    e.form.path = vec![Part::Key("rules".into()), Part::Index(0)];
    open_field(&mut e, "value");
    e.paste("loc");
    press(&mut e, KeyCode::Tab);
    press(&mut e, KeyCode::Enter);
    assert_eq!(
        e.document().unwrap()["rules"][0]["value"].as_str(),
        Some("local")
    );
    open_field(&mut e, "kind");
    e.paste("netw");
    press(&mut e, KeyCode::Tab);
    press(&mut e, KeyCode::Enter);
    assert_eq!(
        e.document().unwrap()["rules"][0]["kind"].as_str(),
        Some("network")
    );
    open_field(&mut e, "value");
    e.paste("u");
    press(&mut e, KeyCode::Down);
    press(&mut e, KeyCode::Enter);
    assert_eq!(
        e.document().unwrap()["rules"][0]["value"].as_str(),
        Some("udp")
    );
    open_field(&mut e, "value");
    e.paste("no-match");
    press(&mut e, KeyCode::Tab);
    assert!(matches!(&e.form.mode,Mode::Field{input,..} if input.text=="no-match"));
    press(&mut e, KeyCode::Esc);
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
fn each_proxy_group_saves_its_own_adapter_without_changing_tun() {
    let mut editor = Editor::new("proxy".into(), "standalone=true\n[tun]\nauto_interface=true\n[proxy_groups.one]\nmembers=['DIRECT']\n[proxy_groups.two]\nmembers=['DIRECT']\n".into());
    for (group, adapter) in [("one", "Ethernet"), ("two", "虚拟网卡 VPN")] {
        editor.form.path = path(&["proxy_groups", group]);
        let doc = editor.document().unwrap();
        editor.focus_field(&doc, &editor.form.path.clone(), "interface");
        press(&mut editor, KeyCode::Enter);
        editor.interfaces = vec!["Ethernet".into(), "虚拟网卡 VPN".into()];
        editor.paste(adapter);
        press(&mut editor, KeyCode::Enter);
    }
    let doc = editor.document().unwrap();
    assert_eq!(
        doc["proxy_groups"]["one"]["interface"].as_str(),
        Some("Ethernet")
    );
    assert_eq!(
        doc["proxy_groups"]["two"]["interface"].as_str(),
        Some("虚拟网卡 VPN")
    );
    assert!(doc["proxy_groups"]["one"].get("auto_interface").is_none());
    assert_eq!(doc["tun"]["auto_interface"].as_bool(), Some(true));
    editor.form.path = path(&["proxy_groups", "one"]);
    field(&mut editor, "interface", "");
    let doc = editor.document().unwrap();
    assert!(doc["proxy_groups"]["one"].get("interface").is_none());
    assert_eq!(
        doc["proxy_groups"]["two"]["interface"].as_str(),
        Some("虚拟网卡 VPN")
    );
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
fn proxy_modes_and_route_lists_keep_toml_types() {
    let mut e = native();
    field(&mut e, "watch_config", "true");
    e.form.path = path(&["tun"]);
    field(&mut e, "routes", "[\"0.0.0.0/1\", \"128.0.0.0/1\"]");
    let interface = crate::network_interfaces::names(None).unwrap().remove(0);
    field(&mut e, "interface", &interface);
    if cfg!(target_os = "macos") {
        e.form.path = path(&["system_proxy"]);
        field(&mut e, "services", "[\"Wi-Fi\"]");
    }
    let parsed: toml::Value = toml::from_str(&e.text()).unwrap();
    assert_eq!(parsed["watch_config"].as_bool(), Some(true));
    assert_eq!(parsed["tun"]["routes"].as_array().unwrap().len(), 2);
    #[cfg(target_os = "macos")]
    assert_eq!(
        parsed["system_proxy"]["services"][0].as_str(),
        Some("Wi-Fi")
    );
}

#[test]
fn hidden_platform_fields_are_preserved_and_not_reintroduced() {
    let mut e = Editor::new("proxy".into(), "# preserve platform settings\nstandalone = true\n[tproxy]\nlisten = '127.0.0.1:7893'\n[redirect]\nlisten = '127.0.0.1:7894'\n[system_proxy]\nservices = ['Wi-Fi']\n".into());
    let platform = crate::availability::Platform::current();
    let doc = e.document().unwrap();
    let rows = e.visible_rows(&doc);
    for key in ["tproxy", "redirect"] {
        assert_eq!(
            rows.iter().any(
                |row| matches!(row, FlatRow::Section { path, .. } if last_key(path) == Some(key))
            ),
            platform.option(None, key)
        );
    }
    let services = e.schema_rows(&doc, &path(&["system_proxy"]));
    assert_eq!(
        services
            .iter()
            .any(|row| matches!(row, Row::Field(spec) if spec.key == "services")),
        platform.option(Some("system_proxy"), "services")
    );
    field(&mut e, "watch_config", "true");
    let saved = e.prepare_save().unwrap();
    let parsed: toml::Value = toml::from_str(&saved).unwrap();
    assert_eq!(parsed["tproxy"]["listen"].as_str(), Some("127.0.0.1:7893"));
    assert_eq!(
        parsed["redirect"]["listen"].as_str(),
        Some("127.0.0.1:7894")
    );
    assert_eq!(
        parsed["system_proxy"]["services"][0].as_str(),
        Some("Wi-Fi")
    );
    assert!(saved.starts_with("# preserve platform settings"));
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
fn standalone_without_rule_socket_is_editable_as_proxy() {
    let mut e = Editor::new(
        "custom".into(),
        "standalone = true\nsocks5_listen = '127.0.0.1:1080'\n".into(),
    );
    let doc = e.document().unwrap();
    assert!(e.is_proxy(&doc));
    assert!(e
        .schema_rows(&doc, &[])
        .iter()
        .any(|r| matches!(r, Row::Field(s) if s.key == "rule_socket" && !s.required)));
    field(&mut e, "standalone", "false");
    assert_eq!(e.document().unwrap()["standalone"].as_bool(), Some(false));
    field(&mut e, "standalone", "true");
    field(&mut e, "standalone_outbound", "selected");
    let parsed: toml::Value = toml::from_str(&e.text()).unwrap();
    assert_eq!(parsed["standalone"].as_bool(), Some(true));
    assert_eq!(parsed["standalone_outbound"].as_str(), Some("selected"));
    assert!(parsed.get("rule_socket").is_none());
}

#[test]
fn vless_encryption_and_distinct_tls_pins_are_editable_and_round_trip() {
    let mut e = native();
    let mut config_path = node_path();
    config_path.push(Part::Key("config".into()));
    e.form.path = config_path.clone();
    let encryption = "mlkem768x25519plus.native.0rtt.AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA";
    field(&mut e, "encryption", encryption);
    let mut tls_path = config_path.clone();
    tls_path.push(Part::Key("tls".into()));
    e.form.path = tls_path.clone();
    field(&mut e, "fingerprint", "chrome");
    let pin = "11".repeat(32);
    field(&mut e, "cert_pin_sha256", &pin);
    let mut transport_path = config_path.clone();
    transport_path.push(Part::Key("transport".into()));
    e.form.path = transport_path.clone();
    field(&mut e, "mode", "auto");
    let parsed: toml::Value = toml::from_str(&e.text()).unwrap();
    let config = &parsed["outbounds"][0]["config"];
    assert_eq!(config["encryption"].as_str(), Some(encryption));
    assert_eq!(config["tls"]["fingerprint"].as_str(), Some("chrome"));
    assert_eq!(
        config["tls"]["cert_pin_sha256"].as_str(),
        Some(pin.as_str())
    );
    assert_eq!(config["transport"]["mode"].as_str(), Some("auto"));
    let restored = Editor::new("proxy".into(), e.text());
    let doc = restored.document().unwrap();
    let tls_fields = restored.schema_rows(&doc, &tls_path);
    assert!(tls_fields.iter().any(|r| matches!(r, Row::Field(spec) if spec.key == "fingerprint" && spec.label.contains("客户端"))));
    assert!(tls_fields.iter().any(|r| matches!(r, Row::Field(spec) if spec.key == "cert_pin_sha256" && spec.label.contains("SHA-256"))));
}

#[test]
fn strings_are_escaped_and_comments_are_preserved() {
    let mut e = Editor::new(
        "rules".into(),
        "# keep header\nsocket = \"/tmp/a.sock\" # keep inline\ndefault_action = \"direct\"\n"
            .into(),
    );
    let original = e.text();
    assert_eq!(e.prepare_save().unwrap(), original);
    field(&mut e, "socket", "/tmp/中文 \"quoted\".sock");
    assert!(e.text().starts_with("# keep header\n"), "{}", e.text());
    assert!(e.text().contains("# keep inline"));
    assert_eq!(
        e.document().unwrap()["socket"].as_str(),
        Some("/tmp/中文 \"quoted\".sock")
    );
}
#[test]
fn typed_numbers_reject_invalid_values_and_keep_pending_input_for_retry() {
    let mut e = Editor::new(
        "proxy".into(),
        include_str!("../../../../configs/proxy.toml").into(),
    );
    let doc = e.document().unwrap();
    e.form.selected = e
        .rows(&doc)
        .iter()
        .position(|r| matches!(r,Row::Field(s) if s.key == "ipc_connections"))
        .unwrap();
    press(&mut e, KeyCode::Enter);
    e.paste("0");
    assert!(e.key(key(KeyCode::Enter)).is_err());
    assert_eq!(
        e.document().unwrap()["ipc_connections"].as_integer(),
        Some(8)
    );
    let snapshot = serde_json::to_vec(&e).unwrap();
    let mut e: Editor = serde_json::from_slice(&snapshot).unwrap();
    assert!(matches!(&e.form.mode,Mode::Field{input,..} if input.text == "0"));
    e.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL))
        .unwrap();
    e.paste("16");
    press(&mut e, KeyCode::Enter);
    assert_eq!(
        e.document().unwrap()["ipc_connections"].as_integer(),
        Some(16)
    );
}
#[test]
fn multi_rule_moves_preserve_order_and_undo() {
    let mut e = Editor::new(
        "rules".into(),
        (0..5)
            .map(|i| format!("[[rules]]\nkind='domain'\nvalue='{i}.example'\naction='direct'\n"))
            .collect(),
    );
    let original = e.text();
    e.form.marked_rules = vec![1, 3];
    e.move_marked_rules(None, false).unwrap();
    let values = |e: &Editor| {
        e.document().unwrap()["rules"]
            .as_array_of_tables()
            .unwrap()
            .iter()
            .map(|r| r["value"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        values(&e),
        [
            "1.example",
            "0.example",
            "3.example",
            "2.example",
            "4.example"
        ]
    );
    assert_eq!(e.form.marked_rules, vec![0, 2]);
    e.move_marked_rules(Some(4), false).unwrap();
    assert_eq!(
        values(&e),
        [
            "0.example",
            "2.example",
            "1.example",
            "3.example",
            "4.example"
        ]
    );
    e.restore(false).unwrap();
    e.restore(false).unwrap();
    assert_eq!(e.text(), original);
    assert!(e.form.marked_rules.is_empty());
    e.form.marked_rules = vec![1, 2];
    e.move_marked_rules(None, true).unwrap();
    assert_eq!(
        values(&e),
        [
            "0.example",
            "3.example",
            "1.example",
            "2.example",
            "4.example"
        ]
    );
}

#[test]
fn rule_add_edit_reorder_and_confirm_delete_keep_matching_order() {
    let mut e = rules();
    e.form.path = path(&["rules"]);
    press(&mut e, KeyCode::Char('n'));
    // A new rule opens its fields one after another; stop and edit them directly.
    assert!(matches!(&e.form.mode, Mode::Field { key, .. } if key == "kind"));
    press(&mut e, KeyCode::Esc);
    field(&mut e, "value", "new.example");
    field(&mut e, "action", "proxy:edge");
    press(&mut e, KeyCode::Esc);
    assert!(matches!(
        e.visible_rows(&e.document().unwrap()).get(e.form.selected),
        Some(FlatRow::Rule { index: 2, .. })
    ));
    press(&mut e, KeyCode::Char('K'));
    let doc = e.document().unwrap();
    let a = doc["rules"].as_array_of_tables().unwrap();
    assert_eq!(a.get(1).unwrap()["value"].as_str(), Some("new.example"));
    let before = e.text();
    press(&mut e, KeyCode::Char('d'));
    assert_ne!(e.text(), before);
    press(&mut e, KeyCode::Char('u'));
    assert_eq!(e.text(), before);
    e.key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL))
        .unwrap();
    assert_eq!(
        e.document().unwrap()["rules"]
            .as_array_of_tables()
            .unwrap()
            .len(),
        2
    );
}
#[test]
fn new_outbound_wizard_and_protocol_switch_use_valid_field_shapes() {
    let mut e = Editor::new(
        "proxy".into(),
        include_str!("../../../../configs/proxy.toml").into(),
    );
    e.form.path = path(&["outbounds"]);
    press(&mut e, KeyCode::Char('n'));
    press(&mut e, KeyCode::Down);
    press(&mut e, KeyCode::Down);
    press(&mut e, KeyCode::Enter);
    assert_eq!(
        e.document().unwrap()["outbounds"][0]["kind"].as_str(),
        Some("native")
    );
    assert!(matches!(&e.form.mode, Mode::Field { key, .. } if key == "tag"));
    press(&mut e, KeyCode::Esc);
    e.form.path.push(Part::Key("config".into()));
    field(&mut e, "server", "my.node.example");
    field(&mut e, "protocol", "tuic");
    let doc = e.document().unwrap();
    let config = table_at(doc.as_table(), &e.form.path).unwrap();
    assert_eq!(
        config.get("server").unwrap().as_str(),
        Some("my.node.example")
    );
    assert_eq!(config.get("protocol").unwrap().as_str(), Some("tuic"));
    assert!(config.get("password").is_some());
    field(&mut e, "tuic_idle_timeout_secs", "30");
    field(&mut e, "protocol", "vless");
    let doc = e.document().unwrap();
    assert!(table_at(doc.as_table(), &e.form.path)
        .unwrap()
        .get("tuic_idle_timeout_secs")
        .is_none());
}
#[test]
fn nested_tls_transport_headers_and_inline_tables_are_editable() {
    let mut e = native();
    e.form.path = node_path();
    e.form.path.extend(path(&["config", "tls"]));
    field(&mut e, "ca_file", "private CA.pem");
    e.form.path = node_path();
    e.form
        .path
        .extend(path(&["config", "transport", "headers"]));
    let doc = e.document().unwrap();
    e.form.selected = e
        .visible_rows(&doc)
        .iter()
        .position(|r| matches!(r,FlatRow::AddHeader{path} if path == &e.form.path))
        .unwrap();
    press(&mut e, KeyCode::Enter);
    e.paste("X-Custom");
    press(&mut e, KeyCode::Enter);
    e.paste("中文 header");
    press(&mut e, KeyCode::Enter);
    let doc = e.document().unwrap();
    assert_eq!(
        table_at(doc.as_table(), &e.form.path)
            .unwrap()
            .get("X-Custom")
            .unwrap()
            .as_str(),
        Some("中文 header")
    );
    assert!(e.text().contains("private CA.pem"));
    let mut inline = Editor::new("rules".into(), "socket = '/tmp/a'\ndefault_action = 'direct'\nrules = [{ kind = 'domain_exact', value = 'a.example', action = 'block' }]\n".into());
    inline.form.path = vec![Part::Key("rules".into()), Part::Index(0)];
    field(&mut inline, "value", "b.example");
    assert_eq!(
        inline.document().unwrap()["rules"][0]["value"].as_str(),
        Some("b.example")
    );
}
#[test]
fn process_rules_and_local_default_are_editable_without_losing_rule_order() {
    let mut e = Editor::new("rules".into(), "socket='/tmp/a'\ndefault_action='proxy:edge'\n[[rules]]\nkind='domain_suffix'\nvalue='example.com'\naction='block'\n".into());
    field(&mut e, "direct_local", "false");
    e.form.path = vec![Part::Key("rules".into()), Part::Index(0)];
    field(&mut e, "kind", "process_name");
    field(&mut e, "value", "browser.exe");
    assert_eq!(
        e.document().unwrap()["rules"][0]["kind"].as_str(),
        Some("process_name")
    );
    field(&mut e, "kind", "process_path");
    field(&mut e, "value", "C:/Apps/browser.exe");
    assert_eq!(
        e.document().unwrap()["rules"][0]["value"].as_str(),
        Some("C:/Apps/browser.exe")
    );
    assert_eq!(e.document().unwrap()["direct_local"].as_bool(), Some(false));
}
#[test]
fn geoip_and_named_rule_sets_can_be_created_and_edited() {
    let mut e = Editor::new(
        "rules".into(),
        "socket='/tmp/a'\ndefault_action='direct'\n".into(),
    );
    field(&mut e, "geoip_database", "Country.mmdb");
    field(&mut e, "geosite_database", "geosite.dat");
    let doc = e.document().unwrap();
    e.form.selected = e
        .visible_rows(&doc)
        .iter()
        .position(
            |row| matches!(row,FlatRow::Section{path,..} if last_key(path)==Some("rule_sets")),
        )
        .unwrap();
    press(&mut e, KeyCode::Char('n'));
    e.paste("ads");
    press(&mut e, KeyCode::Enter);
    field(&mut e, "payload", "['+.ads.test']");
    let doc = e.document().unwrap();
    assert_eq!(doc["rule_sets"]["ads"]["behavior"].as_str(), Some("domain"));
    assert_eq!(
        doc["rule_sets"]["ads"]["payload"]
            .as_array()
            .unwrap()
            .get(0)
            .unwrap()
            .as_str(),
        Some("+.ads.test")
    );
    assert_eq!(doc["geoip_database"].as_str(), Some("Country.mmdb"));
}
#[test]
fn moving_native_nodes_keeps_each_nested_configuration_with_its_node() {
    let mut e = Editor::new(
        "proxy".into(),
        include_str!("../../../../configs/proxy.toml").into(),
    );
    let mut doc = e.document().unwrap();
    let mut array = ArrayOfTables::new();
    for (tag, server) in [("first", "first.example"), ("second", "second.example")] {
        let mut t = outbound_template("native:vless", tag);
        t["config"]["server"] = value(server);
        t["config"]["tls"]["server_name"] = value(server);
        array.push(t);
    }
    doc["outbounds"] = Item::ArrayOfTables(array);
    e.update(doc);
    e.form.path = path(&["outbounds"]);
    let doc = e.document().unwrap();
    e.focus_item(&doc, "outbounds", 1);
    press(&mut e, KeyCode::Char('K'));
    let doc = e.document().unwrap();
    assert_eq!(doc["outbounds"][0]["tag"].as_str(), Some("second"));
    assert_eq!(
        doc["outbounds"][0]["config"]["server"].as_str(),
        Some("second.example")
    );
    assert_eq!(
        doc["outbounds"][0]["config"]["tls"]["server_name"].as_str(),
        Some("second.example")
    );
    assert_eq!(
        doc["outbounds"][1]["config"]["server"].as_str(),
        Some("first.example")
    );
}
#[test]
fn optional_fields_delete_and_required_fields_remain_protected() {
    let mut e = native();
    e.form.path = node_path();
    e.form.path.extend(path(&["config"]));
    let doc = e.document().unwrap();
    let config_path = e.form.path.clone();
    e.focus_field(&doc, &config_path, "port");
    assert!(e.key(key(KeyCode::Char('d'))).is_err());
    let mut tls_path = config_path;
    tls_path.push(Part::Key("tls".into()));
    e.form.selected = e
        .visible_rows(&doc)
        .iter()
        .position(|r| matches!(r,FlatRow::Section{path,..} if path == &tls_path))
        .unwrap();
    press(&mut e, KeyCode::Char('d'));
    assert!(table_at(e.document().unwrap().as_table(), &e.form.path)
        .unwrap()
        .get("tls")
        .is_none());
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
#[test]
fn legacy_draft_migrates_to_forms_without_losing_source() {
    let old = serde_json::json!({"name":"rules","original":"socket = '/tmp/a'\ndefault_action = 'direct'\n","lines":["socket = '/tmp/b'","default_action = 'direct'", ""],"row":1,"col":3});
    let mut e: Editor = serde_json::from_value(old).unwrap();
    e.normalize();
    assert!(e.text().contains("/tmp/b"));
    assert!(e.original.contains("/tmp/a"));
    field(&mut e, "socket", "/tmp/c");
    assert_eq!(e.document().unwrap()["socket"].as_str(), Some("/tmp/c"));
}
#[test]
fn rule_categories_fold_with_vim_keys_without_changing_configuration() {
    let mut e = rules();
    let original = e.text();
    let doc = e.document().unwrap();
    let rows = e.visible_rows(&doc);
    assert!(!rows.iter().any(|r| matches!(r, FlatRow::Rule { .. })));
    e.form.selected = rows
        .iter()
        .position(
            |row| matches!(row,FlatRow::Collection {path,..} if last_key(path)==Some("rules")),
        )
        .unwrap();
    press(&mut e, KeyCode::Char('l'));
    assert!(e
        .visible_rows(&doc)
        .iter()
        .any(|row| matches!(row, FlatRow::RuleGroup { .. })));
    assert!(!e
        .visible_rows(&doc)
        .iter()
        .any(|row| matches!(row, FlatRow::Rule { .. })));
    press(&mut e, KeyCode::Char('z'));
    press(&mut e, KeyCode::Char('R'));
    let rows = e.visible_rows(&doc);
    for index in 0..2 {
        for key in ["kind", "value", "action"] {
            assert!(rows.iter().any(|r|matches!(r,FlatRow::Field{path,spec,..} if path == &vec![Part::Key("rules".into()),Part::Index(index)] && spec.key == key)));
        }
    }
    let indexes: Vec<_> = rows
        .iter()
        .filter_map(|row| {
            if let FlatRow::Rule { index, .. } = row {
                Some(*index)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        indexes,
        (0..doc["rules"].as_array_of_tables().unwrap().len()).collect::<Vec<_>>()
    );
    press(&mut e, KeyCode::Char('G'));
    assert_eq!(e.form.selected, rows.len() - 1);
    press(&mut e, KeyCode::Char('g'));
    press(&mut e, KeyCode::Char('g'));
    assert_eq!(e.form.selected, 0);
    press(&mut e, KeyCode::Char('z'));
    press(&mut e, KeyCode::Char('M'));
    assert!(!e
        .visible_rows(&doc)
        .iter()
        .any(|row| matches!(row, FlatRow::Rule { .. })));
    assert_eq!(e.text(), original);
    assert!(e.can_close());
}
#[test]
fn vim_navigation_does_not_capture_field_text_or_reorder_rule_groups() {
    let mut e = Editor::new("rules".into(), "socket='r'\ndefault_action='direct'\nrules=[{kind='domain_exact',value='first.test',action='block'},{kind='ip_cidr',value='10.0.0.0/8',action='direct'},{kind='domain_exact',value='last.test',action='block'}]".into());
    let original = e.text();
    let doc = e.document().unwrap();
    let groups = rule_groups(doc["rules"].as_array_of_tables().unwrap());
    assert_eq!(
        groups.iter().map(|v| v.0).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    e.focus_field(&doc, &[], "default_action");
    press(&mut e, KeyCode::Char('l'));
    for ch in "hjklgg".chars() {
        press(&mut e, KeyCode::Char(ch));
    }
    assert!(matches!(&e.form.mode,Mode::Field{input,..} if input.text == "hjklgg"));
    press(&mut e, KeyCode::Esc);
    assert_eq!(e.text(), original);
    e.focus_field(&doc, &[Part::Key("rules".into()), Part::Index(1)], "value");
    press(&mut e, KeyCode::Char('h'));
    assert!(matches!(
        e.visible_rows(&doc).get(e.form.selected),
        Some(FlatRow::RuleGroup { start: 1, .. })
    ));
    press(&mut e, KeyCode::Char('h'));
    assert!(matches!(
        e.visible_rows(&doc).get(e.form.selected),
        Some(FlatRow::Collection { .. })
    ));
    assert_eq!(e.text(), original);
}
#[test]
fn escape_leaves_rule_fields_then_group_without_changing_rule_order() {
    let mut editor = Editor::new("rules".into(), "socket='r'\ndefault_action='direct'\nrules=[{kind='domain_exact',value='first.test',action='block'},{kind='ip_cidr',value='10.0.0.0/8',action='direct'}]".into());
    let original = editor.text();
    let doc = editor.document().unwrap();
    editor.focus_field(&doc, &[Part::Key("rules".into()), Part::Index(1)], "value");
    press(&mut editor, KeyCode::Esc);
    assert!(matches!(
        editor.visible_rows(&doc).get(editor.form.selected),
        Some(FlatRow::Rule { index: 1, .. })
    ));
    press(&mut editor, KeyCode::Esc);
    assert!(matches!(
        editor.visible_rows(&doc).get(editor.form.selected),
        Some(FlatRow::RuleGroup { start: 1, .. })
    ));
    press(&mut editor, KeyCode::Esc);
    assert!(matches!(
        editor.visible_rows(&doc).get(editor.form.selected),
        Some(FlatRow::Collection { .. })
    ));
    assert!(editor.can_close());
    assert_eq!(editor.text(), original);
}
#[test]
fn only_nodes_fold_and_their_nested_fields_share_the_same_page() {
    let mut e = native();
    let doc = e.document().unwrap();
    let original = e.text();
    assert!(!e
        .visible_rows(&doc)
        .iter()
        .any(|r| matches!(r,FlatRow::Field{path,..} if node_index(path).is_some())));
    e.focus_item(&doc, "outbounds", 0);
    press(&mut e, KeyCode::Enter);
    let rows = e.visible_rows(&doc);
    for (suffix, key) in [
        (vec!["config"], "protocol"),
        (vec!["config", "tls"], "server_name"),
        (vec!["config", "transport"], "kind"),
    ] {
        let mut full_path = node_path();
        full_path.extend(path(&suffix));
        assert!(rows.iter().any(
            |r| matches!(r,FlatRow::Field{path,spec,..} if path == &full_path && spec.key == key)
        ));
    }
    assert!(rows.iter().any(|r| matches!(r, FlatRow::AddHeader { .. })));
    e.form.selected = rows
        .iter()
        .position(|r| matches!(r,FlatRow::Section{path,..} if last_key(path) == Some("transport")))
        .unwrap();
    press(&mut e, KeyCode::Enter);
    assert_eq!(e.visible_rows(&doc).len(), rows.len());
    assert!(!e.can_close());
    press(&mut e, KeyCode::Esc);
    assert!(e.can_close());
    assert!(!e
        .visible_rows(&doc)
        .iter()
        .any(|r| matches!(r,FlatRow::Field{path,..} if node_index(path).is_some())));
    assert_eq!(e.text(), original);
}
#[test]
fn node_fold_state_follows_reordering_and_deletion_and_survives_session_restore() {
    let mut e = native();
    let mut doc = e.document().unwrap();
    doc["outbounds"]
        .as_array_of_tables_mut()
        .unwrap()
        .push(outbound_template("socks5", "second"));
    e.update(doc);
    let doc = e.document().unwrap();
    e.focus_item(&doc, "outbounds", 0);
    press(&mut e, KeyCode::Enter);
    press(&mut e, KeyCode::Char('J'));
    assert_eq!(e.form.expanded_nodes, vec![1]);
    let doc = e.document().unwrap();
    assert_eq!(doc["outbounds"][1]["tag"].as_str(), Some("edge"));
    e.focus_item(&doc, "outbounds", 0);
    press(&mut e, KeyCode::Char('d'));
    assert_eq!(e.form.expanded_nodes, vec![0]);
    let mut restored: Editor = serde_json::from_slice(&serde_json::to_vec(&e).unwrap()).unwrap();
    restored.normalize();
    assert_eq!(restored.form.expanded_nodes, vec![0]);
    assert_eq!(
        restored.document().unwrap()["outbounds"][0]["tag"].as_str(),
        Some("edge")
    );
}
#[test]
fn legacy_nested_field_input_migrates_to_flat_page_without_losing_draft() {
    let mut e = native();
    e.form.flat = false;
    e.form.path = node_path();
    e.form.path.extend(path(&["config", "tls"]));
    e.form.mode = Mode::Field {
        key: "ca_file".into(),
        input: TextInput::new("remembered CA.pem".into()),
        reveal: false,
    };
    let mut e: Editor = serde_json::from_slice(&serde_json::to_vec(&e).unwrap()).unwrap();
    e.normalize();
    assert!(e.form.flat);
    assert_eq!(e.form.expanded_nodes, vec![0]);
    assert!(matches!(&e.form.mode,Mode::Field{input,..} if input.text == "remembered CA.pem"));
    press(&mut e, KeyCode::Enter);
    let doc = e.document().unwrap();
    assert_eq!(
        doc["outbounds"][0]["config"]["tls"]["ca_file"].as_str(),
        Some("remembered CA.pem")
    );
}
#[test]
fn tls_switch_is_inline_and_editing_a_disabled_tls_field_sets_a_valid_server_name() {
    let mut e = native();
    let doc = e.document().unwrap();
    e.expand_node(0);
    e.form.selected = e
        .visible_rows(&doc)
        .iter()
        .position(|r| matches!(r,FlatRow::Section{path,..} if last_key(path) == Some("tls")))
        .unwrap();
    press(&mut e, KeyCode::Enter);
    assert!(e.document().unwrap()["outbounds"][0]["config"]
        .get("tls")
        .is_none());
    e.form.path = node_path();
    e.form.path.extend(path(&["config", "tls"]));
    field(&mut e, "ca_file", "new CA.pem");
    let doc = e.document().unwrap();
    assert_eq!(
        doc["outbounds"][0]["config"]["tls"]["server_name"].as_str(),
        Some("your.server.example")
    );
    assert_eq!(
        doc["outbounds"][0]["config"]["tls"]["ca_file"].as_str(),
        Some("new CA.pem")
    );
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

#[cfg(test)]
mod text_input_history_tests {
    use super::*;
    #[test]
    fn unicode_field_editing_undo_redo_preserves_cursor_and_legacy_serialization() {
        let mut input = TextInput::new("a界c".into());
        input.key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        input.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(input.text, "ac");
        input.key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL));
        assert_eq!(input.text, "a界c");
        assert_eq!(input.cursor, 2);
        input.key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL));
        assert_eq!(input.text, "ac");
        assert_eq!(input.cursor, 1);
        let old: TextInput =
            serde_json::from_str(r#"{"text":"legacy","cursor":6,"replace":false}"#).unwrap();
        assert!(old.undo.is_empty());
        assert!(old.redo.is_empty());
    }
}
