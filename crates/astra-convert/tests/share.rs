use astra_convert::{share::convert_links, Options};
const LINK: &str = "vless://00000000-0000-4000-8000-000000000001@example.com:52588?encryption=mlkem768x25519plus.native.0rtt.AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA&flow=xtls-rprx-vision&security=tls&sni=tls.example.com&pcs=1111111111111111111111111111111111111111111111111111111111111111&type=xhttp&path=%2Fhello%3Fa%3D1%26b%3D2&mode=auto#TW%20%E8%8A%82%E7%82%B9";
#[test]
fn vision_encryption_pin_and_xhttp_survive_import() {
    let converted = convert_links(LINK, &Options::default()).unwrap();
    let doc: toml::Value = toml::from_str(&converted.proxy).unwrap();
    let node = &doc["outbounds"][0];
    assert_eq!(node["tag"].as_str(), Some("TW 节点"));
    let config = &node["config"];
    assert_eq!(config["protocol"].as_str(), Some("vless"));
    assert_eq!(config["flow"].as_str(), Some("xtls-rprx-vision"));
    assert_eq!(
        config["encryption"].as_str(),
        Some("mlkem768x25519plus.native.0rtt.AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA")
    );
    assert_eq!(
        config["tls"]["server_name"].as_str(),
        Some("tls.example.com")
    );
    assert_eq!(
        config["tls"]["cert_pin_sha256"].as_str(),
        Some("1111111111111111111111111111111111111111111111111111111111111111")
    );
    assert_eq!(config["transport"]["kind"].as_str(), Some("xhttp"));
    assert_eq!(config["transport"]["path"].as_str(), Some("/hello?a=1&b=2"));
    assert_eq!(config["transport"]["mode"].as_str(), Some("auto"));
}
#[test]
fn line_lists_ipv6_reality_and_errors_are_explicit() {
    let reality = "vless://00000000-0000-4000-8000-000000000002@[::1]:443?security=reality&sni=example.com&pbk=AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA&sid=abcd&fp=chrome&type=grpc&serviceName=test#reality";
    let converted = convert_links(&format!("{LINK}\n\n{reality}"), &Options::default()).unwrap();
    assert_eq!(converted.nodes, 2);
    let doc: toml::Value = toml::from_str(&converted.proxy).unwrap();
    assert_eq!(
        doc["outbounds"][1]["config"]["server"].as_str(),
        Some("::1")
    );
    assert_eq!(
        doc["outbounds"][1]["config"]["tls"]["fingerprint"].as_str(),
        Some("chrome")
    );
    assert!(convert_links(&format!("{LINK}\n{LINK}"), &Options::default()).is_err());
    for invalid in [
        LINK.replace("mode=auto", "secret=secret-value"),
        LINK.replace("mode=auto", "mode=auto&mode=stream-one"),
        LINK.replace("security=tls", "security=none"),
        LINK.replace("mode=auto", "allowInsecure=1"),
        LINK.replace(
            "pcs=1111111111111111111111111111111111111111111111111111111111111111",
            "pcs=bad",
        ),
    ] {
        let error = convert_links(&invalid, &Options::default()).err().unwrap();
        let error = format!("{error:#}");
        assert!(!error.contains("00000000-0000") && !error.contains("secret-value"));
    }
}

