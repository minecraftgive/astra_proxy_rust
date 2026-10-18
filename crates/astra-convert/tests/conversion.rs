use astra_convert::{convert, Options};
use std::{fs, process::Command};

const FIXTURE: &str = include_str!("../../../configs/mihomo-example.yaml");
fn toml(text: &str) -> toml::Value {
    toml::from_str(text).unwrap()
}

#[test]
fn protocols_transports_and_rules() {
    let c = convert(FIXTURE, &Options::default()).unwrap();
    let p = toml(&c.proxy);
    let r = toml(&c.rules);
    assert_eq!(c.nodes, 7);
    assert_eq!(c.rule_count, 6);
    assert_eq!(p["socks5_listen"].as_str(), Some("127.0.0.1:7890"));
    assert_eq!(r["default_action"].as_str(), Some("proxy:vless-reality"));
    assert_eq!(
        p["outbounds"][0]["config"]["tls"]["reality_short_id"].as_str(),
        Some("abcd")
    );
    assert_eq!(
        p["outbounds"][1]["config"]["transport"]["host"].as_str(),
        Some("cdn.example.com")
    );
    assert_eq!(p["outbounds"][5]["server"].as_str(), Some("[::1]:1081"));
    assert_eq!(r["rules"][0]["kind"].as_str(), Some("domain_exact"));
    assert_eq!(r["rules"][5]["value"].as_str(), Some("udp"));
}

#[test]
fn unsupported_nodes_and_policies_do_not_leave_dangling_references() {
    let c = convert("proxies:\n- {name: bad, type: ssr, server: localhost, port: 443}\nrules:\n- DOMAIN,example.com,bad\n- GEOIP,CN,DIRECT\n- MATCH,bad\n", &Options::default()).unwrap();
    assert_eq!(c.nodes, 0);
    assert_eq!(c.rule_count, 0);
    assert_eq!(toml(&c.rules)["default_action"].as_str(), Some("block"));
    assert!(c.warnings.iter().any(|s| s.contains("ssr")));
}

#[test]
fn groups_cycles_and_explicit_selection() {
    let source = "proxy-groups:\n- {name: cycle, type: select, proxies: [cycle]}\n- {name: group, type: url-test, proxies: [cycle, DIRECT, REJECT]}\nrules: [MATCH,group]\n".replace("[MATCH,group]", "['MATCH,group']");
    let c = convert(&source, &Options::default()).unwrap();
    assert_eq!(toml(&c.rules)["default_action"].as_str(), Some("direct"));
    let mut options = Options::default();
    options.selections.insert("group".into(), "REJECT".into());
    let c = convert(&source, &options).unwrap();
    assert_eq!(toml(&c.rules)["default_action"].as_str(), Some("block"));
    options.selections.insert("group".into(), "missing".into());
    assert!(convert(&source, &options).is_err());
}

#[test]
fn yaml_merge_unicode_and_numeric_passwords() {
    let c = convert("base: &b {type: trojan, server: example.com, port: 443, password: 12345}\nproxies:\n- <<: *b\n  name: 香港\nrules: ['MATCH,香港']", &Options::default()).unwrap();
    assert_eq!(
        toml(&c.proxy)["outbounds"][0]["config"]["password"].as_str(),
        Some("12345")
    );
    assert_eq!(
        toml(&c.rules)["default_action"].as_str(),
        Some("proxy:香港")
    );
}

#[test]
fn match_order_and_modes() {
    let source = "rules: ['DST-PORT,443,REJECT', 'MATCH,DIRECT', 'DOMAIN,example.com,REJECT']";
    let c = convert(source, &Options::default()).unwrap();
    assert_eq!(c.rule_count, 1);
    assert_eq!(toml(&c.rules)["default_action"].as_str(), Some("direct"));
    let c = convert(&format!("mode: direct\n{source}"), &Options::default()).unwrap();
    assert_eq!(c.rule_count, 0);
    let c = convert(
        "mode: global\nproxy-groups:\n- {name: GLOBAL, type: select, proxies: [REJECT]}",
        &Options::default(),
    )
    .unwrap();
    assert_eq!(toml(&c.rules)["default_action"].as_str(), Some("block"));
}

