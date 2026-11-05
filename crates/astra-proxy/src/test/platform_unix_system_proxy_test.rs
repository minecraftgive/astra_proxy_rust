use super::*;
#[test]
fn system_settings_plan_restores_original_endpoint_and_enable_states() {
    let config: ProxyConfig = toml::from_str("standalone=true\nmixed_listen='127.0.0.1:7891'\nsocks5_listen='127.0.0.1:1080'\n[system_proxy]\nservices=['Wi-Fi']\n[[outbounds]]\nkind='socks5'\ntag='node'\nserver='127.0.0.1:7890'\n").unwrap();
    if !cfg!(target_os = "macos") {
        return;
    }
    let plan = plan(&config, |args| {
        if args[1].ends_with("proxy") {
            Ok(
                "Enabled: No\nServer: original.example\nPort: 1234\nAuthenticated Proxy Enabled: 0"
                    .into(),
            )
        } else {
            Ok("Enabled: Yes".into())
        }
    })
    .unwrap();
    assert_eq!(plan.len(), 8);
    for pair in plan[2..].as_chunks::<2>().0 {
        assert_eq!(pair[0].apply.last().unwrap(), "7891");
        assert_eq!(&pair[1].restore[3..], &["original.example", "1234"]);
        assert_eq!(pair[0].restore.last().unwrap(), "off");
    }
    assert!(plan[..2]
        .iter()
        .all(|change| change.restore.last().unwrap() == "on"));
}
#[cfg(target_os = "linux")]
#[test]
fn gnome_snapshot_captures_restored_values_and_expected_overrides() {
    let config: ProxyConfig=toml::from_str("standalone=true\nmixed_listen='127.0.0.1:7891'\n[system_proxy]\n[[outbounds]]\nkind='socks5'\ntag='node'\nserver='127.0.0.1:7890'\n").unwrap();
    let restored = |args: &[String]| -> Result<String> {
        Ok(match args[3].as_str() {
            "host" => "'original.proxy'",
            "port" => "3128",
            "mode" => "'auto'",
            "use-same-proxy" => "true",
            _ => panic!("unexpected key"),
        }
        .into())
    };
    let changes = plan(&config, restored).unwrap();
    let snapshot = checks_with(&changes, restored).unwrap();
    assert_eq!(snapshot.len(), 9);
    let mode = snapshot.iter().find(|c| c.query[3] == "mode").unwrap();
    assert_eq!(mode.before, "'auto'");
    assert_eq!(mode.expected, "'manual'");
    let http = snapshot
        .iter()
        .find(|c| c.query[2] == "org.gnome.system.proxy.http" && c.query[3] == "host")
        .unwrap();
    assert_eq!(http.before, "'original.proxy'");
    assert_eq!(http.expected, "'127.0.0.1'");
    for change in changes {
        assert!(!change.restore.iter().any(|s| s.contains("7891")));
    }
    assert!(compatible(
        "Enabled: Yes\nServer: original\nPort: 3128",
        "Enabled: No\nServer: original\nPort: 3128",
        "Enabled: Yes\nServer: localhost\nPort: 7891"
    ));
    assert!(!compatible(
        "Enabled: Yes\nServer: external\nPort: 3128",
        "Enabled: No\nServer: original\nPort: 3128",
        "Enabled: Yes\nServer: localhost\nPort: 7891"
    ));
}
#[test]
fn recovery_rejects_arbitrary_commands_before_execution() {
    let path =
        std::env::temp_dir().join(format!("astra-recovery-test-{}.toml", std::process::id()));
    std::fs::write(&path, "[[changes]]\napply=[]\nrestore=['/bin/false']\n").unwrap();
    assert!(recover(&path)
        .unwrap_err()
        .to_string()
        .contains("unsupported command"));
    assert!(path.exists());
    std::fs::remove_file(path).unwrap();
}
