use astra_convert::{convert, Options};
use std::{fs, process::Command};

const FIXTURE: &str = include_str!("../../../configs/mihomo-example.yaml");
fn toml(text: &str) -> toml::Value {
    toml::from_str(text).unwrap()
}

fn warning_at(warnings: &[String], text: &str, line: usize) {
    assert!(
        warnings
            .iter()
            .any(|s| s.contains(text) && s.starts_with(&format!("[第 {line} 行]"))),
        "missing line {line} for {text}: {warnings:#?}"
    );
}

#[test]
fn geoip_categories_select_dat_and_keep_explicit_database_overrides() {
    let options = Options {
        strict_rules: true,
        ..Default::default()
    };
    let source = "geodata-mode: true\ngeox-url: {geoip: 'https://example.com/geoip.dat', mmdb: 'https://example.com/country.mmdb'}\nrule-providers: {ips: {type: inline, behavior: classical, payload: ['GEOIP,telegram,no-resolve']}}\nrules: ['GEOIP,telegram,DIRECT,no-resolve','RULE-SET,ips,REJECT,no-resolve','MATCH,REJECT']";
    let result = convert(source, &options).unwrap();
    let rules = toml(&result.rules);
    assert_eq!(result.rule_count, 2);
    assert_eq!(
        rules["geoip_url"].as_str(),
        Some("https://example.com/geoip.dat")
    );
    assert_eq!(rules["geoip_database"].as_str(), Some("geoip/geoip.dat"));
    assert_eq!(rules["rules"][0]["value"].as_str(), Some("TELEGRAM"));
    let options = Options {
        geoip_database: Some("custom.mmdb".into()),
        ..options
    };
    let overridden = toml(&convert(source, &options).unwrap().rules);
    assert_eq!(overridden["geoip_database"].as_str(), Some("custom.mmdb"));
    assert!(overridden.get("geoip_url").is_none());
    let source = "rules: ['GEOIP,telegram,DIRECT','MATCH,REJECT']";
    let automatic = toml(&convert(source, &Options::default()).unwrap().rules);
    assert_eq!(
        automatic["geoip_database"].as_str(),
        Some("geoip/geoip.dat")
    );
    assert!(automatic["geoip_url"]
        .as_str()
        .unwrap()
        .ends_with("/geoip.dat"));
    let country = toml(&convert("geox-url: {mmdb: 'https://example.com/country.mmdb'}\nrules: ['GEOIP,CN,DIRECT','MATCH,REJECT']", &Options::default()).unwrap().rules);
    assert_eq!(
        country["geoip_url"].as_str(),
        Some("https://example.com/country.mmdb")
    );
}

#[test]
fn no_resolve_domain_rules_are_kept_in_order_and_in_strict_mode() {
    let source = "rule-providers:\n  sites: {type: inline, behavior: classical, payload: ['DOMAIN,provider.test,no-resolve']}\nrules:\n- DOMAIN,csgo.wmsj.cn,DIRECT,no-resolve\n- DOMAIN-SUFFIX,steamcontent.com,REJECT,no-resolve\n- IP-CIDR,10.0.0.0/8,DIRECT,no-resolve\n- IP-CIDR6,fc00::/7,DIRECT,no-resolve\n- GEOIP,LAN,DIRECT,no-resolve\n- RULE-SET,sites,REJECT,no-resolve\n- MATCH,REJECT\n";
    let options = Options {
        strict_rules: true,
        ..Default::default()
    };
    let flagged = convert(source, &options).unwrap();
    let plain = convert(&source.replace(",no-resolve", ""), &options).unwrap();
    assert_eq!(flagged.rule_count, 6);
    assert!(!flagged.warnings.iter().any(|s| s.contains("跳过规则")));
    let rules = toml(&flagged.rules);
    let plain_rules = toml(&plain.rules);
    assert_eq!(rules["rules"], plain_rules["rules"]);
    assert_eq!(rules["rules"][0]["kind"].as_str(), Some("domain_exact"));
    assert_eq!(rules["rules"][1]["action"].as_str(), Some("block"));
    for suffix in ["resolve", "no-resolve,extra"] {
        let bad = format!("rules: ['DOMAIN,a.test,DIRECT,{suffix}']");
        assert!(convert(&bad, &options).is_err());
        let skipped = convert(&bad, &Options::default()).unwrap();
        warning_at(&skipped.warnings, "跳过规则 1", 1);
    }
}

