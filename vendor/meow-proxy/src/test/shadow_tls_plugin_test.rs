use super::*;

#[test]
fn parse_opts_cases() {
    let cases: &[(&str, bool)] = &[
        ("host=cover.example.com", false), // version required (upstream parity)
        ("host=cover.example.com;password=psk", false), // ditto
        // every version accepted
        ("host=h;version=1;password=p", true),
        ("host=h;version=2;password=p", true),
        ("host=h;version=3;password=p", true),
        // upstream option surface
        ("host=h;version=2;password=p;alpn=h2,http/1.1", true),
        ("host=h;version=2;alpn=", true), // explicit empty = no ALPN
        ("host=h;version=2;skip-cert-verify=true", true),
        ("host=h;version=2;name-cert-verify=real.example.com", true),
        (
            "host=h;version=2;fingerprint=aa:bb:cc:dd:ee:ff:00:11:22:33:44:55:66:77:\
                 88:99:aa:bb:cc:dd:ee:ff:00:11:22:33:44:55:66:77:88:99",
            true,
        ),
        ("host=h;version=0", false),
        ("host=h;version=4", false),
        ("host=h;version=three", false),
        ("version=2;password=psk", false), // host required
        ("", false),                       // empty → no host
        ("host=h;version=2;fingerprint=chrome", false), // uTLS name rejected
        ("host=h;version=2;fingerprint=zz", false), // bad hex
        // Security-relevant booleans parse strictly — a typo is a
        // config error, not a silent downgrade.
        ("host=h;version=2;skip-cert-verify=bogus", false),
        ("host=h;version=3;strict-mode=bogus", false),
        ("host=h;version=3;strict-mode=true", true),
        // Explicit-empty value clears the opt — same as absent.
        ("host=h;version=2;fingerprint=", true),
        ("host=h;version=2;name-cert-verify=", true),
        ("host=h;version=2;certificate=nonexistent.pem", false), // path missing
        ("host=h;version=2;certificate=-----BEGIN X-----\nPEM", false), // key missing
    ];
    for (opts, ok) in cases {
        assert_eq!(
            parse_opts(opts).is_ok(),
            *ok,
            "opts {opts:?} expected ok={ok}"
        );
    }
}

/// Upstream removes the hybrid-PQ `X25519MLKEM768` key share from the
/// v2 ClientHello after uTLS shaping — the strip never replaces the
/// shaped group list. Every shipped profile already excludes ML-KEM,
/// so the classic pin applies only when no profile resolves and the
/// vendored-BoringSSL default hello (which offers ML-KEM) would run.
#[test]
fn tls_config_for_pins_v2_classic_curves() {
    for (version, want) in [(1u8, None), (2, Some("X25519:P-256:P-384")), (3, None)] {
        let cfg = parse_opts(&format!("host=h;version={version};password=p")).unwrap();
        let tls = tls_config_for(&cfg, None);
        assert_eq!(
            tls.curves.as_deref(),
            want,
            "version {version} curves pin mismatch"
        );
    }
    // A resolved fingerprint lifts the pin — its own group list is
    // already ML-KEM-free, and overriding it would break the profile
    // (firefox's P-521, android's P-256-first ordering).
    let cfg = parse_opts("host=h;version=2;password=p").unwrap();
    for fp in ["chrome", "firefox", "android", "random"] {
        let tls = tls_config_for(&cfg, Some(fp));
        assert_eq!(tls.curves.as_deref(), None, "{fp} must keep its own list");
        assert_eq!(tls.fingerprint.as_deref(), Some(fp));
    }
    // A deferred/unknown fingerprint resolves to nothing — the
    // default hello offers ML-KEM, so the pin must hold.
    let tls = tls_config_for(&cfg, Some("randomized"));
    assert_eq!(tls.curves.as_deref(), Some("X25519:P-256:P-384"));
    assert_eq!(tls.fingerprint.as_deref(), Some("randomized"));
}