#[test]
fn cli_link_and_stdin_only_write_proxy_config() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory =
        std::env::temp_dir().join(format!("astra-share-{}-{nonce}", std::process::id()));
    for (index, input) in [LINK, "-"].iter().enumerate() {
        let output = directory.join(index.to_string());
        let mut child = Command::new(env!("CARGO_BIN_EXE_astra-convert"))
            .args([input, "-o", output.to_str().unwrap()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        if *input == "-" {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(LINK.as_bytes())
                .unwrap();
        }
        let result = child.wait_with_output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(output.join("proxy.toml").is_file());
        assert!(!output.join("rule.toml").exists());
        assert!(!String::from_utf8_lossy(&result.stderr).contains("00000000-0000"));
        std::fs::remove_file(output.join("proxy.toml")).unwrap();
        std::fs::remove_dir(output).unwrap();
    }
    std::fs::remove_dir(directory).unwrap();
}

#[test]
fn other_protocols_and_base64_subscription_lists() {
    use base64::{
        engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
        Engine,
    };
    let vmess = serde_json::json!({"v":"2","ps":"vmess-ws","add":"example.com","port":"443","id":"00000000-0000-4000-8000-000000000002","aid":0,"scy":"auto","net":"ws","type":"none","host":"cdn.example.com","path":"/ws","tls":"tls","sni":"example.com"});
    let lines = [
        format!("vmess://{}", STANDARD.encode(vmess.to_string())),
        "trojan://pass%3Aword@example.com:443?security=tls&sni=example.com&type=ws&path=%2Fws#trojan-ws".into(),
        format!("ss://{}@example.com:8388#ss-aead",URL_SAFE_NO_PAD.encode("aes-128-gcm:pass:word")),
        "tuic://00000000-0000-4000-8000-000000000003:pass%3Aword@example.com:443?sni=example.com&congestion_control=bbr&udp_relay_mode=native#tuic".into(),
        "hy2://user:pass%3Aword@example.com/?sni=example.com&obfs=salamander&obfs-password=obfs-secret#hy2".into(),
        "socks5://user:pass%3Aword@[::1]:1080#socks".into(),
    ];
    let input = lines.join("\n");
    let converted = convert_links(&input, &Options::default()).unwrap();
    assert_eq!(converted.nodes, 6);
    let doc: toml::Value = toml::from_str(&converted.proxy).unwrap();
    assert_eq!(
        doc["outbounds"][0]["config"]["transport"]["path"].as_str(),
        Some("/ws")
    );
    assert_eq!(
        doc["outbounds"][1]["config"]["password"].as_str(),
        Some("pass:word")
    );
    assert_eq!(
        doc["outbounds"][2]["config"]["password"].as_str(),
        Some("pass:word")
    );
    assert_eq!(
        doc["outbounds"][3]["config"]["congestion_control"].as_str(),
        Some("bbr")
    );
    assert_eq!(
        doc["outbounds"][4]["config"]["port"].as_integer(),
        Some(443)
    );
    assert_eq!(
        doc["outbounds"][4]["config"]["password"].as_str(),
        Some("user:pass:word")
    );
    assert_eq!(doc["outbounds"][5]["password"].as_str(), Some("pass:word"));
    for encoded in [STANDARD.encode(&input), URL_SAFE_NO_PAD.encode(&input)] {
        assert!(astra_convert::share::is_share(&encoded));
        assert_eq!(
            convert_links(&encoded, &Options::default()).unwrap().proxy,
            converted.proxy
        );
        assert_eq!(
            astra_convert::subscription::convert_nodes(
                &encoded,
                &Options::default(),
                "test",
                false
            )
            .unwrap()
            .nodes,
            6
        );
    }
    let legacy = format!(
        "ss://{}#old",
        STANDARD.encode("aes-128-gcm:pass@example.com:8388")
    );
    assert_eq!(
        convert_links(&legacy, &Options::default()).unwrap().nodes,
        1
    );
}
#[test]
fn unsupported_options_never_echo_secrets_or_create_a_partial_import() {
    for link in [
        "ss://YWVzLTEyOC1nY206c2VjcmV0@example.com:8388?plugin=secret-plugin#ss",
        "hy2://secret@example.com?insecure=1#hy",
        "hy2://secret@example.com?obfs=gecko&obfs-password=secret#hy",
        "tuic://00000000-0000-4000-8000-000000000003:secret@example.com:443?alpn=h3#tuic",
        "trojan://secret@example.com:443?type=kcp#trojan",
    ] {
        let error = convert_links(link, &Options::default()).err().unwrap();
        assert!(!format!("{error:#}").contains("secret"));
        assert!(convert_links(&format!("{LINK}\n{link}"), &Options::default()).is_err());
    }
}