#[test]
fn skipped_entries_report_original_lines_instead_of_list_positions() {
    let input = "# 跳过节点 bad: this comment is not the node\n\nproxies:\n  # spacer\n  - name: bad\n    type: ssr\n    server: localhost\n    port: 443\nproxy-groups: [\n  {name: cycle, type: select, proxies: [cycle]},\n  {type: select, proxies: [DIRECT]}\n]\nrule-providers:\n  broken:\n    type: inline\n    behavior: domain\n    format: mrs\n    payload: ['example.com']\ndns: {enable: true}\nrules:\n  # UNSUPPORTED,example.com,DIRECT is just a comment\n  - UNSUPPORTED,example.com,DIRECT\n  - UNSUPPORTED,example.com,DIRECT\n  - 123\n  - DOMAIN,example.com,bad\n  - >-\n    UNKNOWN,example.com,DIRECT\n  - MATCH,DIRECT\n";
    let c = convert(input, &Options::default()).unwrap();
    for (message, line) in [
        ("跳过节点 bad", 5),
        ("跳过代理组 cycle", 10),
        ("跳过无名称代理组", 11),
        ("规则集 broken 不可转换", 14),
        ("跳过规则 1", 22),
        ("跳过规则 2", 23),
        ("跳过规则 3", 24),
        ("跳过规则 4", 25),
        ("跳过规则 5", 27),
    ] {
        warning_at(&c.warnings, message, line);
    }
    let windows = format!("\u{feff}{}", input.replace('\n', "\r\n"));
    let c = convert(&windows, &Options::default()).unwrap();
    warning_at(&c.warnings, "跳过节点 bad", 5);
    warning_at(&c.warnings, "跳过规则 5", 27);
}

#[test]
fn aliases_flow_sequences_and_root_merges_keep_source_locations() {
    let input = "bad: &bad {name: aliased, type: ssr, server: localhost, port: 1}\nbad-rule: &bad-rule 'UNKNOWN,x,DIRECT'\nbase: &base\n  rules:\n    - *bad-rule\n    - *bad-rule\n    - MATCH,DIRECT\n<<: *base\nproxies:\n  - *bad\n";
    let c = convert(input, &Options::default()).unwrap();
    warning_at(&c.warnings, "跳过节点 aliased", 10);
    warning_at(&c.warnings, "跳过规则 1", 5);
    warning_at(&c.warnings, "跳过规则 2", 6);
    let input="base: &base\n  rules: ['UNKNOWN,x,DIRECT']\n<<: *base\nrules: [123, 'UNKNOWN,x,DIRECT', 'MATCH,DIRECT']\n";
    let c = convert(input, &Options::default()).unwrap();
    warning_at(&c.warnings, "跳过规则 1", 4);
    warning_at(&c.warnings, "跳过规则 2", 4);
    assert!(!c.warnings.iter().any(|s| s.contains("[第 2 行]")));
    let input="sets: &sets\n  bad: {type: inline, format: mrs, behavior: domain, payload: []}\nrule-providers:\n  <<: *sets\nrules: ['MATCH,DIRECT']\n";
    let c = convert(input, &Options::default()).unwrap();
    warning_at(&c.warnings, "规则集 bad 不可转换", 2);
}

#[test]
fn strict_rule_and_provider_errors_include_source_lines() {
    let options = Options {
        strict_rules: true,
        ..Default::default()
    };
    for (input, line) in [
        ("# comment\n\nrules:\n  - UNKNOWN,x,DIRECT\n", 4),
        ("rules:\n  - DOMAIN,x,absent\n", 2),
        ("rules: [123]\n", 1),
        (
            "rule-providers:\n  bad: {type: inline, behavior: domain, format: mrs}\n",
            2,
        ),
        ("rules:\n  - RULE-SET,absent,DIRECT\n", 2),
    ] {
        let error = convert(input, &options).err().expect("invalid input");
        assert!(
            format!("{error:#}").contains(&format!("[第 {line} 行]")),
            "{error:#}"
        );
    }
}

