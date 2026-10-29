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
    let config: astra_dns::Config = document["dns"].clone().try_into().unwrap();
    config.validate().unwrap();
    assert!(config.fake_ip);
    assert_eq!(config.nameserver.len(), 2);
    assert_eq!(config.nameserver_policy["+.example.org"], ["8.8.8.8"]);
    assert!(!converted
        .warnings
        .iter()
        .any(|warning| warning.contains("忽略顶层配置 dns")));
}
#[test]
fn subscription_imports_dns_once_and_preserves_local_changes() {
    let converted = convert_nodes(INPUT, &Options::default(), "test", false).unwrap();
    let mut ownership = Subscription::default();
    let merged = merge("", &converted.proxy, &mut ownership).unwrap();
    let document: toml::Table = toml::from_str(&merged).unwrap();
    assert_eq!(document["dns"]["fake_ip"].as_bool(), Some(true));
    assert!(
        document["dns"].get("listen").is_none(),
        "a subscription must not choose where local DNS listens"
    );
    let changed = merged.replace("fake_ip = true", "fake_ip = false");
    let merged = merge(&changed, &converted.proxy, &mut ownership).unwrap();
    let document: toml::Table = toml::from_str(&merged).unwrap();
    assert_eq!(document["dns"]["fake_ip"].as_bool(), Some(false));
}
