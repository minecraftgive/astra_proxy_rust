use astra_convert::{subscription::*, Options};
fn source(name: &str) -> String {
    format!("proxies: [{{name: {name}, type: socks5, server: 127.0.0.1, port: 1234}}]\nproxy-groups: [{{name: PROXY, type: select, include-all: true}}]\nrules: ['MATCH,PROXY']")
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
    assert_eq!(proxy["proxy_groups"].as_table().unwrap().len(), 1);
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
