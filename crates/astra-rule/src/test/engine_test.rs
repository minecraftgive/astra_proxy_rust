use super::*;
use astra_ipc::API_VERSION;
use astra_model::Inbound;
use astra_model::Target;
fn decision(rules: &str, host: &str, port: u16) -> RouteAction {
    let config: RuleConfig = toml::from_str(&format!(
        "socket = '/tmp/test.sock'\ndefault_action = 'direct'\n{rules}"
    ))
    .unwrap();
    let engine = RuleEngine::compile(&config).unwrap();
    engine.decide(&RouteRequest {
        destination_ip: None,
        api_version: API_VERSION,
        id: 1,
        target: Target {
            host: host.to_string(),
            port,
        },
        network: Network::Tcp,
        inbound: Inbound::Socks5,
        process: None,
    })
}
fn request(host: &str, process: Option<astra_model::ProcessInfo>) -> RouteRequest {
    RouteRequest {
        destination_ip: None,
        api_version: API_VERSION,
        id: 1,
        target: Target {
            host: host.into(),
            port: 443,
        },
        network: Network::Tcp,
        inbound: Inbound::Tun,
        process,
    }
}
fn compile(settings: &str) -> RuleEngine {
    RuleEngine::compile(
        &toml::from_str::<RuleConfig>(&format!(
            "socket='/tmp/test.sock'\ndefault_action='proxy:edge'\n{settings}"
        ))
        .unwrap(),
    )
    .unwrap()
}
#[test]
fn local_default_covers_private_loopback_link_local_and_mapped_ipv6() {
    let engine = compile("");
    for ip in [
        "10.0.0.1",
        "10.255.255.255",
        "172.16.0.1",
        "172.31.255.255",
        "192.168.0.1",
        "127.0.0.1",
        "169.254.1.2",
        "::1",
        "fc00::1",
        "fdff::1",
        "fe80::1",
        "febf::1",
        "::ffff:192.168.1.1",
    ] {
        assert_eq!(
            engine.decide(&request(ip, None)),
            RouteAction::Direct,
            "{ip}"
        );
    }
    for ip in [
        "8.8.8.8",
        "172.15.255.255",
        "172.32.0.1",
        "192.169.0.1",
        "2001:4860:4860::8888",
        "fec0::1",
        "::ffff:8.8.8.8",
        "example.com",
    ] {
        assert_eq!(
            engine.decide(&request(ip, None)),
            RouteAction::Proxy("edge".into()),
            "{ip}"
        );
    }
    assert_eq!(
        compile("direct_local=false").decide(&request("192.168.1.1", None)),
        RouteAction::Proxy("edge".into())
    );
    assert_eq!(
        compile("[[rules]]\nkind='ip_cidr'\nvalue='10.0.0.0/8'\naction='block'")
            .decide(&request("10.1.2.3", None)),
        RouteAction::Block
    );
}
#[test]
fn process_rules_use_observed_identity_and_preserve_first_match_order() {
    let process = astra_model::ProcessInfo {
        pid: 42,
        name: "Browser.EXE".into(),
        path: r"C:\Apps\Browser.exe".into(),
    };
    let engine = compile("[[rules]]\nkind='process_name'\nvalue='browser.exe'\naction='block'\n[[rules]]\nkind='domain_suffix'\nvalue='example.com'\naction='direct'");
    assert_eq!(
        engine.decide(&request("example.com", Some(process.clone()))),
        RouteAction::Block
    );
    assert_eq!(
        engine.decide(&request("example.com", None)),
        RouteAction::Direct
    );
    let engine = compile("[[rules]]\nkind='domain_suffix'\nvalue='example.com'\naction='direct'\n[[rules]]\nkind='process_name'\nvalue='browser.exe'\naction='block'");
    assert_eq!(
        engine.decide(&request("example.com", Some(process.clone()))),
        RouteAction::Direct
    );
    let engine =
        compile("[[rules]]\nkind='process_path'\nvalue='c:/apps/browser.exe'\naction='block'");
    assert_eq!(
        engine.decide(&request("10.1.2.3", Some(process))),
        RouteAction::Block
    );
    assert_eq!(
        engine.decide(&request("8.8.8.8", None)),
        RouteAction::Proxy("edge".into())
    );
    let unix = astra_model::ProcessInfo {
        pid: 7,
        name: "browser".into(),
        path: "/usr/bin/Browser".into(),
    };
    assert_eq!(
        compile("[[rules]]\nkind='process_path'\nvalue='/usr/bin/browser'\naction='block'")
            .decide(&request("8.8.8.8", Some(unix))),
        RouteAction::Proxy("edge".into())
    );
}
#[test]
fn invalid_process_rules_are_rejected() {
    for (kind, value) in [
        ("process_name", ""),
        ("process_name", "a/b"),
        ("process_path", "browser.exe"),
        ("process_path", "C:browser.exe"),
    ] {
        let config: RuleConfig = toml::from_str(&format!("socket='/tmp/a'\ndefault_action='direct'\n[[rules]]\nkind='{kind}'\nvalue='{value}'\naction='block'" )).unwrap();
        assert!(RuleEngine::compile(&config).is_err(), "{kind}: {value}");
    }
}
#[test]
fn geoip_dat_categories_keep_priority_provenance_and_no_dns() {
    let dir = std::env::temp_dir().join(format!("astra-geoip-dat-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("geoip.dat");
    std::fs::write(&path, include_bytes!("data/geoip.dat")).unwrap();
    let config: RuleConfig = toml::from_str("socket='/tmp/a'\ndirect_local=false\ndefault_action='direct'\ngeoip_database='geoip.dat'\n[[rules]]\nkind='ip_cidr'\nvalue='127.0.0.1/32'\naction='proxy:first'\n[[rules]]\nkind='rule_set'\nvalue='ips'\naction='block'\n[[rules]]\nkind='ip_cidr'\nvalue='127.0.0.0/8'\naction='direct'\n[rule_sets.ips]\nbehavior='classical'\npayload=['GEOIP,TeLeGrAm,no-resolve']").unwrap();
    let engine = RuleEngine::compile_at(&config, &dir).unwrap();
    assert_eq!(
        engine.decide(&request("127.0.0.1", None)),
        RouteAction::Proxy("first".into())
    );
    for host in ["127.0.0.2", "::ffff:127.0.0.2", "2001:db8::1"] {
        let decision = engine.decision(&request(host, None));
        assert_eq!(decision.action, RouteAction::Block);
        assert!(decision
            .path
            .to_string()
            .starts_with("rules[2] -> rule_set"));
        let mut restored = request("restored.example", None);
        restored.destination_ip = Some(host.parse().unwrap());
        assert_eq!(engine.decide(&restored), RouteAction::Block);
    }
    for host in ["localhost", "example.com", "192.0.2.1", "2001:db9::1"] {
        assert_eq!(engine.decide(&request(host, None)), RouteAction::Direct);
    }
    let missing: RuleConfig = toml::from_str("socket='/tmp/a'\ndefault_action='direct'\ngeoip_database='missing.dat'\n[[rules]]\nkind='geoip'\nvalue='missing'\naction='block'").unwrap();
    let missing_engine = RuleEngine::compile_at(&missing, &dir).unwrap();
    assert_eq!(
        missing_engine.decide(&request("203.0.113.1", None)),
        RouteAction::Direct
    );
    std::fs::write(&path, b"invalid dat").unwrap();
    assert!(RuleEngine::compile_at(&config, &dir).is_err());
    std::fs::remove_file(path).unwrap();
    std::fs::remove_dir(dir).unwrap();
}
#[test]
fn geoip_country_flat_and_list_databases_match_and_preserve_priority() {
    let dir = std::env::temp_dir().join(format!("astra-geoip-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for data in [
        include_bytes!("data/country.mmdb").as_slice(),
        include_bytes!("data/flat.mmdb"),
        include_bytes!("data/list.mmdb"),
    ] {
        std::fs::write(dir.join("test.mmdb"), data).unwrap();
        let config:RuleConfig=toml::from_str("socket='/tmp/a'\ndefault_action='direct'\ngeoip_database='test.mmdb'\n[[rules]]\nkind='geoip'\nvalue='CN'\naction='block'\n[[rules]]\nkind='geoip'\nvalue='US'\naction='proxy:edge'").unwrap();
        let engine = RuleEngine::compile_at(&config, &dir).unwrap();
        assert_eq!(
            engine.decide(&request("127.0.0.1", None)),
            RouteAction::Block
        );
        assert_eq!(
            engine.decide(&request("::ffff:127.0.0.1", None)),
            RouteAction::Block
        );
        assert_eq!(
            engine.decide(&request("200.1.2.3", None)),
            RouteAction::Proxy("edge".into())
        );
        assert_eq!(
            engine.decide(&request("example.com", None)),
            RouteAction::Direct
        );
    }
    std::fs::remove_file(dir.join("test.mmdb")).unwrap();
    std::fs::remove_dir(dir).unwrap();
    assert_eq!(
        compile("[[rules]]\nkind='geoip'\nvalue='LAN'\naction='block'")
            .decide(&request("fd00::1", None)),
        RouteAction::Block
    );
}
#[test]
fn rule_set_expansion_keeps_reference_position_and_relative_paths() {
    let dir = std::env::temp_dir().join(format!("astra-rule-set-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("domains.yaml"),
        "payload: ['+.example.com', '.sub.test', '*.*.test']",
    )
    .unwrap();
    let config:RuleConfig=toml::from_str("socket='/tmp/a'\ndefault_action='direct'\n[[rules]]\nkind='domain_exact'\nvalue='keep.example.com'\naction='direct'\n[[rules]]\nkind='rule_set'\nvalue='domains'\naction='block'\n[[rules]]\nkind='domain_exact'\nvalue='example.com'\naction='proxy:edge'\n[rule_sets.domains]\nbehavior='domain'\npath='domains.yaml'").unwrap();
    let engine = RuleEngine::compile_at(&config, &dir).unwrap();
    assert!(engine
        .decision(&request("a.example.com", None))
        .path
        .to_string()
        .starts_with("rules[2] -> rule_set(\"domains\")"));
    assert_eq!(
        engine.decide(&request("keep.example.com", None)),
        RouteAction::Direct
    );
    for domain in ["example.com", "a.example.com", "x.sub.test", "a.b.test"] {
        assert_eq!(
            engine.decide(&request(domain, None)),
            RouteAction::Block,
            "{domain}"
        );
    }
    for domain in ["notexample.com", "sub.test", "a.test", "a.b.c.test"] {
        assert_eq!(
            engine.decide(&request(domain, None)),
            RouteAction::Direct,
            "{domain}"
        );
    }
    std::fs::remove_file(dir.join("domains.yaml")).unwrap();
    std::fs::remove_dir(dir).unwrap();
}
#[test]
fn disabled_rule_sets_are_skipped_without_loading_resources() {
    let mut config: RuleConfig = toml::from_str("socket='/tmp/a'\ndefault_action='proxy:fallback'\ndirect_local=false\n[[rules]]\nkind='rule_set'\nvalue='ads'\naction='block'\n[[rules]]\nkind='domain_exact'\nvalue='next.test'\naction='direct'\n[rule_sets.ads]\nenabled=false\npath='missing.yaml'").unwrap();
    let engine = RuleEngine::compile(&config).unwrap();
    assert_eq!(
        engine.decide(&request("anything.test", None)),
        RouteAction::Proxy("fallback".into())
    );
    let decision = engine.decision(&request("next.test", None));
    assert_eq!(decision.action, RouteAction::Direct);
    assert!(decision.path.to_string().starts_with("rules[2]"));
    config.rule_sets.get_mut("ads").unwrap().enabled = true;
    let engine = RuleEngine::compile(&config).unwrap();
    assert_eq!(
        engine.decide(&request("anything.test", None)),
        RouteAction::Proxy("fallback".into())
    );
}
#[test]
fn native_inline_yaml_and_text_providers_have_identical_decisions() {
    let dir = std::env::temp_dir().join(format!("astra-rule-matrix-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cases = [
        (
            "domain_exact",
            "example.test",
            "DOMAIN,example.test",
            "example.test",
            "other.test",
        ),
        (
            "domain_suffix",
            "example.test",
            "DOMAIN-SUFFIX,example.test",
            "a.example.test",
            "notexample.test",
        ),
        (
            "domain_keyword",
            "needle",
            "DOMAIN-KEYWORD,needle",
            "a.needle.test",
            "other.test",
        ),
        (
            "domain_regex",
            r"^rx[0-9]+\.test$",
            r"DOMAIN-REGEX,^rx[0-9]+\.test$",
            "rx42.test",
            "rx.test",
        ),
        (
            "ip_cidr",
            "198.51.100.0/24",
            "IP-CIDR,198.51.100.0/24",
            "198.51.100.1",
            "203.0.113.1",
        ),
        (
            "ip_cidr",
            "2001:db8::/32",
            "IP-CIDR6,2001:db8::/32",
            "2001:db8::1",
            "2001:db9::1",
        ),
        ("geoip", "LAN", "GEOIP,LAN", "192.168.1.1", "203.0.113.1"),
        ("port", "53", "DST-PORT,53", "example.test", "example.test"),
        (
            "network",
            "udp",
            "NETWORK,udp",
            "example.test",
            "example.test",
        ),
        (
            "process_name",
            "browser.exe",
            "PROCESS-NAME,browser.exe",
            "example.test",
            "example.test",
        ),
        (
            "process_path",
            "C:/Program Files/Browser/browser.exe",
            "PROCESS-PATH,C:/Program Files/Browser/browser.exe",
            "example.test",
            "example.test",
        ),
    ];
    for (kind, value, classical, matched, missed) in cases {
        let mut hit = request(matched, None);
        let miss = request(missed, None);
        if kind == "port" {
            hit.target.port = 53;
        }
        if kind == "network" {
            hit.network = Network::Udp;
        }
        if kind.starts_with("process_") {
            hit.process = Some(astra_model::ProcessInfo {
                pid: 1,
                name: "Browser.EXE".into(),
                path: r"c:\Program Files\Browser\browser.exe".into(),
            });
        }
        for mode in ["native", "inline", "yaml", "text"] {
            let mut config: RuleConfig = toml::from_str(
                "socket='unused'\ndirect_local=false\ndefault_action='proxy:fallback'",
            )
            .unwrap();
            if mode == "native" {
                config.rules.push(RuleSpec {
                    kind: kind.into(),
                    value: value.into(),
                    action: "block".into(),
                });
            } else {
                let entry = format!("{classical},no-resolve");
                let mut source = astra_rule_data::RuleSet::default();
                if mode == "inline" {
                    source.payload.push(entry);
                } else {
                    source.format = mode.into();
                    source.path = Some(format!("provider.{mode}"));
                    let content = if mode == "yaml" {
                        format!("payload:\n  - '{entry}'\n")
                    } else {
                        format!("# original line 2 follows\n{entry}\n")
                    };
                    std::fs::write(dir.join(source.path.as_ref().unwrap()), content).unwrap();
                }
                config.rule_sets.insert("provider".into(), source);
                // Reusing a provider must keep each reference's action and order.
                for action in ["block", "direct"] {
                    config.rules.push(RuleSpec {
                        kind: "rule_set".into(),
                        value: "provider".into(),
                        action: action.into(),
                    });
                }
            }
            let engine = RuleEngine::compile_at(&config, &dir).unwrap();
            let decision = engine.decision(&hit);
            assert_eq!(decision.action, RouteAction::Block, "{kind}/{mode}");
            assert!(
                decision.path.to_string().starts_with("rules[1]"),
                "{kind}/{mode}"
            );
            assert!(
                engine.matched_condition(&decision.path).is_some(),
                "{kind}/{mode}"
            );
            assert_eq!(
                engine.decide(&miss),
                RouteAction::Proxy("fallback".into()),
                "{kind}/{mode}"
            );
        }
    }
    for name in ["provider.yaml", "provider.text"] {
        std::fs::remove_file(dir.join(name)).unwrap();
    }
    std::fs::remove_dir(dir).unwrap();
}
#[test]
fn unused_definitions_validate_without_fetching_and_disabled_payload_can_be_repaired() {
    let mut config: RuleConfig = toml::from_str("socket='unused'\ndefault_action='direct'\n[rule_sets.unused]\npath='missing.yaml'\nurl='http://127.0.0.1:1/not-requested'").unwrap();
    assert!(RuleEngine::compile(&config).is_ok());
    config.rule_sets.get_mut("unused").unwrap().path = Some("  ".into());
    let error = format!("{:#}", RuleEngine::compile(&config).err().unwrap());
    assert!(error.contains("unused") && error.contains("nonempty"));
    let source = config.rule_sets.get_mut("unused").unwrap();
    source.path = None;
    source.url = None;
    source.payload = vec!["DOMAIN-REGEX,[".into()];
    assert!(RuleEngine::compile(&config).is_err());
    config.rule_sets.get_mut("unused").unwrap().enabled = false;
    assert!(RuleEngine::compile(&config).is_ok());
}
#[test]
fn missing_cyclic_and_invalid_sets_reject_the_candidate() {
    for extra in [
        "",
        "[rule_sets.a]\npayload=['RULE-SET,a']",
        "[rule_sets.a]\npayload=['SRC-PORT,80']",
        "[rule_sets.a]\nformat='mrs'\npayload=['DOMAIN,a.test']",
    ] {
        let config:RuleConfig=toml::from_str(&format!("socket='/tmp/a'\ndefault_action='direct'\n[[rules]]\nkind='rule_set'\nvalue='a'\naction='block'\n{extra}")).unwrap();
        assert!(RuleEngine::compile(&config).is_err(), "{extra}");
    }
}
#[test]
fn overlapping_keywords_and_pattern_groups_keep_original_precedence() {
    let engine = compile("[[rules]]\nkind='domain_keyword'\nvalue='bc'\naction='block'\n[[rules]]\nkind='domain_keyword'\nvalue='abc'\naction='direct'\n[[rules]]\nkind='domain_pattern'\nvalue='*.test'\naction='direct'\n[[rules]]\nkind='domain_keyword'\nvalue='bc'\naction='proxy:later'");
    assert_eq!(
        engine.decide(&request("ABC.test", None)),
        RouteAction::Block
    );
    assert_eq!(engine.decide(&request("x.test", None)), RouteAction::Direct);
    let patterns = compile("[[rules]]\nkind='domain_pattern'\nvalue='*.test'\naction='block'\n[[rules]]\nkind='domain_pattern'\nvalue='*.*.test'\naction='direct'");
    let req = request("a.b.test", None);
    let decision = patterns.decision(&req);
    assert_eq!(decision.action, RouteAction::Direct);
    assert_eq!(
        patterns.matched_condition(&decision.path),
        Some(("domain_pattern".into(), "*.*.test".into()))
    );
    assert_eq!(
        patterns.decide(&request("a.test", None)),
        RouteAction::Block
    );
    assert_eq!(
        patterns.decide(&request("test", None)),
        RouteAction::Proxy("edge".into())
    );
}

#[test]
fn process_normalization_preserves_unicode_and_posix_case() {
    assert_eq!(lowercase("BROWSER.exe"), "browser.exe");
    assert_eq!(lowercase("İΣ.EXE"), "İΣ.EXE".to_lowercase());
    assert!(matches!(lowercase("browser.exe"), Cow::Borrowed(_)));
    assert!(matches!(
        process_path("/usr/Browser"),
        Cow::Borrowed("/usr/Browser")
    ));
    assert_eq!(
        process_path("C:/Apps/İ.exe"),
        "c:/apps/İ.exe".to_lowercase()
    );
}

#[test]
fn regex_set_keeps_first_matching_rule_and_case_insensitivity() {
    let engine = compile(
        "[[rules]]
kind='domain_suffix'
value='early.test'
action='direct'
             [[rules]]
kind='domain_regex'
value='^a.*\\.test$'
action='block'
             [[rules]]
kind='domain_regex'
value='^ab'
action='proxy:second'
",
    );
    assert!(engine.regex_set.is_some());
    assert_eq!(
        engine.decide(&request("ABC.test", None)),
        RouteAction::Block
    );
    assert_eq!(
        engine.decide(&request("abc.other", None)),
        RouteAction::Proxy("second".into())
    );
    assert_eq!(
        engine.decide(&request("a.early.test", None)),
        RouteAction::Direct
    );
    assert_eq!(
        engine.decide(&request("zzz", None)),
        RouteAction::Proxy("edge".into())
    );
}
#[test]
fn geosite_dat_matches_types_filters_and_source_order() {
    let dir = std::env::temp_dir().join(format!("astra-geosite-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("geosite.dat"), include_bytes!("data/geosite.dat")).unwrap();
    let config_for = |selector: &str| {
        toml::from_str::<RuleConfig>(&format!("socket='/tmp/a'\ndefault_action='proxy:edge'\ngeosite_database='geosite.dat'\n[[rules]]\nkind='domain_exact'\nvalue='keep.suffix.test'\naction='direct'\n[[rules]]\nkind='geosite'\nvalue='{selector}'\naction='block'\n[[rules]]\nkind='domain_exact'\nvalue='exact.test'\naction='proxy:later'")).unwrap()
    };
    let engine = RuleEngine::compile_at(&config_for("TEST"), &dir).unwrap();
    for host in [
        "EXACT.TEST.",
        "suffix.test",
        "a.suffix.test",
        "a.keyword.test",
        "rx42.test",
    ] {
        assert_eq!(
            engine.decide(&request(host, None)),
            RouteAction::Block,
            "{host}"
        );
    }
    for host in [
        "notsuffix.test",
        "a.exact.test",
        "rx.test",
        "rx42.test.other",
    ] {
        assert!(
            matches!(engine.decide(&request(host, None)), RouteAction::Proxy(_)),
            "{host}"
        );
    }
    assert_eq!(
        engine.decide(&request("keep.suffix.test", None)),
        RouteAction::Direct
    );
    let engine = RuleEngine::compile_at(&config_for("test@cn"), &dir).unwrap();
    assert_eq!(
        engine.decide(&request("suffix.test", None)),
        RouteAction::Block
    );
    assert_eq!(
        engine.decide(&request("exact.test", None)),
        RouteAction::Proxy("later".into())
    );
    let engine = RuleEngine::compile_at(&config_for("test@!cn"), &dir).unwrap();
    assert_eq!(
        engine.decide(&request("exact.test", None)),
        RouteAction::Block
    );
    assert_eq!(
        engine.decide(&request("suffix.test", None)),
        RouteAction::Proxy("edge".into())
    );
    let engine = RuleEngine::compile_at(&config_for("test@cn@fast"), &dir).unwrap();
    assert_eq!(
        engine.decide(&request("rx42.test", None)),
        RouteAction::Block
    );
    let missing = RuleEngine::compile_at(&config_for("missing"), &dir).unwrap();
    assert_eq!(
        missing.decide(&request("unrelated.example", None)),
        RouteAction::Proxy("edge".into())
    );
    std::fs::write(dir.join("geosite.dat"), b"invalid").unwrap();
    assert!(RuleEngine::compile_at(&config_for("test"), &dir).is_err());
    std::fs::remove_file(dir.join("geosite.dat")).unwrap();
    std::fs::remove_dir(dir).unwrap();
}
#[test]
fn native_domains_reject_invalid_labels_and_wildcard_ambiguity() {
    for kind in [
        "domain_exact",
        "domain_suffix",
        "domain_subdomain",
        "domain_pattern",
    ] {
        for value in ["bad..test", "bad\ttest", "bad/test", "bad+test"] {
            let mut config: RuleConfig =
                toml::from_str("socket='unused'\ndefault_action='direct'").unwrap();
            config.rules.push(RuleSpec {
                kind: kind.into(),
                value: value.into(),
                action: "block".into(),
            });
            assert!(RuleEngine::compile(&config).is_err(), "{kind}: {value:?}");
        }
    }
    for kind in ["domain_exact", "domain_suffix", "domain_subdomain"] {
        let mut config: RuleConfig =
            toml::from_str("socket='unused'\ndefault_action='direct'").unwrap();
        config.rules.push(RuleSpec {
            kind: kind.into(),
            value: "*.test".into(),
            action: "block".into(),
        });
        assert!(RuleEngine::compile(&config).is_err(), "{kind}");
    }
}
#[test]
fn domain_suffix_respects_label_boundaries() {
    let spec = "[[rules]]\nkind='domain_suffix'\nvalue='example.com'\naction='block'";
    assert_eq!(decision(spec, "a.EXAMPLE.com", 443), RouteAction::Block);
    assert_eq!(decision(spec, "notexample.com", 443), RouteAction::Direct);
}
#[test]
fn earliest_rule_wins_across_indices() {
    let spec = "[[rules]]\nkind='port'\nvalue='443'\naction='block'\n[[rules]]\nkind='domain_suffix'\nvalue='example.com'\naction='proxy:edge'";
    assert_eq!(decision(spec, "example.com", 443), RouteAction::Block);
    assert_eq!(
        decision(spec, "example.com", 80),
        RouteAction::Proxy("edge".to_owned())
    );
}
#[test]
fn restored_domain_and_observed_ip_share_original_rule_order() {
    let domain = "[[rules]]\nkind='domain_suffix'\nvalue='qq.com'\naction='direct'\n";
    let ip = "[[rules]]\nkind='ip_cidr'\nvalue='203.0.113.0/24'\naction='block'\n";
    let mut req = request("www.qq.com", None);
    req.destination_ip = Some("203.0.113.8".parse().unwrap());
    assert_eq!(
        compile(&format!("{domain}{ip}")).decide(&req),
        RouteAction::Direct
    );
    assert_eq!(
        compile(&format!("{ip}{domain}")).decide(&req),
        RouteAction::Block
    );
    req.target.host = "unmatched.test".into();
    assert_eq!(compile(ip).decide(&req), RouteAction::Block);
    req.destination_ip = Some("192.168.1.1".parse().unwrap());
    assert_eq!(compile("").decide(&req), RouteAction::Direct);
}
#[test]
#[ignore = "manual benchmark; run cargo test -p astra-rule --release -- --ignored --nocapture"]
fn benchmark_mixed_rules() {
    use std::time::Instant;
    let config: RuleConfig = toml::from_str("socket = '/tmp/test.sock'\ndefault_action = 'direct'\n[[rules]]\nkind='domain_suffix'\nvalue='example.com'\naction='block'\n").unwrap();
    let engine = RuleEngine::compile(&config).unwrap();
    let req = RouteRequest {
        destination_ip: None,
        api_version: API_VERSION,
        id: 1,
        target: Target {
            host: "www.example.com".to_owned(),
            port: 443,
        },
        network: Network::Tcp,
        inbound: Inbound::Socks5,
        process: None,
    };
    let start = Instant::now();
    for _ in 0..1_000_000 {
        std::hint::black_box(engine.decide(std::hint::black_box(&req)));
    }
    eprintln!(
        "1M rule queries elapsed: {:?} (not a cross-platform benchmark)",
        start.elapsed()
    );
}

#[test]
fn cidr_v4_and_v6() {
    let spec = "[[rules]]\nkind='ip_cidr'\nvalue='10.0.0.0/8'\naction='block'\n[[rules]]\nkind='ip_cidr'\nvalue='2001:db8::/32'\naction='block'";
    assert_eq!(decision(spec, "10.99.0.1", 80), RouteAction::Block);
    assert_eq!(decision(spec, "2001:db8::1", 80), RouteAction::Block);
    assert_eq!(decision(spec, "11.0.0.1", 80), RouteAction::Direct);
}
