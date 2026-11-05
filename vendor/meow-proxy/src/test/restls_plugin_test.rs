use super::*;

#[test]
fn parse_opts_cases() {
    let cases: &[(&str, bool)] = &[
            // minimal — host, password and version-hint all required
            ("host=cover.example.com;password=p;version-hint=tls13", true),
            ("host=cover.example.com;password=p;version-hint=tls12", true),
            ("host=cover.example.com;password=p;version-hint=TLS13", true),
            // every upstream option
            (
                "host=h;password=p;version-hint=tls13;restls-script=200?100<1",
                true,
            ),
            ("host=h;password=p;version-hint=tls13;skip-cert-verify=true", true),
            ("host=h;password=p;version-hint=tls13;name-cert-verify=v.example.com", true),
            (
                "host=h;password=p;version-hint=tls13;fingerprint=aa:bb:cc:dd:ee:ff:00:11:22:33:44:55:66:77:\
                 88:99:aa:bb:cc:dd:ee:ff:00:11:22:33:44:55:66:77:88:99",
                true,
            ),
            ("host=h;password=p;version-hint=tls13;force-tls12=true", true),
            // missing required opts
            ("password=p;version-hint=tls13", false),           // no host
            ("host=h;version-hint=tls13", false),               // no password
            ("host=h;password=p", false),                       // no version-hint
            ("host=h;password=p;version-hint=", false),         // empty hint
            ("host=h;password=p;version-hint=tls11", false),    // bad hint
            ("host=h;password=p;version-hint=1.3", false),      // bad hint
            ("", false),
            ("host=h;password=p;version-hint=tls13;fingerprint=chrome", false),
            ("host=h;password=p;version-hint=tls13;fingerprint=zz", false),
            ("host=h;password=p;version-hint=tls13;restls-script=abc", false),
            ("host=h;password=p;version-hint=tls13;restls-script=20000<300", false),
            // keys are case-insensitive (mapstructure parity)
            ("Host=cover.example.com;Password=p;Version-Hint=tls13", true),
            // security-relevant bools reject junk — a typo must not
            // silently coerce to "verify on" / "tls12 path off"
            ("host=h;password=p;version-hint=tls13;skip-cert-verify=bogus", false),
            ("host=h;password=p;version-hint=tls13;force-tls12=maybe", false),
        ];
    for (opts, ok) in cases {
        assert_eq!(
            parse_opts(opts).is_ok(),
            *ok,
            "opts {opts:?} expected ok={ok}"
        );
    }
}

#[test]
fn parse_opts_full() {
    let cfg = parse_opts(
        "host=cover.example.com;password=psk;version-hint=tls12;\
             restls-script=100?50<1;skip-cert-verify=true;\
             name-cert-verify=real.example.com;\
             fingerprint=AABBCCDDEEFF00112233445566778899AABBCCDDEEFF00112233445566778899",
    )
    .unwrap();
    assert_eq!(cfg.version_hint, "tls12");
    assert_eq!(cfg.restls_script.as_deref(), Some("100?50<1"));
    assert!(cfg.skip_cert_verify);
    assert_eq!(cfg.verify_name.as_deref(), Some("real.example.com"));
    assert!(cfg.cert_pin.is_some());
}

#[test]
fn force_tls12_overrides_hint() {
    let cfg = parse_opts("host=h;password=p;version-hint=tls13;force-tls12=true").unwrap();
    assert_eq!(cfg.version_hint, "tls12");
    // No-op on an already-tls12 hint.
    let cfg = parse_opts("host=h;password=p;version-hint=tls12;force-tls12=true").unwrap();
    assert_eq!(cfg.version_hint, "tls12");
}
