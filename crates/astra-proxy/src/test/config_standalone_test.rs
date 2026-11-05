use super::*;
fn config(extra: &str, nodes: usize) -> ProxyConfig {
    let mut text = format!("socks5_listen='127.0.0.1:1080'\n{extra}\n");
    for index in 0..nodes {
        text.push_str(&format!(
            "[[outbounds]]\nkind='socks5'\ntag='node{index}'\nserver='127.0.0.1:1081'\n"
        ));
    }
    toml::from_str(&text).unwrap()
}
#[test]
fn standalone_selection_is_explicit_and_never_direct() {
    let single = config("standalone=true", 1);
    single.validate().unwrap();
    assert_eq!(single.standalone_tag().unwrap().as_deref(), Some("node0"));
    let multiple = config("standalone=true\nstandalone_outbound='node1'", 2);
    multiple.validate().unwrap();
    assert_eq!(multiple.standalone_tag().unwrap().as_deref(), Some("node1"));
    for invalid in [
        config("standalone=true", 0),
        config("standalone=true", 2),
        config("standalone=true\nstandalone_outbound='missing'", 1),
        config(
            "rule_socket='/tmp/rules.sock'\nstandalone_outbound='node0'",
            1,
        ),
        config("", 1),
    ] {
        assert!(invalid.validate().is_err());
    }
    let normal = config("rule_socket='/tmp/rules.sock'", 0);
    normal.validate().unwrap();
    assert_eq!(normal.standalone_tag().unwrap(), None);
}