#[test]
fn malformed_values_duplicates_and_authentication() {
    for source in [
        "[invalid]",
        "proxies: {}",
        "rules: {}",
        "socks-port: 70000",
        "socks-port: 8080\nport: 8080",
        "proxies: [{name: n}, {name: n}]",
        "mode: unknown",
    ] {
        assert!(convert(source, &Options::default()).is_err(), "{source}");
    }
    let c = convert(
        "allow-lan: true\nauthentication: ['user:password']\nsocks-port: 1080",
        &Options::default(),
    )
    .unwrap();
    assert_eq!(
        toml(&c.proxy)["socks5_listen"].as_str(),
        Some("127.0.0.1:1080")
    );
    let c = convert("rules: ['DST-PORT,80-90,DIRECT', 'IP-CIDR,bad,DIRECT', 'NETWORK,icmp,DIRECT', 'DOMAIN,,DIRECT']", &Options::default()).unwrap();
    assert_eq!(c.rule_count, 0);
}

#[test]
fn transport_options_and_http_upstream() {
    for (network, opts, expected) in [
        ("grpc", "grpc-opts: {grpc-service-name: service}", "grpc"),
        ("xhttp", "xhttp-opts: {path: /x, mode: packet-up}", "xhttp"),
        (
            "httpupgrade",
            "http-upgrade-opts: {path: /u}",
            "http_upgrade",
        ),
        (
            "ws",
            "ws-opts: {path: /u, v2ray-http-upgrade: true}",
            "http_upgrade",
        ),
    ] {
        let source = format!("proxies:\n- name: node\n  type: vless\n  server: example.com\n  port: 443\n  uuid: 00000000-0000-0000-0000-000000000001\n  tls: true\n  network: {network}\n  {opts}\n");
        let c = convert(&source, &Options::default()).unwrap();
        assert_eq!(c.nodes, 1);
        assert_eq!(
            toml(&c.proxy)["outbounds"][0]["config"]["transport"]["kind"].as_str(),
            Some(expected)
        );
    }
    let c = convert("proxies: [{name: upstream, type: http, server: localhost, port: 8080, username: user, password: pass}]", &Options::default()).unwrap();
    assert_eq!(
        toml(&c.proxy)["outbounds"][0]["kind"].as_str(),
        Some("http")
    );
    let c = convert("proxies: [{name: plugin, type: ss, server: localhost, port: 8080, cipher: aes-128-gcm, password: pass, plugin: obfs}]", &Options::default()).unwrap();
    assert_eq!(c.nodes, 0);
}

#[test]
fn cli_never_overwrites_and_check_does_not_write() {
    let dir = std::env::temp_dir().join(format!("astra-convert-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let input = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../configs/mihomo-example.yaml"
    );
    let run = |extra: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_astra-convert"))
            .arg(input)
            .arg("-o")
            .arg(&dir)
            .args(extra)
            .output()
            .unwrap()
    };
    assert!(run(&["--check"]).status.success());
    assert!(!dir.exists());
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("rule.toml"), "original").unwrap();
    assert!(!run(&[]).status.success());
    assert!(!dir.join("proxy.toml").exists());
    assert_eq!(
        fs::read_to_string(dir.join("rule.toml")).unwrap(),
        "original"
    );
    fs::remove_file(dir.join("rule.toml")).unwrap();
    assert!(run(&[]).status.success());
    let original = fs::read(dir.join("proxy.toml")).unwrap();
    assert!(!run(&[]).status.success());
    assert_eq!(fs::read(dir.join("proxy.toml")).unwrap(), original);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(dir.join("proxy.toml"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    fs::remove_dir_all(dir).unwrap();
}
