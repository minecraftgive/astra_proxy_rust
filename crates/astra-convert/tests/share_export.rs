use astra_convert::{
    share::{convert_links, export_node},
    Options,
};
use toml::Value;

fn node(link: &str) -> Value {
    let converted = convert_links(link, &Options::default()).unwrap();
    let doc: Value = toml::from_str(&converted.proxy).unwrap();
    doc["outbounds"][0].clone()
}

#[test]
fn native_share_roundtrips_representable_options() {
    let links = [
        "vless://00000000-0000-4000-8000-000000000001@example.com:443?encryption=none&flow=xtls-rprx-vision&security=tls&sni=example.com&fp=chrome&type=ws&path=%2Fedge&host=cdn.example.com#vless",
        "vmess://eyJ2IjoiMiIsInBzIjoidm1lc3MiLCJhZGQiOiJleGFtcGxlLmNvbSIsInBvcnQiOiI0NDMiLCJpZCI6IjAwMDAwMDAwLTAwMDAtNDAwMC04MDAwLTAwMDAwMDAwMDAwMiIsImFpZCI6MCwic2N5IjoiYXV0byIsIm5ldCI6IndzIiwiaG9zdCI6ImNkbi5leGFtcGxlLmNvbSIsInBhdGgiOiIvd3MiLCJ0bHMiOiJ0bHMifQ==",
        "ss://YWVzLTEyOC1nY206cGFzcw@example.com:8388#ss",
        "trojan://pass@example.com:443?security=tls&sni=example.com&type=grpc&serviceName=edge#trojan",
        "hysteria2://pass@example.com:443?sni=example.com&obfs=salamander&obfs-password=obfs#hy2",
        "tuic://00000000-0000-4000-8000-000000000003:pass@example.com:443?sni=example.com&congestion_control=bbr&udp_relay_mode=native#tuic",
    ];
    for link in links {
        let original = node(link);
        let exported = export_node(&original).unwrap();
        let roundtrip = node(&exported);
        assert_eq!(roundtrip, original, "{exported}");
    }
}

#[test]
fn legacy_and_socks_roundtrip() {
    for link in [
        "socks5://user:pass@example.com:1080#socks",
        "vless://00000000-0000-4000-8000-000000000001@example.com:443?security=tls&sni=example.com#vless",
        "trojan://pass@example.com:443?sni=example.com#trojan",
    ] {
        let original = node(link);
        let exported = export_node(&original).unwrap();
        assert_eq!(node(&exported), original, "{exported}");
    }
}

#[test]
fn vmess_transport_options_are_rejected_without_loss() {
    let mut grpc = node(
        "vmess://eyJ2IjoiMiIsInBzIjoidm1lc3MiLCJhZGQiOiJleGFtcGxlLmNvbSIsInBvcnQiOiI0NDMiLCJpZCI6IjAwMDAwMDAwLTAwMDAtNDAwMC04MDAwLTAwMDAwMDAwMDAwMiIsImFpZCI6MCwic2N5IjoiYXV0byIsIm5ldCI6IndzIiwiaG9zdCI6ImNkbi5leGFtcGxlLmNvbSIsInBhdGgiOiIvd3MiLCJ0bHMiOiJ0bHMifQ==",
    );
    grpc["config"].as_table_mut().unwrap().insert(
        "transport".into(),
        Value::Table(toml::toml! {
            kind = "grpc"
            service_name = "private-grpc-service"
        }),
    );
    let error = export_node(&grpc).unwrap_err().to_string();
    assert!(error.contains("service name"), "{error}");
    assert!(!error.contains("private-grpc-service"), "{error}");

    let mut xhttp = node(
        "vmess://eyJ2IjoiMiIsInBzIjoidm1lc3MiLCJhZGQiOiJleGFtcGxlLmNvbSIsInBvcnQiOiI0NDMiLCJpZCI6IjAwMDAwMDAwLTAwMDAtNDAwMC04MDAwLTAwMDAwMDAwMDAwMiIsImFpZCI6MCwic2N5IjoiYXV0byIsIm5ldCI6IndzIiwiaG9zdCI6ImNkbi5leGFtcGxlLmNvbSIsInBhdGgiOiIvd3MiLCJ0bHMiOiJ0bHMifQ==",
    );
    xhttp["config"].as_table_mut().unwrap().insert(
        "transport".into(),
        Value::Table(toml::toml! {
            kind = "xhttp"
            mode = "packet-up"
        }),
    );
    let error = export_node(&xhttp).unwrap_err().to_string();
    assert!(error.contains("mode"), "{error}");
    assert!(!error.contains("packet-up"), "{error}");
}

#[test]
fn incomplete_tls_is_rejected_without_secrets() {
    let mut value = node(
        "vless://00000000-0000-4000-8000-000000000001@example.com:443?security=tls&sni=example.com#vless",
    );
    value["config"]["tls"] = Value::Table(Default::default());
    let error = export_node(&value).unwrap_err().to_string();
    assert!(error.contains("TLS server name is required"), "{error}");
    assert!(
        !error.contains("00000000-0000-4000-8000-000000000001"),
        "{error}"
    );
}

