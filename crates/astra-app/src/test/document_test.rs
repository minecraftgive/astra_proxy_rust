use super::*;

#[test]
fn diagnostic_connect_timeout_is_editable_validated_and_defaults_for_old_profiles() {
    use astra_model::config::proxy::ProxyConfig;
    let source = "standalone=true\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\nkind='socks5'\ntag='node'\nserver='127.0.0.1:1081'\n";
    let mut document = Document::parse(CoreKind::Proxy, source).unwrap();
    let parse = |text: &str| toml::from_str::<ProxyConfig>(text).unwrap();
    assert_eq!(parse(source).diagnostic_connect_timeout_secs, 3);
    for platform in [
        Platform::Windows,
        Platform::Macos,
        Platform::Linux,
        Platform::Other,
    ] {
        let schema = document.schema(&[], platform).unwrap();
        let field = schema
            .rows
            .iter()
            .find_map(|row| match row {
                SchemaRow::Field { field } if field.key == "diagnostic_connect_timeout_secs" => {
                    Some(field)
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(field.default, "3");
        assert!(field.value.is_none());
    }
    for seconds in [1, 15, 60] {
        document
            .apply(EditCommand::SetField {
                path: vec![],
                key: "diagnostic_connect_timeout_secs".into(),
                input: seconds.to_string(),
            })
            .unwrap();
        document.validate_structure().unwrap();
        let config = parse(document.text());
        assert_eq!(config.diagnostic_connect_timeout_secs, seconds);
        assert_eq!(config.connect_timeout_secs, 8);
        assert_eq!(
            parse(&toml::to_string(&config).unwrap()).diagnostic_connect_timeout_secs,
            seconds
        );
    }
    document
        .apply(EditCommand::RemoveField {
            path: vec![],
            key: "diagnostic_connect_timeout_secs".into(),
        })
        .unwrap();
    assert_eq!(parse(document.text()).diagnostic_connect_timeout_secs, 3);
    for invalid in ["0", "61", "-1", "'15'", "1.5", "18446744073709551615"] {
        assert!(Document::parse(
            CoreKind::Proxy,
            &format!("diagnostic_connect_timeout_secs={invalid}\n{source}")
        )
        .and_then(|document| document.validate_structure())
        .is_err());
    }
}

fn roundtrip_auto_start(kind: CoreKind, text: &str) -> (bool, String) {
    macro_rules! roundtrip {
        ($config:ty) => {{
            let config: $config = toml::from_str(text).unwrap();
            let serialized = toml::to_string(&config).unwrap();
            let restored: $config = toml::from_str(&serialized).unwrap();
            assert_eq!(config.auto_start, restored.auto_start);
            (restored.auto_start, serialized)
        }};
    }
    match kind {
        CoreKind::Proxy => roundtrip!(astra_model::config::proxy::ProxyConfig),
        CoreKind::Rule => roundtrip!(astra_model::config::rule::RuleConfig),
        CoreKind::Server => roundtrip!(astra_model::config::server::Config),
    }
}

#[test]
fn instance_auto_start_is_opt_in_and_roundtrips_for_every_core_kind() {
    for kind in [CoreKind::Proxy, CoreKind::Rule, CoreKind::Server] {
        let template = Document::template(kind);
        assert_eq!(
            toml::from_str::<toml::Value>(&template).unwrap()["auto_start"].as_bool(),
            Some(false)
        );
        let legacy = template.replace("auto_start = false\n", "");
        assert!(!roundtrip_auto_start(kind, &legacy).0);
        for enabled in [false, true] {
            let text = format!("auto_start = {enabled}\n{legacy}");
            Document::parse(kind, &text)
                .unwrap()
                .validate_structure()
                .unwrap();
            let (restored, serialized) = roundtrip_auto_start(kind, &text);
            assert_eq!(restored, enabled);
            assert_eq!(
                toml::from_str::<toml::Value>(&serialized).unwrap()["auto_start"].as_bool(),
                Some(enabled)
            );
        }
        for invalid in ["'true'", "1", "[]"] {
            assert!(
                Document::parse(kind, &format!("auto_start = {invalid}\n{legacy}"))
                    .unwrap()
                    .validate_structure()
                    .is_err()
            );
        }
    }
}

#[test]
fn instance_auto_start_schema_edits_save_and_reset_on_all_platforms() {
    for kind in [CoreKind::Proxy, CoreKind::Rule, CoreKind::Server] {
        let legacy = Document::template(kind).replace("auto_start = false\n", "");
        let mut document = Document::parse(kind, &legacy).unwrap();
        for platform in [
            Platform::Windows,
            Platform::Macos,
            Platform::Linux,
            Platform::Other,
        ] {
            let schema = document.schema(&[], platform).unwrap();
            let field = schema
                .rows
                .iter()
                .find_map(|row| match row {
                    SchemaRow::Field { field } if field.key == "auto_start" => Some(field),
                    _ => None,
                })
                .unwrap();
            assert_eq!(field.field_type, FieldType::Boolean);
            assert_eq!(field.default, "false");
            assert!(field.value.is_none());
            assert!(!field.required);
        }
        for enabled in [true, false] {
            document
                .apply(EditCommand::SetField {
                    path: vec![],
                    key: "auto_start".into(),
                    input: enabled.to_string(),
                })
                .unwrap();
            document.validate_structure().unwrap();
            assert_eq!(roundtrip_auto_start(kind, document.text()).0, enabled);
            document = Document::parse(kind, document.text()).unwrap();
        }
        document
            .apply(EditCommand::RemoveField {
                path: vec![],
                key: "auto_start".into(),
            })
            .unwrap();
        assert!(!roundtrip_auto_start(kind, document.text()).0);
    }
}

#[test]
fn edits_keep_comments_unknown_fields_and_exact_undo() {
    let source = "# owned comment\ndefault_action = 'DIRECT' # inline\nunknown = 'preserved'\n";
    let mut doc = Document::parse(CoreKind::Rule, source).unwrap();
    assert_eq!(doc.text(), source);
    doc.apply(EditCommand::SetField {
        path: vec![],
        key: "default_action".into(),
        input: "REJECT".into(),
    })
    .unwrap();
    assert!(doc.text().contains("# owned comment"));
    assert!(doc.text().contains("# inline"));
    assert!(doc.text().contains("unknown = 'preserved'"));
    let changed = doc.text().to_owned();
    assert!(doc.undo());
    assert_eq!(doc.text(), source);
    assert!(doc.redo());
    assert_eq!(doc.text(), changed);
}
#[test]
fn failed_edit_is_atomic_and_does_not_discard_redo() {
    let mut doc = Document::parse(CoreKind::Server, &Document::template(CoreKind::Server)).unwrap();
    let source = doc.text().to_owned();
    doc.apply(EditCommand::SetField {
        path: vec![],
        key: "max_connections".into(),
        input: "2000".into(),
    })
    .unwrap();
    let updated = doc.text().to_owned();
    assert!(doc.undo());
    assert!(doc
        .apply(EditCommand::SetField {
            path: vec![],
            key: "max_connections".into(),
            input: "-1".into()
        })
        .is_err());
    assert_eq!(doc.text(), source);
    assert!(doc.redo());
    assert_eq!(doc.text(), updated);
}
#[test]
fn rename_updates_references_and_preserves_unknown_sections() {
    let source="standalone_outbound='old'\n[[outbounds]]\ntag='old'\nkind='socks5'\nserver='127.0.0.1:1080'\n[proxy_groups.custom]\nmode='select'\nmembers=['old']\nselected='old'\n[future]\nfeature=true\n";
    let mut doc = Document::parse(CoreKind::Proxy, source).unwrap();
    doc.apply(EditCommand::RenameProxy {
        previous: "old".into(),
        next: "new".into(),
    })
    .unwrap();
    let parsed: toml::Value = toml::from_str(doc.text()).unwrap();
    assert_eq!(parsed["standalone_outbound"].as_str(), Some("new"));
    assert_eq!(
        parsed["proxy_groups"]["custom"]["members"][0].as_str(),
        Some("new")
    );
    assert_eq!(parsed["future"]["feature"].as_bool(), Some(true));
    assert!(doc.undo());
    assert_eq!(doc.text(), source);
}
#[test]
fn rule_set_rename_and_move_keep_nested_tables_with_their_entry() {
    let source="[[rules]]\nkind='rule_set'\nvalue='old'\naction='DIRECT'\n[[rules]]\nkind='domain_suffix'\nvalue='test'\naction='REJECT'\n[rule_sets.old]\npath='local.txt' # preserve\n";
    let mut doc = Document::parse(CoreKind::Rule, source).unwrap();
    doc.apply(EditCommand::RenameRuleSet {
        previous: "old".into(),
        next: "new".into(),
    })
    .unwrap();
    assert!(doc.text().contains("# preserve"));
    doc.apply(EditCommand::MoveEntry {
        path: vec![Part::Key("rules".into())],
        from: 0,
        to: 1,
    })
    .unwrap();
    let parsed: toml::Value = toml::from_str(doc.text()).unwrap();
    assert_eq!(parsed["rules"][1]["value"].as_str(), Some("new"));
    assert_eq!(
        parsed["rule_sets"]["new"]["path"].as_str(),
        Some("local.txt")
    );
}
#[test]
fn all_templates_parse_and_server_secrets_are_marked() {
    for kind in [CoreKind::Proxy, CoreKind::Rule, CoreKind::Server] {
        Document::parse(kind, &Document::template(kind))
            .unwrap()
            .validate_structure()
            .unwrap();
    }
    let doc = Document::parse(
        CoreKind::Server,
        "[[listeners]]\ntag='a'\nprotocol='trojan'\nlisten='0.0.0.0:443'\npassword='secret'\n",
    )
    .unwrap();
    let schema = doc
        .schema(
            &[Part::Key("listeners".into()), Part::Index(0)],
            Platform::Linux,
        )
        .unwrap();
    assert!(schema
        .rows
        .iter()
        .any(|r| matches!(r,SchemaRow::Field {field} if field.key=="password" && field.secret)));
    let dto = toml::Value::try_from(astra_model::config::server::Config::default()).unwrap();
    let root = doc.schema(&[], Platform::Linux).unwrap();
    for (key, _) in dto.as_table().unwrap() {
        assert!(
            root.rows.iter().any(|r| match r {
                SchemaRow::Field { field } => &field.key == key,
                SchemaRow::Group { key: k, .. } => k == key,
                _ => false,
            }),
            "missing {key}"
        );
    }
}
#[test]
fn invalid_or_oversize_replacement_preserves_original() {
    let mut doc = Document::parse(CoreKind::Rule, "# draft\n").unwrap();
    assert!(doc
        .apply(EditCommand::ReplaceText { text: "[[".into() })
        .is_err());
    assert!(doc
        .apply(EditCommand::ReplaceText {
            text: " ".repeat(DOCUMENT_LIMIT + 1)
        })
        .is_err());
    assert_eq!(doc.text(), "# draft\n");
}
#[test]
fn schema_reports_values_and_empty_collections_without_rewriting_inline_data() {
    let source="default_action='direct'\nrules = [{ kind = 'network', value = 'tcp', action = 'direct' }] # inline\n[extension]\nitems = [{unknown = true}]\n";
    let mut document = Document::parse(CoreKind::Rule, source).unwrap();
    let schema = document.schema(&[], Platform::Linux).unwrap();
    assert!(schema.rows.iter().any(|row|matches!(row,SchemaRow::Field {field} if field.key=="default_action" && field.value.as_deref()==Some("direct"))));
    document
        .apply(EditCommand::SetField {
            path: vec![],
            key: "default_action".into(),
            input: "block".into(),
        })
        .unwrap();
    assert!(document
        .text()
        .contains("rules = [{ kind = 'network', value = 'tcp', action = 'direct' }] # inline"));
    assert!(document.text().contains("items = [{unknown = true}]"));
    let mut server = Document::parse(CoreKind::Server, "listeners=[]\n").unwrap();
    let listeners = vec![Part::Key("listeners".into())];
    assert!(
        server
            .schema(&listeners, Platform::Linux)
            .unwrap()
            .collection
    );
    server
        .apply(EditCommand::AppendEntry { path: listeners })
        .unwrap();
    let users = vec![
        Part::Key("listeners".into()),
        Part::Index(0),
        Part::Key("users".into()),
    ];
    assert!(server.schema(&users, Platform::Linux).unwrap().collection);
    server
        .apply(EditCommand::AppendEntry { path: users })
        .unwrap();
    server.validate_structure().unwrap();
}
#[test]
fn structure_validation_runs_without_resource_or_host_access() {
    let server=Document::parse(CoreKind::Server,"[[listeners]]\ntag='tls'\nprotocol='trojan'\nlisten='127.0.0.1:443'\n[[listeners.users]]\nname='owner'\npassword='secret'\n[listeners.tls]\ncertificate='missing-cert.pem'\nprivate_key='missing-key.pem'\n").unwrap();
    server.validate_structure().unwrap();
    let invalid = Document::parse(
        CoreKind::Rule,
        "socket='r'\ndefault_action='direct'\n[[rules]]\nkind='port'\nvalue='0'\naction='direct'\n",
    )
    .unwrap();
    assert!(invalid.validate_structure().is_err());
}

#[test]
fn chain_editor_preserves_order_and_protects_front_references() {
    let source = "standalone=true\nstandalone_outbound='exit'\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\nkind='socks5'\ntag='entry'\nserver='127.0.0.1:1081'\n[[outbounds]]\nkind='socks5'\ntag='exit'\nserver='127.0.0.1:1082'\ndialer_proxy='entry'\n[proxy_groups.chain]\nmembers=['entry','exit']\nselected='exit'\n";
    let mut doc = Document::parse(CoreKind::Proxy, source).unwrap();
    doc.apply(EditCommand::SetField {
        path: vec![Part::Key("proxy_groups".into()), Part::Key("chain".into())],
        key: "mode".into(),
        input: "relay".into(),
    })
    .unwrap();
    doc.validate_structure().unwrap();
    assert!(!doc.text.contains("selected="));
    assert!(doc
        .apply(EditCommand::RemoveEntry {
            path: vec![Part::Key("outbounds".into())],
            index: 0
        })
        .is_err());
    let schema = doc
        .schema(
            &[Part::Key("outbounds".into()), Part::Index(1)],
            Platform::current(),
        )
        .unwrap();
    assert!(schema.rows.iter().any(|row| matches!(row, SchemaRow::Field { field } if field.key == "dialer_proxy" && field.help.contains("entry entry → exit exit"))));

    let mut alias = Document::parse(
        CoreKind::Proxy,
        &doc.text().replace("dialer_proxy", "dialer-proxy"),
    )
    .unwrap();
    let path = vec![Part::Key("outbounds".into()), Part::Index(1)];
    let schema = alias.schema(&path, Platform::current()).unwrap();
    assert!(schema.rows.iter().any(|row| matches!(row, SchemaRow::Field { field } if field.key == "dialer-proxy" && field.value.as_deref() == Some("entry"))));
    assert!(!schema
        .rows
        .iter()
        .any(|row| matches!(row, SchemaRow::Field { field } if field.key == "dialer_proxy")));
    alias
        .apply(EditCommand::SetField {
            path,
            key: "dialer-proxy".into(),
            input: "DIRECT".into(),
        })
        .unwrap();
    alias.validate_structure().unwrap();
}
