use astra_convert::{
    convert,
    subscription::{convert_nodes, merge, Subscription},
    Options,
};

const INPUT:&str="socks-port: 1080\nproxies:\n  - {name: node, type: socks5, server: 127.0.0.1, port: 9999}\ndns:\n  enable: true\n  listen: 127.0.0.1:1053\n  enhanced-mode: fake-ip\n  fake-ip-range: 198.18.0.1/16\n  fake-ip-range6: fdab::1/64\n  fake-ip-filter: ['+.lan']\n  nameserver: [https://dns.example/dns-query, tls://1.1.1.1]\n  default-nameserver: [1.1.1.1]\n  nameserver-policy:\n    '+.example.org': 8.8.8.8\n";
#[test]
fn mihomo_dns_and_fake_ip_convert_to_valid_native_settings() {
    let converted = convert(INPUT, &Options::default()).unwrap();
    let document: toml::Table = toml::from_str(&converted.proxy).unwrap();
    let config: astra_model::config::dns::Config = document["dns"].clone().try_into().unwrap();
    config.validate().unwrap();
    assert!(config.fake_ip);
    assert_eq!(config.nameserver.len(), 2);
    assert!(
        !config.nameserver_policy.contains_key("+.example.org"),
        "plaintext-only policy is dropped, not downgraded"
    );
    assert!(converted
        .warnings
        .iter()
        .any(|warning| warning.contains("明文上游已忽略") && warning.contains("8.8.8.8")));
    assert!(!converted
        .warnings
        .iter()
        .any(|warning| warning.contains("忽略顶层配置 dns")));
}
#[test]
fn plaintext_only_mihomo_dns_converts_to_encrypted_defaults() {
    let input = "socks-port: 1080\nproxies:\n  - {name: node, type: socks5, server: 127.0.0.1, port: 9999}\ndns:\n  enable: true\n  nameserver: [223.5.5.5, 'udp://119.29.29.29']\n  fallback: ['tcp://8.8.8.8', https://dns.example/dns-query]\n";
    let converted = convert(input, &Options::default()).unwrap();
    let document: toml::Table = toml::from_str(&converted.proxy).unwrap();
    let config: astra_model::config::dns::Config = document["dns"].clone().try_into().unwrap();
    config.validate().unwrap();
    assert!(!config.plaintext_bypass);
    assert_eq!(
        config.nameserver,
        astra_model::config::dns::Config::default().nameserver
    );
    assert!(config
        .fallback
        .contains(&"https://dns.example/dns-query".to_owned()));
    assert!(converted
        .warnings
        .iter()
        .any(|warning| warning.contains("223.5.5.5")));
}
#[test]
fn subscription_never_imports_dns_even_when_importing_groups() {
    let input = format!("{INPUT}proxy-groups: [{{name: remote, type: select, proxies: [node]}}]\n");
    for import_groups in [false, true] {
        let converted = convert_nodes(&input, &Options::default(), "test", import_groups).unwrap();
        let document: toml::Table = toml::from_str(&converted.proxy).unwrap();
        assert_eq!(converted.nodes, 1);
        assert!(document.get("dns").is_none());
        assert_eq!(
            document["proxy_groups"].get("remote").is_some(),
            import_groups
        );

        // Updating from an already converted full document is also restricted.
        let full = convert(&input, &Options::default()).unwrap();
        let mut ownership = Subscription {
            import_groups,
            ..Default::default()
        };
        let merged = merge("", &full.proxy, &mut ownership).unwrap();
        let document: toml::Table = toml::from_str(&merged).unwrap();
        assert!(document.get("dns").is_none());
        let changed = format!("{merged}\n[dns]\nenable=true\nlisten=['127.0.0.1:2053']\nfake_ip=false\nnameserver=['9.9.9.9']\n");
        let before: toml::Table = toml::from_str(&changed).unwrap();
        let merged = merge(&changed, &full.proxy, &mut ownership).unwrap();
        let after: toml::Table = toml::from_str(&merged).unwrap();
        assert_eq!(after["dns"], before["dns"]);
    }
}
