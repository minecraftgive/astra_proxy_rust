use super::*;
#[derive(serde::Deserialize)]
struct TestConfig {
    outbounds: Vec<OutboundConfig>,
}

#[test]
fn auto_selects_peer_compatible_xhttp_mode() {
    assert_eq!(effective_xhttp_mode(Some("auto"), false), "packet-up");
    assert_eq!(effective_xhttp_mode(Some("auto"), true), "stream-one");
    assert_eq!(effective_xhttp_mode(None, false), "stream-one");
    assert_eq!(effective_xhttp_mode(Some("stream-up"), false), "stream-up");
}

#[test]
fn pin_and_encryption_validation_fail_early() {
    let source = r#"
rule_socket = "/tmp/astra-test-config.sock"
socks5_listen = "127.0.0.1:1080"
[[outbounds]]
kind = "native"
tag = "test"
[outbounds.config]
protocol = "vless"
server = "localhost"
port = 443
uuid = "00000000-0000-0000-0000-000000000001"
flow = "xtls-rprx-vision"
encryption = "mlkem768x25519plus.native.0rtt.AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA"
[outbounds.config.tls]
server_name = "localhost"
cert_pin_sha256 = "1111111111111111111111111111111111111111111111111111111111111111"
[outbounds.config.transport]
kind = "xhttp"
mode = "auto"
"#;
    let config: TestConfig = toml::from_str(source).unwrap();
    compile(&config.outbounds, Path::new(".")).unwrap();
    for (before, after) in [
        (
            "1111111111111111111111111111111111111111111111111111111111111111",
            "ab",
        ),
        (
            "mlkem768x25519plus.native.0rtt.AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA",
            "bad-encryption",
        ),
    ] {
        let config: TestConfig = toml::from_str(&source.replace(before, after)).unwrap();
        assert!(compile(&config.outbounds, Path::new(".")).is_err());
    }
}
