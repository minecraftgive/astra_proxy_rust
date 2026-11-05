use super::*;

const FAKE_ECH_B64: &str =
    "AEX+DQBBRwAgACBs1mZ0bOlNXgMxOAxJgD/h+pSyM6F7nGhmGyR0R0p3agAEAAEAAQAOcHVibGljLmV4YW1wbGUAAA==";

#[test]
fn parse_minimal_client_config() {
    let s = format!("mode=client;sni=tunnel.example.com;path=/ws-secret;ech_config={FAKE_ECH_B64}");
    let cfg = parse_opts(&s).expect("ok");
    assert_eq!(cfg.sni, "tunnel.example.com");
    assert_eq!(cfg.path, "/ws-secret");
    assert!(!cfg.ech_config.is_empty());
}

#[test]
fn parse_rejects_server_mode() {
    let err = parse_opts(&format!(
        "mode=server;sni=tunnel.example.com;path=/ws;ech_config={FAKE_ECH_B64}"
    ))
    .unwrap_err();
    let MeowError::Config(msg) = err else {
        panic!("expected Config error");
    };
    assert!(msg.contains("only 'client' is implemented"));
}

#[test]
fn parse_rejects_missing_sni() {
    let err = parse_opts(&format!("mode=client;path=/ws;ech_config={FAKE_ECH_B64}")).unwrap_err();
    assert!(format!("{err}").contains("'sni'"));
}

#[test]
fn parse_rejects_missing_path() {
    let err = parse_opts(&format!(
        "mode=client;sni=tunnel.example.com;ech_config={FAKE_ECH_B64}"
    ))
    .unwrap_err();
    assert!(format!("{err}").contains("'path'"));
}

#[test]
fn parse_rejects_relative_path() {
    let err = parse_opts(&format!(
        "mode=client;sni=tunnel.example.com;path=ws;ech_config={FAKE_ECH_B64}"
    ))
    .unwrap_err();
    assert!(format!("{err}").contains("must start with '/'"));
}

#[test]
fn parse_rejects_missing_ech_config() {
    let err = parse_opts("mode=client;sni=tunnel.example.com;path=/ws").unwrap_err();
    assert!(format!("{err}").contains("'ech_config'"));
}

#[test]
fn parse_rejects_invalid_base64() {
    let err =
        parse_opts("mode=client;sni=t.example;path=/ws;ech_config=not!!base64==").unwrap_err();
    assert!(format!("{err}").contains("base64"));
}

#[test]
fn parse_accepts_fast_open_ignored() {
    let cfg = parse_opts(&format!(
        "mode=client;sni=t.example;path=/ws;ech_config={FAKE_ECH_B64};fast_open=true"
    ))
    .expect("ok");
    assert_eq!(cfg.sni, "t.example");
}

#[test]
fn parse_fingerprint_cases() {
    // (label, opts suffix appended to the minimal client opts, expected cfg.fingerprint)
    let cases: &[(&str, &str, Option<&str>)] = &[
        ("explicit chrome", ";fingerprint=chrome", Some("chrome")),
        ("absent defaults to chrome", "", Some("chrome")),
        ("none opts out", ";fingerprint=none", None),
    ];

    for (label, suffix, expected) in cases {
        let cfg = parse_opts(&format!(
            "mode=client;sni=t.example;path=/ws;ech_config={FAKE_ECH_B64}{suffix}"
        ))
        .unwrap_or_else(|e| panic!("case {label}: parse_opts failed: {e}"));
        assert_eq!(cfg.fingerprint.as_deref(), *expected, "case: {label}");
    }
}

#[test]
fn parse_unknown_key_ignored() {
    let cfg = parse_opts(&format!(
        "mode=client;sni=t.example;path=/ws;ech_config={FAKE_ECH_B64};unknown_key=1"
    ))
    .expect("ok");
    assert_eq!(cfg.path, "/ws");
}
