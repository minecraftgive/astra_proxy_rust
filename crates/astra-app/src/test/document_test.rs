use super::*;
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
