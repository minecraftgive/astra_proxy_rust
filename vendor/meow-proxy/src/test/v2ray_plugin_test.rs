use super::*;

/// Provider-controlled `header=` opts repeat unboundedly and the map is
/// re-serialized per dial — same bound as xhttp `headers` (issue #648).
#[test]
fn parse_opts_caps_header_count() {
    let mut opts = String::from("mode=websocket");
    for i in 0..meow_transport::MAX_EXTRA_HEADERS {
        use std::fmt::Write;
        write!(opts, ";header=H{i}:v").unwrap();
    }
    // At the cap: still parses (re-`insert` of a known key is a replace).
    assert!(parse_opts(&opts).is_ok());
    use std::fmt::Write;
    write!(opts, ";header=one-more:v").unwrap();
    let Err(err) = parse_opts(&opts) else {
        panic!(
            "{} headers must be rejected",
            meow_transport::MAX_EXTRA_HEADERS + 1
        )
    };
    assert!(err.to_string().contains("header"), "msg: {err}");
}

#[test]
fn parse_opts_cases() {
    struct Case {
        name: &'static str,
        input: &'static str,
        /// `None` means `parse_opts` must return `Err`.
        expect: Option<V2rayPluginConfig>,
    }

    let cases = [
        Case {
            name: "websocket_mux",
            input: "mode=websocket;mux=1;host=example.com;path=/ws",
            expect: Some(V2rayPluginConfig {
                mode: Mode::Websocket,
                tls: false,
                host: "example.com".to_string(),
                path: "/ws".to_string(),
                headers: Default::default(),
                skip_cert_verify: false,
                mux: true,
            }),
        },
        Case {
            name: "tls_websocket_mux_skip_verify",
            input: "mode=websocket;tls;mux=1;host=example.com;path=/ws;skip-cert-verify=true",
            expect: Some(V2rayPluginConfig {
                mode: Mode::Websocket,
                tls: true,
                host: "example.com".to_string(),
                path: "/ws".to_string(),
                headers: Default::default(),
                skip_cert_verify: true,
                mux: true,
            }),
        },
        Case {
            name: "defaults_on_empty",
            input: "",
            expect: Some(V2rayPluginConfig {
                mode: Mode::Websocket,
                tls: false,
                host: String::new(),
                path: "/".to_string(),
                headers: Default::default(),
                skip_cert_verify: false,
                mux: false,
            }),
        },
        Case {
            name: "bare_tls_and_mux",
            input: "tls;mux",
            expect: Some(V2rayPluginConfig {
                mode: Mode::Websocket,
                tls: true,
                host: String::new(),
                path: "/".to_string(),
                headers: Default::default(),
                skip_cert_verify: false,
                mux: true,
            }),
        },
        Case {
            name: "unknown_key_ignored",
            input: "mode=websocket;foo=bar;path=/ws",
            expect: Some(V2rayPluginConfig {
                mode: Mode::Websocket,
                tls: false,
                host: String::new(),
                path: "/ws".to_string(),
                headers: Default::default(),
                skip_cert_verify: false,
                mux: false,
            }),
        },
        Case {
            name: "header_opt",
            input: "mode=websocket;header=X-Foo:bar;host=example.com",
            expect: Some(V2rayPluginConfig {
                mode: Mode::Websocket,
                tls: false,
                host: "example.com".to_string(),
                path: "/".to_string(),
                headers: [("X-Foo".to_string(), "bar".to_string())]
                    .into_iter()
                    .collect(),
                skip_cert_verify: false,
                mux: false,
            }),
        },
        Case {
            name: "bad_mode_errors",
            input: "mode=quic",
            expect: None,
        },
    ];

    let mut failures = Vec::new();
    for case in &cases {
        match (parse_opts(case.input), case.expect.as_ref()) {
            (Ok(cfg), Some(want)) => {
                if cfg.mode != want.mode
                    || cfg.tls != want.tls
                    || cfg.host != want.host
                    || cfg.path != want.path
                    || cfg.headers != want.headers
                    || cfg.skip_cert_verify != want.skip_cert_verify
                    || cfg.mux != want.mux
                {
                    failures.push(format!("{}: got {cfg:?}, want {want:?}", case.name));
                }
            }
            (Ok(cfg), None) => {
                failures.push(format!("{}: expected Err, got Ok({cfg:?})", case.name));
            }
            (Err(e), Some(_)) => {
                failures.push(format!("{}: expected Ok, got Err({e})", case.name));
            }
            (Err(_), None) => {}
        }
    }

    assert!(
        failures.is_empty(),
        "parse_opts case failures:\n{}",
        failures.join("\n")
    );
}
