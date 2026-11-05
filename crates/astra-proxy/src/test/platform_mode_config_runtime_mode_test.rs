use super::*;
#[test]
fn fixed_tables_never_enable_runtime_modes_and_defaults_need_no_tables() {
    let mut config:crate::config::ProxyConfig=toml::from_str("socks5_listen='127.0.0.1:1080'\nhttp_listen='127.0.0.1:8080'\n[system_proxy]\n[tun]\nname='fixed-network-parameters'\n").unwrap();
    let state = astra_model::runtime::Overrides::default();
    runtime_modes(&mut config, &state);
    assert!(config.system_proxy.is_none());
    assert!(config.tun.is_none());
    let enabled = astra_model::runtime::Overrides {
        system_proxy: Some(true),
        tun: Some(true),
        ..Default::default()
    };
    runtime_modes(&mut config, &enabled);
    assert!(config.system_proxy.is_some());
    assert_eq!(system_proxy_address(&config).unwrap().port(), 8080);
    let tun = config.tun.as_ref().unwrap();
    assert_eq!(tun.routes, ["0.0.0.0/1", "128.0.0.0/1"]);
    assert!(tun.detects_interface());
    runtime_modes(&mut config, &state);
    assert!(config.system_proxy.is_none());
    assert!(config.tun.is_none());
}
#[test]
fn explicit_fixed_tun_parameters_are_kept_only_while_enabled() {
    let mut config: crate::config::ProxyConfig =
        toml::from_str("[tun]\nname='my-device'\nmtu=1400\n").unwrap();
    runtime_modes(
        &mut config,
        &astra_model::runtime::Overrides {
            tun: Some(true),
            ..Default::default()
        },
    );
    assert_eq!(
        config.tun.as_ref().unwrap().name.as_deref(),
        Some("my-device")
    );
    assert_eq!(config.tun.as_ref().unwrap().mtu, 1400);
    let without_listener: crate::config::ProxyConfig = toml::from_str("standalone=true\n").unwrap();
    assert!(system_proxy_address(&without_listener).is_err());
}