#[test]
fn cli_diagnostics_group_under_one_absolute_source_path_and_keep_lines() {
    use std::{io::Write, process::Stdio};
    let dir = std::env::temp_dir().join(format!("astra-convert-lines-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("中文 file.yaml");
    let input="# comment\nproxies:\n  - {name: bad, type: ssr, server: localhost, port: 1}\nrules: ['MATCH,DIRECT']\n";
    fs::write(&path, input).unwrap();
    let binary = env!("CARGO_BIN_EXE_astra-convert");
    let result = Command::new(binary)
        .current_dir(&dir)
        .arg(path.file_name().unwrap())
        .arg("--check")
        .output()
        .unwrap();
    assert!(result.status.success());
    let stderr = String::from_utf8(result.stderr).unwrap();
    assert!(
        stderr.starts_with(&format!("── {} ──\n", path.display()))
            && stderr.contains("提示: [第 3 行] 跳过节点 bad"),
        "{stderr}"
    );
    assert_eq!(
        stderr.matches(&path.display().to_string()).count(),
        1,
        "{stderr}"
    );
    assert!(
        stderr
            .lines()
            .filter(|line| line.starts_with("提示:"))
            .count()
            > 1
    );
    let mut child = Command::new(binary)
        .args(["-", "--check"])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let result = child.wait_with_output().unwrap();
    assert!(result.status.success());
    let stderr = String::from_utf8(result.stderr).unwrap();
    assert!(
        stderr.starts_with("── <stdin> ──\n") && stderr.contains("提示: [第 3 行] 跳过节点 bad"),
        "{stderr}"
    );
    assert_eq!(stderr.matches("<stdin>").count(), 1, "{stderr}");
    fs::remove_file(path).unwrap();
    fs::remove_dir(dir).unwrap();
}

#[test]
fn protocols_transports_and_rules() {
    let c = convert(FIXTURE, &Options::default()).unwrap();
    let p = toml(&c.proxy);
    let r = toml(&c.rules);
    assert_eq!(c.nodes, 7);
    assert_eq!(c.rule_count, 7);
    assert_eq!(p["socks5_listen"].as_str(), Some("127.0.0.1:7890"));
    assert_eq!(r["default_action"].as_str(), Some("proxy:PROXY"));
    assert_eq!(p["proxy_groups"]["Proxy"]["mode"].as_str(), Some("select"));
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
    assert_eq!(c.rule_count, 1);
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
        Some("proxy:PROXY")
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
    assert_eq!(
        toml(&c.rules)["default_action"].as_str(),
        Some("proxy:PROXY")
    );
    assert_eq!(
        toml(&c.proxy)["proxy_groups"]["GLOBAL"]["members"][0].as_str(),
        Some("REJECT")
    );
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

#[test]
fn rule_providers_geoip_and_processes_are_translated_in_source_order() {
    let source="rule-providers:\n  local: {type: file, behavior: domain, path: lists/local.yaml}\n  web: {type: http, behavior: ipcidr, format: text, url: 'https://example.com/ip.txt', interval: 600}\n  inline: {type: inline, behavior: classical, payload: ['DOMAIN-KEYWORD,ads','GEOIP,LAN']}\nrules:\n- PROCESS-NAME,browser.exe,DIRECT\n- RULE-SET,local,REJECT\n- RULE-SET,web,DIRECT,no-resolve\n- RULE-SET,inline,REJECT\n- GEOIP,CN,DIRECT,no-resolve\n- MATCH,REJECT\n";
    let mut options = Options::default();
    options.base_dir = std::env::current_dir().unwrap();
    let result = convert(source, &options).unwrap();
    let rules = toml(&result.rules);
    assert_eq!(result.rule_count, 5);
    assert_eq!(rules["rules"][0]["kind"].as_str(), Some("process_name"));
    assert_eq!(rules["rules"][1]["kind"].as_str(), Some("rule_set"));
    assert_eq!(rules["rules"][4]["value"].as_str(), Some("CN"));
    assert!(
        std::path::Path::new(rules["rule_sets"]["local"]["path"].as_str().unwrap()).is_absolute()
    );
    assert_eq!(
        rules["rule_sets"]["web"]["interval"].as_integer(),
        Some(600)
    );
    assert_eq!(
        rules["rule_sets"]["inline"]["payload"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(rules["direct_local"].as_bool(), Some(false));
}
#[test]
fn unsupported_or_missing_referenced_providers_never_leave_a_dangling_rule() {
    for source in ["rules: ['RULE-SET,missing,DIRECT']", "rule-providers: {a: {type: inline, behavior: domain, format: mrs, payload: ['a.test']}}\nrules: ['RULE-SET,a,DIRECT']"] {
        assert!(convert(source,&Options::default()).is_err());
    }
    let mut options = Options::default();
    options.strict_rules = true;
    assert!(convert("rules: ['MATCH,missing-policy']", &options).is_err());
    assert!(convert("rules: ['SRC-PORT,80,DIRECT','MATCH,DIRECT']", &options).is_err());
}

#[test]
fn geosite_rules_and_classical_providers_keep_resources_selectors_and_order() {
    let source="geox-url: {geosite: 'https://example.com/geosite.dat'}\nrule-providers:\n  sites: {type: inline, behavior: classical, payload: ['GEOSITE,google@!cn']}\nrules: ['DOMAIN,first.test,DIRECT','GEOSITE,cn,DIRECT','RULE-SET,sites,REJECT','MATCH,REJECT']";
    let result = convert(source, &Options::default()).unwrap();
    let rules = toml(&result.rules);
    assert_eq!(rules["rules"][1]["kind"].as_str(), Some("geosite"));
    assert_eq!(rules["rules"][1]["value"].as_str(), Some("cn"));
    assert_eq!(
        rules["geosite_url"].as_str(),
        Some("https://example.com/geosite.dat")
    );
    assert_eq!(
        rules["geosite_database"].as_str(),
        Some("geosite/geosite.dat")
    );
    assert_eq!(
        rules["rule_sets"]["sites"]["payload"][0].as_str(),
        Some("GEOSITE,google@!cn")
    );
    let mut options = Options::default();
    options.geosite_database = Some("local.dat".into());
    assert!(toml(&convert(source, &options).unwrap().rules)
        .get("geosite_url")
        .is_none());
    assert!(convert(
        "rules: ['GEOSITE,google@,DIRECT']",
        &Options {
            strict_rules: true,
            ..Default::default()
        }
    )
    .is_err());
}

#[test]
fn cli_defaults_to_yaml_directory_and_stdin_to_working_directory() {
    use std::{
        io::Write,
        process::Stdio,
        time::{SystemTime, UNIX_EPOCH},
    };
    let dir = std::env::temp_dir().join(format!(
        "astra-convert-output-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let source = dir.join("配置");
    let work = dir.join("work");
    let explicit = work.join("explicit");
    fs::create_dir_all(&source).unwrap();
    fs::create_dir_all(&work).unwrap();
    let input = source.join("sample.yaml");
    fs::write(&input, "rules: ['MATCH,DIRECT']\n").unwrap();
    let binary = env!("CARGO_BIN_EXE_astra-convert");
    let clear = |path: &std::path::Path| {
        for name in ["proxy.toml", "rule.toml"] {
            fs::remove_file(path.join(name)).unwrap();
        }
    };
    let result = Command::new(binary)
        .current_dir(&work)
        .arg(&input)
        .arg("--check")
        .output()
        .unwrap();
    assert!(result.status.success());
    assert!(!source.join("proxy.toml").exists());
    for (cwd, input_arg) in [
        (&work, std::path::PathBuf::from("../配置/sample.yaml")),
        (&work, input.clone()),
        (&source, std::path::PathBuf::from("sample.yaml")),
    ] {
        let result = Command::new(binary)
            .current_dir(cwd)
            .arg(input_arg)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(source.join("proxy.toml").exists() && source.join("rule.toml").exists());
        assert!(!work.join("proxy.toml").exists() && !work.join("converted").exists());
        clear(&source);
    }
    let result = Command::new(binary)
        .current_dir(&work)
        .arg(&input)
        .args(["-o", "explicit"])
        .output()
        .unwrap();
    assert!(result.status.success());
    assert!(explicit.join("proxy.toml").exists());
    assert!(!source.join("proxy.toml").exists());
    let mut child = Command::new(binary)
        .current_dir(&work)
        .arg("-")
        .arg("--base-dir")
        .arg(&source)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"rules: ['MATCH,DIRECT']\n")
        .unwrap();
    let result = child.wait_with_output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(work.join("proxy.toml").exists());
    assert!(!source.join("proxy.toml").exists());
    clear(&work);
    clear(&explicit);
    fs::remove_dir(explicit).unwrap();
    fs::remove_file(input).unwrap();
    fs::remove_dir(source).unwrap();
    fs::remove_dir(work).unwrap();
    fs::remove_dir(dir).unwrap();
}

#[test]
fn select_and_url_test_groups_preserve_members_nesting_and_explicit_choice() {
    let source = "proxies: [{name: a, type: socks5, server: localhost, port: 1081}, {name: b, type: http, server: localhost, port: 1082}]\nproxy-groups:\n- {name: speed, type: url-test, proxies: [a,b], url: 'http://example.test/204', interval: 60, tolerance: 25}\n- {name: choose, type: select, proxies: [speed, DIRECT]}\nrules: ['MATCH,choose']";
    let converted = convert(source, &Options::default()).unwrap();
    let proxy = toml(&converted.proxy);
    assert_eq!(
        toml(&converted.rules)["default_action"].as_str(),
        Some("proxy:PROXY")
    );
    let groups: std::collections::BTreeMap<String, astra_ipc::groups::Group> =
        proxy["proxy_groups"].clone().try_into().unwrap();
    astra_ipc::groups::validate(
        &groups,
        &std::collections::HashSet::from(["a".into(), "b".into()]),
    )
    .unwrap();
    assert_eq!(groups["speed"].mode, astra_ipc::groups::Mode::Auto);
    assert_eq!(groups["speed"].interval, 60);
    assert_eq!(groups["speed"].tolerance_ms, 25);
    assert_eq!(groups["choose"].members, ["speed", "DIRECT"]);
    let mut options = Options::default();
    options.selections.insert("speed".into(), "b".into());
    let converted = convert(source, &options).unwrap();
    assert_eq!(
        toml(&converted.proxy)["proxy_groups"]["speed"]["mode"].as_str(),
        Some("select")
    );
    assert_eq!(
        toml(&converted.proxy)["proxy_groups"]["speed"]["selected"].as_str(),
        Some("b")
    );
}

#[test]
fn group_limits_apply_to_the_whole_converted_configuration() {
    let mut source = String::from("proxy-groups:\n");
    for i in 0..129 {
        source.push_str(&format!(
            "- {{name: group{i}, type: select, proxies: [DIRECT]}}\n"
        ));
    }
    assert!(convert(&source, &Options::default()).is_err());
}

#[test]
fn every_proxy_action_uses_default_proxy_while_direct_block_and_order_remain() {
    let source="proxies: [{name: a, type: socks5, server: localhost, port: 1081}, {name: b, type: socks5, server: localhost, port: 1082}]\nproxy-groups: [{name: speed, type: url-test, proxies: [a,b]}]\nrule-providers: {domains: {type: inline, behavior: domain, payload: ['+.example.test']}}\nrules: ['DOMAIN,a.test,a','DOMAIN,b.test,speed','RULE-SET,domains,b','DOMAIN,local.test,DIRECT','DOMAIN,blocked.test,REJECT','MATCH,speed']";
    let converted = convert(source, &Options::default()).unwrap();
    let rules = toml(&converted.rules);
    let proxy = toml(&converted.proxy);
    assert_eq!(rules["default_action"].as_str(), Some("proxy:PROXY"));
    let entries = rules["rules"].as_array().unwrap();
    assert_eq!(entries.len(), 5);
    for rule in &entries[..3] {
        assert_eq!(rule["action"].as_str(), Some("proxy:PROXY"));
    }
    assert_eq!(entries[3]["action"].as_str(), Some("direct"));
    assert_eq!(entries[4]["action"].as_str(), Some("block"));
    assert_eq!(entries[2]["kind"].as_str(), Some("rule_set"));
    assert_eq!(
        proxy["proxy_groups"]["PROXY"]["include_all"].as_bool(),
        Some(true)
    );
    assert!(proxy["proxy_groups"]["PROXY"]["members"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(
        proxy["proxy_groups"]["speed"]["mode"].as_str(),
        Some("auto")
    );
}
#[test]
fn mihomo_dynamic_node_filters_survive_conversion() {
    let converted=convert("proxies: [{name: JP-a, type: socks5, server: localhost, port: 1081}]\nproxy-groups: [{name: region, type: select, include-all-proxies: true, filter: '^JP-', exclude-filter: 'expired'}]\nrules: ['MATCH,region']",&Options::default()).unwrap();
    let groups: std::collections::BTreeMap<String, astra_ipc::groups::Group> =
        toml(&converted.proxy)["proxy_groups"]
            .clone()
            .try_into()
            .unwrap();
    let expanded =
        astra_ipc::groups::expand(&groups, &std::collections::HashSet::from(["JP-a".into()]))
            .unwrap();
    assert_eq!(expanded["region"].members, ["JP-a"]);
}
