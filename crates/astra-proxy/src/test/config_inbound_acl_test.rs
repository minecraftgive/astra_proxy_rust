use super::*;
#[test]
fn lan_acl_and_authentication_exceptions_have_independent_precedence() {
    let app = crate::App::from_text(std::path::Path::new("acl.toml"), "standalone=true\nstandalone_outbound='only'\nmixed_listen='0.0.0.0:7890'\nallow_lan=false\nauthentication=['alice:correct']\nlan_allowed_ips=['127.0.0.0/8','192.0.2.0/24']\nlan_disallowed_ips=['192.0.2.10/32']\nskip_auth_prefixes=['127.0.0.1/32']\n[proxy_groups.only]\nmembers=['DIRECT']\n".into()).unwrap();
    assert!(app.inbound_allowed("127.0.0.1".parse().unwrap()));
    assert!(!app.inbound_allowed("192.0.2.11".parse().unwrap()));
    let mut overrides = (*app.overrides.load_full()).clone();
    overrides.allow_lan = Some(true);
    app.overrides.store(std::sync::Arc::new(overrides));
    assert!(app.inbound_allowed("192.0.2.11".parse().unwrap()));
    assert!(!app.inbound_allowed("192.0.2.10".parse().unwrap()));
    assert!(!app.inbound_allowed("198.51.100.1".parse().unwrap()));
    assert!(app
        .authentication(Some("127.0.0.1".parse().unwrap()))
        .is_empty());
    assert_eq!(
        app.authentication(Some("192.0.2.11".parse().unwrap()))
            .len(),
        1
    );
    assert_eq!(app.authentication(None).len(), 1);
    let bad: ProxyConfig = toml::from_str("authentication=['missing-separator']").unwrap();
    assert!(bad.validate().is_err());
}
#[test]
fn network_recovery_is_an_opt_in_experiment() {
    let default: ProxyConfig = toml::from_str("socks5_listen='127.0.0.1:1080'").unwrap();
    assert!(!default.experimental.network_recovery);
    assert!(!toml::to_string(&default).unwrap().contains("experimental"));
    let enabled: ProxyConfig = toml::from_str(
        "[experimental]
network_recovery=true",
    )
    .unwrap();
    assert!(enabled.experimental.network_recovery);
    assert!(toml::from_str::<ProxyConfig>(
        "[experimental]
unknown=true"
    )
    .is_err());
}
