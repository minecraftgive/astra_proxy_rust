use astra_convert::{subscription::*, Options};

#[test]
fn subscriptions_become_groups_and_proxy_membership_is_controlled_on_each_group() {
    use astra_ipc::groups::{ensure_default, expand, validate, Group};
    use std::collections::{BTreeMap, HashSet};
    let mut doc: toml::Table = toml::from_str("[[outbounds]]\ntag='a'\nkind='socks5'\nserver='localhost:1'\n[[outbounds]]\ntag='b'\nkind='socks5'\nserver='localhost:2'\n[subscriptions.first]\nurl='https://example.com/a'\nmanaged_nodes=['a']\n[subscriptions.second]\nurl='https://example.com/b'\nmanaged_nodes=['b']\n").unwrap();
    prepare_groups(&mut doc).unwrap();
    assert_eq!(
        doc["proxy_groups"]["first"]["members"].as_array().unwrap(),
        &[toml::Value::String("a".into())]
    );
    assert_eq!(
        doc["proxy_groups"]["second"]["include_in_proxy"].as_bool(),
        Some(true)
    );
    doc["proxy_groups"]["first"]
        .as_table_mut()
        .unwrap()
        .insert("include_in_proxy".into(), false.into());
    doc["proxy_groups"]["first"]
        .as_table_mut()
        .unwrap()
        .insert("selected".into(), "a".into());
    prepare_groups(&mut doc).unwrap();
    let mut groups: BTreeMap<String, Group> = doc["proxy_groups"].clone().try_into().unwrap();
    let nodes: HashSet<_> = ["a", "b"].into_iter().map(str::to_owned).collect();
    ensure_default(&mut groups, &nodes).unwrap();
    let expanded = expand(&groups, &nodes).unwrap();
    assert_eq!(expanded["PROXY"].members, ["second"]);
    assert_eq!(expanded["first"].selected.as_deref(), Some("a"));
    validate(&groups, &nodes).unwrap();
    groups.get_mut("PROXY").unwrap().members = vec!["first".into(), "a".into()];
    assert_eq!(
        expand(&groups, &nodes).unwrap()["PROXY"].members,
        ["second"]
    );
}

