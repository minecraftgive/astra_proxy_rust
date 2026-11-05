use super::*;
pub struct Fixture(pub PathBuf);
impl Fixture {
    pub fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "astra-import-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }
    pub fn write(&self, name: &str, text: impl AsRef<[u8]>) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[test]
fn selected_nodes_keep_source_order_nested_tables_and_only_selected_conflicts() {
    let current = "# keep draft\nstandalone=true\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\ntag='old'\nkind='direct'\n";
    let source = "standalone=false\nsocks5_listen='0.0.0.0:9999'\n[[outbounds]]\ntag='old'\nkind='direct'\n[[outbounds]]\ntag='first'\nkind='native'\n[outbounds.config]\nprotocol='vless'\nserver='first.example'\n[outbounds.config.tls]\nserver_name='first.example'\n[[outbounds]]\ntag='second'\nkind='native'\n[outbounds.config]\nprotocol='trojan'\nserver='second.example'\n[outbounds.config.transport]\nkind='ws'\npath='/second'\n[subscriptions.remote]\nurl='https://example.test/sub'\n";
    let candidates = node_candidates_at(source, Path::new(".")).unwrap();
    assert_eq!(candidates.tags, ["old", "first", "second"]);
    let preview: toml::Table = toml::from_str(&candidates.text).unwrap();
    assert_eq!(preview.len(), 1);
    let merged = merge_selected_nodes(current, &candidates, &[2, 1], MergeMode::Append).unwrap();
    assert!(merged.text.contains("# keep draft"));
    let value: toml::Value = toml::from_str(&merged.text).unwrap();
    assert_eq!(value["standalone"].as_bool(), Some(true));
    assert_eq!(value["socks5_listen"].as_str(), Some("127.0.0.1:1080"));
    assert!(value.get("subscriptions").is_none());
    let nodes = value["outbounds"].as_array().unwrap();
    assert_eq!(
        nodes
            .iter()
            .map(|n| n["tag"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["old", "first", "second"]
    );
    assert_eq!(
        nodes[1]["config"]["tls"]["server_name"].as_str(),
        Some("first.example")
    );
    assert_eq!(
        nodes[2]["config"]["transport"]["path"].as_str(),
        Some("/second")
    );
    assert!(nodes[2]["config"].get("tls").is_none());
    assert!(merge_selected_nodes(current, &candidates, &[0, 1], MergeMode::Append).is_err());
}

#[test]
fn selected_nodes_validate_indices_duplicate_tags_and_merge_modes() {
    let source =
        "outbounds=[{tag='edge',kind='socks5',server='new:1080'}, {tag='other',kind='direct'}]";
    let mut candidates = node_candidates_at(source, Path::new(".")).unwrap();
    let current = "standalone=true\noutbounds=[{tag='edge',kind='socks5',server='old:1080'}]";
    for selected in [&[][..], &[0, 0][..], &[2][..]] {
        assert!(merge_selected_nodes(current, &candidates, selected, MergeMode::Append).is_err());
    }
    for selected in [&[1][..], &[0, 1][..]] {
        assert!(
            merge_selected_nodes(current, &candidates, selected, MergeMode::Overwrite(0)).is_err()
        );
    }
    let replaced =
        merge_selected_nodes(current, &candidates, &[0], MergeMode::Overwrite(0)).unwrap();
    let value: toml::Value = toml::from_str(&replaced.text).unwrap();
    assert_eq!(value["outbounds"][0]["server"].as_str(), Some("new:1080"));
    assert_eq!(value["outbounds"].as_array().unwrap().len(), 1);
    let replaced = merge_selected_nodes(current, &candidates, &[1], MergeMode::Replace).unwrap();
    let value: toml::Value = toml::from_str(&replaced.text).unwrap();
    assert_eq!(value["outbounds"][0]["tag"].as_str(), Some("other"));
    assert_eq!(value["standalone"].as_bool(), Some(true));
    candidates.tags[0] = "changed".into();
    assert!(merge_selected_nodes(current, &candidates, &[0], MergeMode::Overwrite(0)).is_err());

    let duplicate = node_candidates_at(
        "outbounds=[{tag='same',kind='direct'}, {tag='same',kind='direct'}]",
        Path::new("."),
    )
    .unwrap();
    assert!(merge_selected_nodes("", &duplicate, &[1], MergeMode::Append).is_ok());
    assert!(merge_selected_nodes("", &duplicate, &[0, 1], MergeMode::Append).is_err());
}

#[test]
fn node_candidates_accept_multiple_dropped_paths_and_snapshot_files() {
    let fixture = Fixture::new();
    let first = fixture.write("节点 one.toml", "outbounds=[{tag='one',kind='direct'}]");
    fixture.write(
        "two.yaml",
        "proxies:\n  - {name: two, type: socks5, server: 127.0.0.1, port: 1080}\n",
    );
    for input in [
        "\"节点 one.toml\" 'two.yaml'",
        "& '节点 one.toml'\n\"two.yaml\"",
    ] {
        let candidates = node_candidates_at(input, &fixture.0).unwrap();
        assert_eq!(candidates.tags, ["one", "two"]);
    }
    for input in ["节点 one.toml", "\"节点 one.toml\"", "& '节点 one.toml'"] {
        assert_eq!(node_candidates_at(input, &fixture.0).unwrap().tags, ["one"]);
    }
    fixture.write(
        "two.yaml three.toml",
        "outbounds=[{tag='single path',kind='direct'}]",
    );
    assert_eq!(
        node_candidates_at("two.yaml three.toml", &fixture.0)
            .unwrap()
            .tags,
        ["single path"]
    );
    let candidates = node_candidates_at("\"节点 one.toml\" two.yaml", &fixture.0).unwrap();
    std::fs::write(first, "broken after preview").unwrap();
    let imported =
        merge_selected_nodes("standalone=true", &candidates, &[0], MergeMode::Append).unwrap();
    assert_eq!(
        toml::from_str::<toml::Value>(&imported.text).unwrap()["outbounds"][0]["tag"].as_str(),
        Some("one")
    );
    for input in ["two.yaml missing.toml", "two.yaml '节点 one.toml'"] {
        assert!(node_candidates_at(input, &fixture.0).is_err());
    }
}

#[test]
fn node_candidates_keep_conversion_warnings_and_support_share_links() {
    let yaml = "\"proxies\":\n  - {name: good, type: socks5, server: 127.0.0.1, port: 1080}\n  - {name: unsupported, type: unknown, server: 127.0.0.1, port: 1080}\n";
    let candidates = node_candidates_at(yaml, Path::new(".")).unwrap();
    assert_eq!(candidates.tags, ["good"]);
    assert!(candidates
        .warnings
        .iter()
        .any(|warning| warning.contains("unsupported")));
    let merged =
        merge_selected_nodes("standalone=true", &candidates, &[0], MergeMode::Append).unwrap();
    assert_eq!(merged.warnings, candidates.warnings);
    let link = "vless://00000000-0000-4000-8000-000000000001@example.com:443?security=tls&sni=example.com&type=ws&path=%2Fhello#first";
    let links = format!("{link}\n{}", link.replace("#first", "#second"));
    let candidates = node_candidates_at(&links, Path::new(".")).unwrap();
    assert_eq!(candidates.tags, ["first", "second"]);
    let merged =
        merge_selected_nodes("standalone=true", &candidates, &[1], MergeMode::Append).unwrap();
    let node = &toml::from_str::<toml::Value>(&merged.text).unwrap()["outbounds"][0];
    assert_eq!(node["tag"].as_str(), Some("second"));
    assert_eq!(node["config"]["transport"]["path"].as_str(), Some("/hello"));
}

#[test]
fn quoted_reality_batch_candidates_and_selected_merge_preserve_native_fields() {
    const PUBLIC_KEY: &str = "AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA";
    let lines = [
            "%F0%9F%8C%8F%20%E9%A6%99%E6%B8%AF",
            "%F0%9F%9A%80%20%E6%97%A5%E6%9C%AC",
            "%F0%9F%8C%9F%20%E6%96%B0%E5%8A%A0%E5%9D%A1",
        ].iter().enumerate().map(|(index, label)| {
            let number = index + 1;
            format!("vless://00000000-0000-4000-8000-00000000000{number}@edge{number}.example.invalid:443?encryption=none&headerType=&spx=%2F&fp=chrome&type=tcp&flow=xtls-rprx-vision&security=reality&sni=front.example.invalid&sid=abcd&pbk={PUBLIC_KEY}#{label}")
        }).collect::<Vec<_>>().join("\n");
    let current = "# keep draft\nstandalone=true\nsocks5_listen='127.0.0.1:1080'\n";
    for quote in ['"', '\''] {
        let batch = format!("{quote}{lines}{quote}");
        assert!(astra_convert::share::is_share(&batch));
        let candidates = node_candidates_at(&batch, Path::new(".")).unwrap();
        assert_eq!(candidates.tags, ["🌏 香港", "🚀 日本", "🌟 新加坡"]);
        assert_eq!(
            links(&batch, Kind::Proxy, current).unwrap().warnings,
            candidates.warnings
        );
        let imported =
            merge_selected_nodes(current, &candidates, &[2, 0], MergeMode::Append).unwrap();
        assert!(imported.text.contains("# keep draft"));
        let document: toml::Value = toml::from_str(&imported.text).unwrap();
        assert_eq!(document["socks5_listen"].as_str(), Some("127.0.0.1:1080"));
        let nodes = document["outbounds"].as_array().unwrap();
        assert_eq!(nodes.len(), 2);
        for (node, number, label) in [(&nodes[0], 1, "🌏 香港"), (&nodes[1], 3, "🌟 新加坡")]
        {
            assert_eq!(node["tag"].as_str(), Some(label));
            assert_eq!(node["kind"].as_str(), Some("native"));
            let config = &node["config"];
            assert_eq!(config["protocol"].as_str(), Some("vless"));
            assert_eq!(
                config["server"].as_str(),
                Some(format!("edge{number}.example.invalid").as_str())
            );
            assert_eq!(config["port"].as_integer(), Some(443));
            assert_eq!(
                config["uuid"].as_str(),
                Some(format!("00000000-0000-4000-8000-00000000000{number}").as_str())
            );
            assert_eq!(config["encryption"].as_str(), Some("none"));
            assert_eq!(config["flow"].as_str(), Some("xtls-rprx-vision"));
            assert_eq!(
                config["tls"]["server_name"].as_str(),
                Some("front.example.invalid")
            );
            assert_eq!(config["tls"]["fingerprint"].as_str(), Some("chrome"));
            assert_eq!(
                config["tls"]["reality_public_key"].as_str(),
                Some(PUBLIC_KEY)
            );
            assert_eq!(config["tls"]["reality_short_id"].as_str(), Some("abcd"));
            assert!(config.get("transport").is_none());
        }
    }
}

#[test]
fn node_candidates_reject_bad_sources_and_bound_cumulative_file_sizes() {
    let fixture = Fixture::new();
    let padding = format!(
        "# {}\noutbounds=[{{tag='edge',kind='direct'}}]",
        "x".repeat(LIMIT / 2)
    );
    fixture.write("first.toml", &padding);
    fixture.write("second.toml", &padding);
    assert!(node_candidates_at("first.toml", &fixture.0).is_ok());
    assert!(node_candidates_at("first.toml second.toml", &fixture.0).is_err());
    fixture.write("bad.toml", "outbounds=[");
    fixture.write("binary.toml", [0xff, 0xfe]);
    for input in [
        "first.toml bad.toml",
        "first.toml binary.toml",
        "",
        "outbounds=[]",
        "outbounds=[{kind='direct'}]",
    ] {
        assert!(node_candidates_at(input, &fixture.0).is_err(), "{input}");
    }
    assert!(node_candidates_at(&"x".repeat(LIMIT + 1), &fixture.0).is_err());
    let candidates =
        node_candidates_at("outbounds=[{tag='edge',kind='direct'}]", &fixture.0).unwrap();
    assert!(
        merge_selected_nodes(&"x".repeat(LIMIT + 1), &candidates, &[0], MergeMode::Append).is_err()
    );
}

#[test]
fn share_links_append_without_replacing_draft_or_local_settings() {
    let link = "vless://00000000-0000-4000-8000-000000000001@example.com:443?security=tls&sni=example.com&type=xhttp&mode=auto&path=%2Fhello#new-node";
    let current = "# keep comment\nstandalone=true\nstandalone_outbound='PROXY'\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\nkind='socks5'\ntag='local'\nserver='localhost:1234'\n[subscriptions.local]\nurl='https://example.com/sub'\n";
    let imported = links(link, Kind::Proxy, current).unwrap();
    assert!(imported.text.contains("# keep comment"));
    let doc: toml::Value = toml::from_str(&imported.text).unwrap();
    assert_eq!(doc["outbounds"].as_array().unwrap().len(), 2);
    assert_eq!(
        doc["subscriptions"]["local"]["url"].as_str(),
        Some("https://example.com/sub")
    );
    assert!(links(link, Kind::Proxy, &imported.text).is_err());
    assert!(links(link, Kind::Rule, current).is_err());
}
#[test]
fn fragment_helpers_append_nodes_rules_and_sets_with_conflict_checks() {
    let node = "[[outbounds]]\nkind='socks5'\ntag='new'\nserver='127.0.0.1:1'\n";
    let imported = load_node(
        node,
        "standalone=true\n[[outbounds]]\ntag='old'\nkind='direct'\n",
    )
    .unwrap();
    let value: toml::Value = toml::from_str(&imported.text).unwrap();
    assert_eq!(value["outbounds"].as_array().unwrap().len(), 2);
    assert!(load_node(node, &imported.text).is_err());

    let rules = "[[rules]]\nkind='domain_suffix'\nvalue='example.com'\naction='direct'\n[rule_sets.ads]\nbehavior='domain'\n";
    let imported = load_rules(rules, "socket='rules.sock'\n").unwrap();
    let value: toml::Value = toml::from_str(&imported.text).unwrap();
    assert_eq!(value["rules"].as_array().unwrap().len(), 1);
    assert_eq!(
        value["rule_sets"]["ads"]["behavior"].as_str(),
        Some("domain")
    );
    assert!(load_rules(rules, &imported.text).is_err());
    assert!(load_rules("[rule_sets.ads]\nbehavior='ip'\n[[rules]]\nkind='ip_cidr'\nvalue='127.0.0.1/32'\naction='direct'\n", &imported.text).is_err());
}

#[test]
fn appended_native_nodes_keep_nested_tables_with_their_node() {
    let link = "vless://00000000-0000-4000-8000-000000000001@example.com:443?security=tls&sni=example.com&type=xhttp&mode=auto&path=%2Fhello#first";
    let current = "standalone=true\n[[outbounds]]\nkind='native'\ntag='existing'\n[outbounds.config]\nprotocol='vless'\nserver='old.example:443'\n[outbounds.config.tls]\nenabled=true\nserver_name='old.example'\n";
    let first = links(link, Kind::Proxy, current).unwrap();
    let second = links(&link.replace("#first", "#second"), Kind::Proxy, &first.text).unwrap();
    let doc: toml::Value = toml::from_str(&second.text).unwrap();
    let nodes = doc["outbounds"].as_array().unwrap();
    assert_eq!(nodes.len(), 3);
    assert_eq!(
        nodes[0]["config"]["server"].as_str(),
        Some("old.example:443")
    );
    for (node, tag) in nodes[1..].iter().zip(["first", "second"]) {
        assert_eq!(node["tag"].as_str(), Some(tag));
        assert_eq!(
            node["config"]["tls"]["server_name"].as_str(),
            Some("example.com")
        );
        assert_eq!(node["config"]["transport"]["path"].as_str(), Some("/hello"));
    }
}
#[test]
fn dropped_paths_handle_spaces_unicode_quotes_and_powershell_prefix_without_shell() {
    let directory = std::env::current_dir().unwrap();
    for text in [
        "配置 file.toml",
        "\"配置 file.toml\"",
        "'配置 file.toml'",
        "& '配置 file.toml'",
    ] {
        assert_eq!(
            path(text, &directory).unwrap(),
            directory.join("配置 file.toml")
        );
    }
    for text in ["", "\"\"", "'a.toml'\n'b.toml'", "a\0.toml"] {
        assert!(path(text, &directory).is_err());
    }
    assert!(path("'", &directory).is_ok()); // A literal filename; never panic on one quote.
}
#[test]
fn native_import_keeps_comments_and_rebases_both_rule_set_forms() {
    let fixture = Fixture::new();
    for sets in [
        "[rule_sets.local]\npath='lists/local.yaml'\n",
        "[rule_sets]\nlocal={path='lists/local.yaml'}\n",
    ] {
        let path = fixture.write("规则 TOML.toml", format!("\u{feff}# keep me\nsocket='rule.sock'\ngeoip_database='Country.mmdb'\ngeosite_database='geosite.dat'\n{sets}"));
        let imported = load(&path, Kind::Rule, "").unwrap();
        assert!(imported.text.contains("# keep me"));
        let doc: toml::Value = toml::from_str(&imported.text).unwrap();
        assert_eq!(
            doc["geoip_database"].as_str().unwrap(),
            fixture.0.join("Country.mmdb").to_str().unwrap()
        );
        assert_eq!(
            doc["rule_sets"]["local"]["path"].as_str().unwrap(),
            fixture.0.join("lists/local.yaml").to_str().unwrap()
        );
        assert_eq!(doc["socket"].as_str(), Some("rule.sock"));
        assert!(imported.warnings.is_empty());
    }
}
#[test]
fn mihomo_import_selects_core_preserves_endpoint_and_stays_offline() {
    let fixture = Fixture::new();
    let path = fixture.write("配置.yml", "socks-port: 1088\nproxies:\n  - {name: edge, type: socks5, server: 127.0.0.1, port: 1080}\nrule-providers:\n  local: {type: file, behavior: domain, path: './local.yaml'}\nrules:\n  - RULE-SET,local,edge\n  - GEOSITE,cn,DIRECT\n  - MATCH,edge\n");
    let proxy = load(&path, Kind::Proxy, "rule_socket='bound.rules'").unwrap();
    let proxy: toml::Value = toml::from_str(&proxy.text).unwrap();
    assert_eq!(proxy["rule_socket"].as_str(), Some("bound.rules"));
    assert_eq!(proxy["outbounds"][0]["tag"].as_str(), Some("edge"));
    assert!(proxy.get("rules").is_none());
    let rules = load(&path, Kind::Rule, "socket='bound.rules'").unwrap();
    assert!(!rules.warnings.is_empty());
    let rules: toml::Value = toml::from_str(&rules.text).unwrap();
    assert_eq!(rules["socket"].as_str(), Some("bound.rules"));
    assert_eq!(rules["rules"][1]["kind"].as_str(), Some("geosite"));
    assert_eq!(
        rules["rule_sets"]["local"]["path"].as_str().unwrap(),
        fixture.0.join("./local.yaml").to_str().unwrap()
    );
    assert!(!fixture.0.join("geosite").exists());
}
#[test]
fn invalid_wrong_core_large_and_unsupported_inputs_are_rejected() {
    let fixture = Fixture::new();
    for (name, text, kind) in [
        ("wrong.toml", "socket='r'", Kind::Proxy),
        ("wrong2.toml", "standalone=true", Kind::Rule),
        ("bad.toml", "socket=[", Kind::Rule),
        ("empty.yaml", "", Kind::Rule),
        ("bad.yml", "rules: ['UNSUPPORTED,test,DIRECT']", Kind::Rule),
        ("data.txt", "socket='r'", Kind::Rule),
    ] {
        assert!(
            load(&fixture.write(name, text), kind, "socket='bound'").is_err(),
            "{name}"
        );
    }
    assert!(load(
        &fixture.write("large.toml", vec![b'a'; LIMIT + 1]),
        Kind::Rule,
        ""
    )
    .is_err());
    assert!(load(&fixture.write("binary.toml", [0xff, 0xfe]), Kind::Rule, "").is_err());
}