#[test]
fn parse_opts_defaults() {
    let cfg = parse_opts("host=cover.example.com;version=2").unwrap();
    assert_eq!(cfg.alpn, ["h2", "http/1.1"], "upstream DefaultALPN");
    assert!(cfg.password.is_empty());
    assert!(cfg.verify_name.is_none());
    assert!(cfg.cert_pin.is_none());

    // `alpn=` explicit-empty suppresses the extension entirely —
    // upstream `opt.ALPN != nil` distinguishes it from "absent".
    let cfg = parse_opts("host=cover.example.com;version=2;alpn=").unwrap();
    assert!(cfg.alpn.is_empty(), "explicit alpn= must not re-default");
}

#[test]
fn parse_opts_full() {
    let cfg = parse_opts(
        "host=cover.example.com;password=psk;version=3;alpn=h3,h2;\
             skip-cert-verify=true;name-cert-verify=real.example.com;\
             fingerprint=AABBCCDDEEFF00112233445566778899AABBCCDDEEFF00112233445566778899",
    )
    .unwrap();
    assert_eq!(cfg.version, 3);
    assert_eq!(cfg.alpn, ["h3", "h2"]);
    assert!(cfg.skip_cert_verify);
    assert_eq!(cfg.verify_name.as_deref(), Some("real.example.com"));
    assert!(cfg.cert_pin.is_some());
}

#[test]
fn build_tls_layer_bounds() {
    // Construction succeeds for every version with a valid host —
    // bounds (v1 → max 1.2, all → min 1.2) are enforced inside
    // TlsConfig; invalid combos would error here.
    for v in 1..=3u8 {
        let cfg = parse_opts(&format!("host=cover.example.com;version={v}")).unwrap();
        build_tls_layer(&cfg, None).unwrap_or_else(|e| panic!("v{v}: {e}"));
    }
    // client-fingerprint is forwarded into the cover handshake —
    // except under v1, whose TLS 1.2 cap drops it (upstream parity).
    let cfg = parse_opts("host=cover.example.com;version=3").unwrap();
    build_tls_layer(&cfg, Some("chrome")).unwrap();
    let cfg = parse_opts("host=cover.example.com;version=1").unwrap();
    build_tls_layer(&cfg, Some("chrome")).unwrap();
}

/// The version-dependent TlsConfig shaping, asserted field by field:
/// a dropped `max_version` would send a TLS 1.3 ClientHello for v1;
/// a dropped `curves` pin reintroduces X25519MLKEM768 into v2.
#[test]
fn tls_config_for_shapes_per_version() {
    use meow_transport::tls::TlsVersion;
    for v in 1..=3u8 {
        let cfg = parse_opts(&format!("host=cover.example.com;version={v}")).unwrap();
        let c = tls_config_for(&cfg, None);
        assert_eq!(c.min_version, Some(TlsVersion::Tls12), "v{v} floor");
        assert_eq!(
            c.max_version,
            (v == 1).then_some(TlsVersion::Tls12),
            "v{v} cap"
        );
        assert_eq!(c.curves.is_some(), v == 2, "v{v} MLKEM drop applies to v2");
    }
    // A resolved v2 fingerprint keeps its own (ML-KEM-free) group
    // list — the pin lifts — while v1 drops the fingerprint entirely
    // (upstream parity).
    let cfg = parse_opts("host=cover.example.com;version=2").unwrap();
    let c = tls_config_for(&cfg, Some("chrome"));
    assert!(c.curves.is_none() && c.fingerprint.is_some());
    let cfg = parse_opts("host=cover.example.com;version=1").unwrap();
    let c = tls_config_for(&cfg, Some("chrome"));
    assert!(
        c.fingerprint.is_none(),
        "v1 must not carry a 1.3-shaped profile"
    );
}

/// `strict-mode` is parsed strictly and threaded to the transport
/// dial (the wire-level gate itself is e2e'd in meow-transport's
/// `v3_strict_mode_rejects_tls12_cover`).
#[test]
fn strict_mode_parses() {
    let cfg = parse_opts("host=h;version=3;strict-mode=true").unwrap();
    assert!(cfg.strict_mode);
    let cfg = parse_opts("host=h;version=3").unwrap();
    assert!(!cfg.strict_mode);
}
