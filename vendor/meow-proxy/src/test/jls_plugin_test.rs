use super::*;

#[test]
fn parse_opts_cases() {
    let cases: &[(&str, bool)] = &[
        // minimal — host, username and password all required
        ("host=cover.example.com;username=u;password=p", true),
        ("host=h;username=u;password=p;alpn=h2,http/1.1", true),
        ("host=h;username=u;password=p;alpn=", true),
        // missing required opts
        ("username=u;password=p", false),       // no host
        ("host=h;password=p", false),           // no username
        ("host=h;username=u", false),           // no password
        ("host=h;username=;password=p", false), // empty username
        ("", false),
        // jls has no skip-cert-verify — the random auth is the cert
        // check; accepting it would silently lie to the user.
        ("host=h;username=u;password=p;skip-cert-verify=true", false),
        ("host=h;username=u;password=p;skip_cert_verify=false", false),
    ];
    for (opts, ok) in cases {
        assert_eq!(
            parse_opts(opts).is_ok(),
            *ok,
            "opts {opts:?} expected ok={ok}"
        );
    }
}

/// `sip003_opts` lowercases keys — upstream decodes the opts map
/// through mapstructure, which matches case-insensitively.
#[test]
fn parse_opts_case_insensitive_keys() {
    let cfg = parse_opts("Host=cover.example.com;USERNAME=u;Password=p").unwrap();
    assert_eq!(cfg.host, "cover.example.com");
    assert_eq!(cfg.username, "u");
    assert_eq!(cfg.password, "p");
}

#[test]
fn parse_opts_full() {
    let cfg =
        parse_opts("host=cover.example.com;username=alice;password=secret;alpn=h3,h2").unwrap();
    assert_eq!(cfg.host, "cover.example.com");
    assert_eq!(cfg.username, "alice");
    assert_eq!(cfg.password, "secret");
    assert_eq!(cfg.alpn, ["h3", "h2"]);
}