#[test]
fn named_refresh_preserves_group_controls_and_other_subscription_members() {
    let current = "[subscriptions.first]\nurl='https://example.com/a'\n[subscriptions.second]\nurl='https://example.com/b'\n[proxy_groups.first]\nsubscription='first'\ninclude_in_proxy=false\nmode='auto'\n";
    let mut first = Subscription::default();
    let one = convert_nodes(&source("a"), &Options::default(), "test", false).unwrap();
    let two = convert_nodes(&source("b"), &Options::default(), "test", false).unwrap();
    let updated = merge_named(current, &one.proxy, &mut first, "first").unwrap();
    let mut second = Subscription::default();
    let updated = merge_named(&updated, &two.proxy, &mut second, "second").unwrap();
    let next = convert_nodes(&source("new-a"), &Options::default(), "test", false).unwrap();
    let updated = merge_named(&updated, &next.proxy, &mut first, "first").unwrap();
    let doc: toml::Value = toml::from_str(&updated).unwrap();
    assert_eq!(
        doc["proxy_groups"]["first"]["members"][0].as_str(),
        Some("new-a")
    );
    assert_eq!(
        doc["proxy_groups"]["first"]["include_in_proxy"].as_bool(),
        Some(false)
    );
    assert_eq!(doc["proxy_groups"]["first"]["mode"].as_str(), Some("auto"));
    assert_eq!(
        doc["proxy_groups"]["second"]["members"][0].as_str(),
        Some("b")
    );
    assert!(!doc["outbounds"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v["tag"].as_str() == Some("a")));
}
fn source(name: &str) -> String {
    format!("proxies: [{{name: {name}, type: socks5, server: 127.0.0.1, port: 1234}}]\nproxy-groups: [{{name: PROXY, type: select, include-all: true}}]\nrules: ['MATCH,PROXY']")
}
#[test]
fn nested_selection_survives_refresh_and_clears_when_node_disappears() {
    let current = "[subscriptions.remote]\nurl='https://example.com/sub'\n[proxy_groups.parent]\nmembers=['remote']\nselected='chosen'\n";
    let mut owned = Subscription::default();
    let nodes = convert_nodes(&source("chosen"), &Options::default(), "test", false).unwrap();
    let merged = merge_named(current, &nodes.proxy, &mut owned, "remote").unwrap();
    let doc: toml::Value = toml::from_str(&merged).unwrap();
    assert_eq!(
        doc["proxy_groups"]["parent"]["selected"].as_str(),
        Some("chosen")
    );
    let nodes = convert_nodes(&source("replacement"), &Options::default(), "test", false).unwrap();
    let merged = merge_named(&merged, &nodes.proxy, &mut owned, "remote").unwrap();
    let doc: toml::Value = toml::from_str(&merged).unwrap();
    assert!(doc["proxy_groups"]["parent"].get("selected").is_none());
}
#[test]
fn providers_membership_filters_and_yaml_anchors() {
    let text = "proxy-providers:\n  remote:\n    type: inline\n    payload:\n      - &node {name: JP, type: socks5, server: localhost, port: 1000}\n      - {<<: *node, name: US}\nproxy-groups:\n  - {name: Asia, type: select, use: [remote], filter: '^JP$'}\n  - {name: All, type: select, include-all-providers: true}\nrules: ['MATCH,Asia']";
    let result = convert_clash(text, &Options::default(), "test").unwrap();
    let proxy: toml::Value = toml::from_str(&result.proxy).unwrap();
    assert_eq!(result.nodes, 2);
    assert_eq!(
        proxy["proxy_groups"]["Asia"]["members"].as_array().unwrap(),
        &[toml::Value::String("JP".into())]
    );
    assert_eq!(
        proxy["proxy_groups"]["All"]["members"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(convert_clash(
        &text.replace("[remote]", "[unknown]"),
        &Options::default(),
        "test"
    )
    .is_err());
}
#[test]
fn refresh_replaces_owned_nodes_preserves_local_settings_and_manual_selection() {
    let initial = convert_clash(&source("old"), &Options::default(), "test").unwrap();
    let mut ownership = Subscription {
        managed_nodes: vec!["old".into()],
        managed_groups: vec!["PROXY".into()],
        import_groups: true,
        ..Default::default()
    };
    let current = format!("{}\n[tun]\nname='keep'\n", initial.proxy);
    let remote = convert_clash(&source("new"), &Options::default(), "test").unwrap();
    let merged = merge(&current, &remote.proxy, &mut ownership).unwrap();
    let table: toml::Value = toml::from_str(&merged).unwrap();
    assert_eq!(table["outbounds"][0]["tag"].as_str(), Some("new"));
    assert_eq!(table["tun"]["name"].as_str(), Some("keep"));
    assert_eq!(ownership.managed_nodes, ["new"]);
    let merged = merge(&merged, &initial.proxy, &mut ownership).unwrap();
    assert!(!merged.contains("tag = \"new\""));
    let mut local: toml::Table = toml::from_str(&merged).unwrap();
    local["proxy_groups"]["PROXY"]
        .as_table_mut()
        .unwrap()
        .insert("selected".into(), "old".into());
    local["proxy_groups"]["PROXY"]
        .as_table_mut()
        .unwrap()
        .insert("disconnect_on_auto".into(), true.into());
    let updated = merge(
        &toml::to_string(&local).unwrap(),
        &initial.proxy,
        &mut ownership,
    )
    .unwrap();
    let table: toml::Value = toml::from_str(&updated).unwrap();
    assert_eq!(
        table["proxy_groups"]["PROXY"]["selected"].as_str(),
        Some("old")
    );
    assert_eq!(
        table["proxy_groups"]["PROXY"]["disconnect_on_auto"].as_bool(),
        Some(true)
    );
}
#[test]
fn rejects_empty_conflicting_or_invalid_updates() {
    let config = convert_clash(&source("local"), &Options::default(), "test").unwrap();
    assert!(merge(&config.proxy, &config.proxy, &mut Subscription::default()).is_err());
    assert!(merge(&config.proxy, "outbounds=[]", &mut Subscription::default()).is_err());
    for url in [
        "file:///etc/passwd",
        "https://user:pass@example.com/a",
        "invalid",
    ] {
        assert!(validate_url(url).is_err());
    }
    assert!(Subscription {
        url: "https://example.com/secret".into(),
        interval: 0,
        ..Default::default()
    }
    .validate()
    .is_err());
}
#[test]
fn local_groups_override_remote_groups() {
    let initial = convert_clash(&source("old"), &Options::default(), "test").unwrap();
    let remote = convert_clash(&source("new"), &Options::default(), "test").unwrap();
    let mut ownership = Subscription {
        managed_nodes: vec!["old".into()],
        ..Default::default()
    };
    let result = merge(&initial.proxy, &remote.proxy, &mut ownership).unwrap();
    let table: toml::Value = toml::from_str(&result).unwrap();
    assert_eq!(
        table["proxy_groups"]["PROXY"]["include_all"].as_bool(),
        Some(true)
    );
    assert!(ownership.managed_groups.is_empty());
}

#[test]
fn node_import_ignores_remote_rules_groups_and_listeners_by_default() {
    let input = "proxies: [{name: only, type: socks5, server: localhost, port: 1234}]\nproxy-groups: invalid\nrules: invalid\nrule-providers: invalid\nsocks-port: invalid\ntun: {enable: true}";
    let converted = convert_nodes(input, &Options::default(), "test", false).unwrap();
    let proxy: toml::Value = toml::from_str(&converted.proxy).unwrap();
    assert_eq!(converted.nodes, 1);
    assert_eq!(proxy["socks5_listen"].as_str(), Some("127.0.0.1:1080"));
    // PROXY plus the built-in DIRECT and REJECT groups.
    assert_eq!(proxy["proxy_groups"].as_table().unwrap().len(), 3);
    assert_eq!(
        proxy["proxy_groups"]["DIRECT"]["members"].as_array().unwrap()[0].as_str(),
        Some("DIRECT")
    );
    assert!(proxy.get("tun").is_none());
    assert!(convert_nodes(input, &Options::default(), "test", true).is_err());
    let mut owned = Subscription {
        managed_nodes: vec!["only".into()],
        managed_groups: vec!["PROXY".into()],
        ..Default::default()
    };
    let mut current: toml::Table = toml::from_str(&converted.proxy).unwrap();
    current["proxy_groups"]["PROXY"]
        .as_table_mut()
        .unwrap()
        .insert("filter".into(), "^new$".into());
    let incoming = convert_clash(&source("new"), &Options::default(), "test").unwrap();
    let merged = merge(
        &toml::to_string(&current).unwrap(),
        &incoming.proxy,
        &mut owned,
    )
    .unwrap();
    let proxy: toml::Value = toml::from_str(&merged).unwrap();
    assert_eq!(
        proxy["proxy_groups"]["PROXY"]["filter"].as_str(),
        Some("^new$")
    );
    assert!(owned.managed_groups.is_empty());
}

#[test]
fn file_provider_resolves_against_input_directory() {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory =
        std::env::temp_dir().join(format!("astra-provider-{}-{nonce}", std::process::id()));
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("nodes.yaml");
    std::fs::write(&path, source("node")).unwrap();
    let result = convert_clash("proxy-providers: {p: {type: file, path: nodes.yaml}}\nproxy-groups: [{name: PROXY, type: select, use: [p]}]", &Options { base_dir: directory.clone(), ..Default::default() }, "test");
    std::fs::remove_file(path).unwrap();
    std::fs::remove_dir(directory).unwrap();
    assert_eq!(result.unwrap().nodes, 1);
}
#[test]
fn http_provider_download_and_error_redaction() {
    use std::io::{Read, Write};
    let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = server.local_addr().unwrap();
    let thread = std::thread::spawn(move || {
        for status in [200, 403] {
            let (mut stream, _) = server.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                .unwrap();
            let mut request = vec![];
            while !request.ends_with(b"\r\n\r\n") {
                let mut b = [0];
                stream.read_exact(&mut b).unwrap();
                request.push(b[0]);
            }
            assert!(String::from_utf8(request)
                .unwrap()
                .to_ascii_lowercase()
                .contains("user-agent: test-agent"));
            let body = source("remote");
            write!(
                stream,
                "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
    });
    let yaml = format!("proxy-providers: {{p: {{type: http, url: 'http://{address}/provider?token=secret'}}}}\nproxy-groups: [{{name: PROXY, type: select, use: [p]}}]");
    assert_eq!(
        convert_clash(&yaml, &Options::default(), "test-agent")
            .unwrap()
            .nodes,
        1
    );
    let error = convert_clash(&yaml, &Options::default(), "test-agent")
        .err()
        .unwrap();
    assert!(!format!("{error:#}").contains("secret"));
    thread.join().unwrap();
}

#[test]
fn injected_download_covers_subscription_and_nested_http_provider() {
    use astra_convert::subscription::{fetch_with, Subscription};
    let calls = std::cell::RefCell::new(Vec::new());
    let result = fetch_with(
        &Subscription {
            url: "https://subscription.test/list".into(),
            ..Default::default()
        },
        std::path::Path::new("."),
        &|url, _agent| {
            calls.borrow_mut().push(url.to_owned());
            Ok(if url.ends_with("/list") {
                "proxy-providers: {p: {type: http, url: 'https://provider.test/nodes'}}".into()
            } else {
                source("remote")
            })
        },
    )
    .unwrap();
    assert_eq!(result.nodes, 1);
    assert_eq!(
        *calls.borrow(),
        [
            "https://subscription.test/list",
            "https://provider.test/nodes"
        ]
    );
}

#[test]
fn fetch_diagnostics_distinguish_html_yaml_and_unsupported_nodes() {
    let config = Subscription {
        url: "https://example.com/private?token=secret".into(),
        ..Default::default()
    };
    for (body, expected) in [
        (
            "<!doctype html><html>login token=body-secret</html>",
            "HTML/XML",
        ),
        ("proxies: [", "YAML at line"),
        (
            "proxies: [{name: bad, type: imaginary, server: 127.0.0.1, port: 1234, password: body-secret}]",
            "不支持的协议 imaginary",
        ),
    ] {
        let error = fetch_with(&config, std::path::Path::new("."), &|_, _| Ok(body.into()))
            .err()
            .unwrap();
        let diagnostic = diagnostics::error(&error);
        assert!(diagnostic.contains(expected), "{diagnostic}");
        assert!(
            !diagnostic.contains("body-secret") && !diagnostic.contains("token=secret"),
            "{diagnostic}"
        );
    }
    let error = fetch_with(&config, std::path::Path::new("."), &|_, _| {
        Err(anyhow::anyhow!("connection refused"))
    })
    .err()
    .unwrap();
    assert!(diagnostics::error(&error).contains("download subscription: connection refused"));
}

#[test]
fn remote_subscriptions_accept_http_refuse_downgrade_and_local_files() {
    use astra_convert::subscription::{fetch_with, validate_redirect, Subscription};
    for url in [
        "http://example.com/sub",
        "http://192.168.1.1/sub",
        "http://[fd00::1]/sub",
        "https://example.com/sub",
        "http://127.0.0.1:8080/sub",
        "http://localhost/sub",
        "http://[::1]/sub",
    ] {
        assert!(validate_url(url).is_ok(), "{url}");
    }
    for url in [
        "http://user:pass@example.com/sub",
        "ftp://example.com/sub",
        "file:///etc/passwd",
    ] {
        assert!(validate_url(url).is_err(), "{url}");
    }
    assert!(validate_redirect("https://example.com/a", "http://127.0.0.1/b").is_err());
    assert!(validate_redirect("https://example.com/a", "https://cdn.example.com/b").is_ok());
    assert!(validate_redirect("http://example.com/a", "http://cdn.example.com/b").is_ok());
    assert!(validate_redirect("http://example.com/a", "https://cdn.example.com/b").is_ok());
    let error = fetch_with(
        &Subscription {
            url: "https://subscription.test/list".into(),
            ..Default::default()
        },
        std::path::Path::new("."),
        &|_, _| Ok("proxy-providers: {p: {type: file, path: /etc/passwd}}".into()),
    )
    .err()
    .unwrap();
    assert!(
        format!("{error:#}").contains("local file providers"),
        "{error:#}"
    );
}