#[test]
fn unsupported_options_are_rejected_without_secrets() {
    let mut value = node("ss://YWVzLTEyOC1nY206cGFzcw@example.com:8388#ss");
    value["config"] = Value::Table(toml::toml! {
        protocol = "shadowsocks"
        server = "example.com"
        port = 8388
        cipher = "aes-128-gcm"
        password = "pass"
        mux = "smux"
    });
    let error = export_node(&value).unwrap_err().to_string();
    assert!(!error.contains("pass"));

    let http = Value::Table(toml::toml! {
        tag = "http"
        kind = "http"
        server = "example.com:8080"
    });
    assert!(export_node(&http).is_err());
}

#[test]
fn malformed_tls_and_tags_are_rejected() {
    let mut missing_key = node(
        "vless://00000000-0000-4000-8000-000000000001@example.com:443?security=tls&sni=example.com#vless",
    );
    missing_key["config"]
        .as_table_mut()
        .unwrap()
        .get_mut("tls")
        .unwrap()
        .as_table_mut()
        .unwrap()
        .insert("reality_short_id".into(), "abcd".into());
    let error = export_node(&missing_key).unwrap_err().to_string();
    assert!(error.contains("short ID requires a public key"), "{error}");

    let mut conflicting_pin = node(
        "vless://00000000-0000-4000-8000-000000000001@example.com:443?security=tls&sni=example.com#vless",
    );
    let tls = conflicting_pin["config"]
        .as_table_mut()
        .unwrap()
        .get_mut("tls")
        .unwrap()
        .as_table_mut()
        .unwrap();
    tls.insert("reality_public_key".into(), "public-key".into());
    tls.insert("cert_pin_sha256".into(), "aabb".into());
    let error = export_node(&conflicting_pin).unwrap_err().to_string();
    assert!(
        error.contains("certificate pin cannot be combined"),
        "{error}"
    );

    let mut bad_tag = node("ss://YWVzLTEyOC1nY206cGFzcw@example.com:8388#ss");
    bad_tag["tag"] = "bad\nname".into();
    let error = export_node(&bad_tag).unwrap_err().to_string();
    assert!(error.contains("control characters"), "{error}");

    let mut long_tag = node("ss://YWVzLTEyOC1nY206cGFzcw@example.com:8388#ss");
    long_tag["tag"] = "x".repeat(65).into();
    let error = export_node(&long_tag).unwrap_err().to_string();
    assert!(error.contains("64 bytes"), "{error}");
}

#[test]
fn required_tls_server_names_are_rejected() {
    for link in [
        "hysteria2://pass@example.com:443#hy2",
        "tuic://00000000-0000-4000-8000-000000000003:pass@example.com:443#tuic",
    ] {
        let mut value = node(link);
        value["config"]
            .as_table_mut()
            .unwrap()
            .get_mut("tls")
            .unwrap()
            .as_table_mut()
            .unwrap()
            .insert("server_name".into(), "".into());
        let error = export_node(&value).unwrap_err().to_string();
        assert!(error.contains("TLS server name is required"), "{error}");
    }
}

#[test]
fn malformed_credentials_are_rejected_without_echoing_values() {
    for link in [
        "vless://00000000-0000-0000-0000-000000000000@example.com:443#nil",
        "tuic://00000000-0000-0000-0000-000000000000:secret@example.com:443?sni=example.com#nil",
        "hysteria2://secret@example.com:443?obfs=salamander&obfs-password=#hy2",
        "hysteria2://secret@example.com:443?pinSHA256=abcd#hy2",
    ] {
        let error = match convert_links(link, &Options::default()) {
            Ok(_) => panic!("malformed credential was accepted: {link}"),
            Err(error) => error.to_string(),
        };
        assert!(!error.contains("secret"), "{error}");
    }

    let mut vmess = node(
        "vmess://eyJ2IjoiMiIsInBzIjoidm1lc3MiLCJhZGQiOiJleGFtcGxlLmNvbSIsInBvcnQiOiI0NDMiLCJpZCI6IjAwMDAwMDAwLTAwMDAtNDAwMC04MDAwLTAwMDAwMDAwMDAwMiIsImFpZCI6MCwic2N5IjoiYXV0byIsIm5ldCI6IndzIiwiaG9zdCI6ImNkbi5leGFtcGxlLmNvbSIsInBhdGgiOiIvd3MiLCJ0bHMiOiJ0bHMifQ==",
    );
    vmess["config"]["cipher"] = "invalid".into();
    let error = export_node(&vmess).unwrap_err().to_string();
    assert!(error.contains("VMess cipher"), "{error}");
    assert!(!error.contains("invalid"), "{error}");
}

#[test]
fn exported_credentials_are_explicit_and_roundtrip() {
    for link in [
        "vless://00000000-0000-4000-8000-000000000001@example.com:443#vless",
        "tuic://00000000-0000-4000-8000-000000000003:pass@example.com:443?sni=example.com#tuic",
        "hysteria2://pass@example.com:443?sni=example.com#hy2",
    ] {
        let original = node(link);
        let exported = export_node(&original).unwrap();
        let parsed = url::Url::parse(&exported).unwrap();
        assert!(!parsed.username().is_empty(), "{exported}");
        assert_eq!(node(&exported), original, "{exported}");
    }
}
