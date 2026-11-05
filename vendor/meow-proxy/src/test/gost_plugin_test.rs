use super::*;

#[test]
fn parse_opts_cases() {
    struct Case {
        name: &'static str,
        input: &'static str,
        check: fn(&Result<GostPluginConfig>) -> bool,
    }

    let cases = [
            Case {
                name: "upstream_defaults",
                // `gostObfsOption{Host: "bing.com", Mux: true}` — only mode
                // is required.  The mux default itself is asserted by
                // `mux_default_true_under_mux_feature` (mux builds only).
                input: "mode=websocket",
                check: |r| match r {
                    Ok(c) => c.host == "bing.com" && !c.tls && c.path == "/",
                    Err(_) => false,
                },
            },
            Case {
                name: "full",
                input: "mode=websocket;tls;mux=false;host=cdn.example.com;path=/ws;\
                        skip-cert-verify=true;header=CF-Token:abc;\
                        fingerprint=00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
                check: |r| match r {
                    Ok(c) => {
                        c.tls
                            && !c.mux
                            && c.host == "cdn.example.com"
                            && c.path == "/ws"
                            && c.skip_cert_verify
                            && c.cert_pin == Some([0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77,
                                0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
                                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77,
                                0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff])
                            && c.headers.get("CF-Token").map(String::as_str) == Some("abc")
                    }
                    Err(_) => false,
                },
            },
            Case {
                name: "fingerprint_utls_name_errors",
                // Upstream `NewFingerprintVerifier` rejects uTLS profile
                // names — `fingerprint` is cert pinning, not ClientHello.
                input: "mode=websocket;mux=false;fingerprint=chrome",
                check: |r| r.is_err(),
            },
            Case {
                name: "fingerprint_bad_hex_errors",
                input: "mode=websocket;mux=false;fingerprint=zz",
                check: |r| r.is_err(),
            },
            Case {
                name: "fingerprint_wrong_len_errors",
                input: "mode=websocket;mux=false;fingerprint=aabb",
                check: |r| r.is_err(),
            },
            Case {
                name: "missing_mode_errors",
                input: "host=example.com",
                check: |r| r.is_err(),
            },
            Case {
                name: "bad_mode_errors",
                input: "mode=quic",
                check: |r| r.is_err(),
            },
            Case {
                name: "lone_certificate_errors",
                input: "mode=websocket;certificate=PEM",
                check: |r| r.is_err(),
            },
            Case {
                name: "cert_pair_ok",
                input: "mode=websocket;certificate=-----BEGIN CERTIFICATE-----x;\
                        private-key=-----BEGIN KEY-----k",
                check: |r| matches!(r, Ok(c) if c.client_cert.is_some()),
            },
            Case {
                name: "cert_path_unreadable_errors",
                input: "mode=websocket;certificate=/nonexistent/cert.pem",
                check: |r| r.is_err(),
            },
            Case {
                name: "ech_enable_without_config_errors",
                input: "mode=websocket;ech-opts.enable=true",
                check: |r| r.is_err(),
            },
            Case {
                name: "ech_config_decodes",
                // "QUJD" = base64("ABC")
                input: "mode=websocket;ech-opts.enable=true;ech-opts.config=QUJD",
                check: |r| match r {
                    Ok(c) => matches!(&c.ech, Some(EchOpts::Config(b)) if b == b"ABC"),
                    Err(_) => false,
                },
            },
            Case {
                name: "ech_bad_base64_errors",
                input: "mode=websocket;ech-opts.enable=true;ech-opts.config=!!!",
                check: |r| r.is_err(),
            },
            Case {
                name: "name_cert_verify",
                input: "mode=websocket;name-cert-verify=real.example.com",
                check: |r| matches!(r, Ok(c) if c.name_cert_verify.as_deref() == Some("real.example.com")),
            },
            Case {
                name: "unknown_key_ignored",
                input: "mode=websocket;foo=bar",
                check: |r| r.is_ok(),
            },
            Case {
                name: "ws_alias",
                input: "mode=ws",
                check: |r| r.is_ok(),
            },
            Case {
                name: "case_insensitive_keys",
                // Upstream decodes via mapstructure — keys match case-insensitively.
                input: "MODE=websocket;MUX=false;HOST=Example.COM;PATH=/Up;TLS",
                check: |r| matches!(r, Ok(c) if c.tls && !c.mux
                    && c.host == "Example.COM" && c.path == "/Up"),
            },
            Case {
                name: "path_without_slash_normalized",
                input: "mode=websocket;mux=false;path=ws",
                check: |r| matches!(r, Ok(c) if c.path == "/ws"),
            },
            Case {
                name: "path_with_space_errors",
                input: "mode=websocket;mux=false;path=/a b",
                check: |r| r.is_err(),
            },
            Case {
                name: "bad_tls_bool_errors",
                // `tls=bogus` must not silently degrade to plaintext ws.
                input: "mode=websocket;tls=enbale",
                check: |r| r.is_err(),
            },
            Case {
                name: "bad_mux_bool_errors",
                input: "mode=websocket;mux=maybe",
                check: |r| r.is_err(),
            },
            Case {
                name: "bad_skip_cert_verify_errors",
                input: "mode=websocket;mux=false;skip-cert-verify=tru",
                check: |r| r.is_err(),
            },
            Case {
                name: "bad_ech_enable_bool_errors",
                input: "mode=websocket;mux=false;ech-opts.enable=yep;ech-opts.config=QUJD",
                check: |r| r.is_err(),
            },
        ];

    let mut failures = Vec::new();
    for case in &cases {
        // Without the `mux` feature the upstream mux default (`true`)
        // is a parse error; neutralize it for inputs that do not
        // exercise mux themselves.
        let owned;
        let input = if cfg!(feature = "mux") || case.input.contains("mux") {
            case.input
        } else {
            owned = format!("{};mux=false", case.input);
            &owned
        };
        let r = parse_opts(input);
        if !(case.check)(&r) {
            failures.push(format!("{}: {r:?}", case.name));
        }
    }
    assert!(failures.is_empty(), "parse_opts failures: {failures:?}");
}

/// Empty values: `host` is a hard error, `path`/`name-cert-verify`/
/// `fingerprint` normalize to defaults/absent (upstream parity).
#[test]
fn parse_opts_empty_values() {
    assert!(parse_opts("mode=websocket;mux=false;host=").is_err());
    assert_eq!(
        parse_opts("mode=websocket;mux=false;path=").unwrap().path,
        "/"
    );
    assert!(parse_opts("mode=websocket;mux=false;name-cert-verify=")
        .unwrap()
        .name_cert_verify
        .is_none());
    assert!(parse_opts("mode=websocket;mux=false;fingerprint=")
        .unwrap()
        .cert_pin
        .is_none());
}

/// Provider-controlled `header=` opts repeat unboundedly — cap them like
/// v2ray-plugin / xhttp `headers` (issue #648).
#[test]
fn parse_opts_caps_header_count() {
    let mut opts = String::from("mode=websocket");
    for i in 0..meow_transport::MAX_EXTRA_HEADERS {
        use std::fmt::Write;
        write!(opts, ";header=H{i}:v").unwrap();
    }
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

/// `Host` entries normalize to one canonical key regardless of input
/// case, so the SNI/host_header lookup cannot race duplicates.
#[test]
fn parse_opts_host_header_canonical() {
    let cfg = parse_opts("mode=websocket;mux=false;header=host:a.com;header=Host:b.com").unwrap();
    assert_eq!(cfg.headers.len(), 1);
    assert_eq!(cfg.headers.get("Host").map(String::as_str), Some("b.com"));
    assert!(!cfg.headers.contains_key("host"));
}

#[cfg(feature = "mux")]
#[test]
fn mux_default_true_under_mux_feature() {
    let cfg = parse_opts("mode=websocket").unwrap();
    assert!(cfg.mux);
    assert!(!parse_opts("mode=websocket;mux=false").unwrap().mux);
}

/// Full transport loopback: gost `dial` → WebSocket upgrade → smux
/// session → echo.  The server side is a ws acceptor feeding a
/// frame-level smux echo — the same wire shape a gost server speaks.
#[cfg(feature = "mux")]
#[tokio::test]
async fn dial_ws_smux_echo_round_trip() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        let mut ws = ws;
        // smux echo: read a frame header+payload, echo PSH back.
        use futures::{SinkExt, StreamExt};
        let mut buf = Vec::new();
        while let Some(msg) = ws.next().await {
            let Ok(msg) = msg else { return };
            if !msg.is_binary() {
                continue;
            }
            buf.extend_from_slice(&msg.into_data());
            while buf.len() >= 8 {
                let len = u16::from_le_bytes([buf[2], buf[3]]) as usize;
                if buf.len() < 8 + len {
                    break;
                }
                let frame: Vec<u8> = buf.drain(..8 + len).collect();
                if frame[1] == 2 {
                    // CMD_PSH → echo payload back on the same stream id.
                    let mut out = Vec::with_capacity(8 + len);
                    out.extend_from_slice(&frame[..8]);
                    out.extend_from_slice(&frame[8..]);
                    ws.send(tokio_tungstenite::tungstenite::Message::Binary(out.into()))
                        .await
                        .unwrap();
                }
            }
        }
    });

    let cfg = parse_opts("mode=websocket;host=test.example;mux=true").unwrap();
    let ws = build_ws_layer(&cfg).unwrap();
    let dialer = crate::dialer::DirectDialer;
    let mut stream = dial(&cfg, None, &ws, "127.0.0.1", port, &dialer, false)
        .await
        .unwrap();
    stream.write_all(b"ping").await.unwrap();
    let mut buf = [0u8; 4];
    stream.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"ping");
    server.abort();
}

/// ws-only path (mux=false): raw WebSocket echo, no smux framing.
/// Ungated on purpose — this is the only dial path a `ss`-without-`mux`
/// build can exercise, so it must compile and run there too.
#[tokio::test]
async fn dial_ws_echo_round_trip() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        let mut ws = ws;
        use futures::{SinkExt, StreamExt};
        while let Some(msg) = ws.next().await {
            let Ok(msg) = msg else { return };
            if msg.is_binary() {
                ws.send(msg).await.unwrap();
            }
        }
    });

    let cfg = parse_opts("mode=websocket;host=test.example;mux=false").unwrap();
    let ws = build_ws_layer(&cfg).unwrap();
    let dialer = crate::dialer::DirectDialer;
    let mut stream = dial(&cfg, None, &ws, "127.0.0.1", port, &dialer, false)
        .await
        .unwrap();
    stream.write_all(b"ping").await.unwrap();
    let mut buf = [0u8; 4];
    stream.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"ping");
    server.abort();
}

/// A wss acceptor for the loopback tests: rcgen leaf → rustls → ws echo.
/// Returns (port, leaf sha256 pin, server task).
async fn start_wss_echo(cn: &str) -> (u16, String, tokio::task::JoinHandle<()>) {
    use sha2::Digest;

    let ck = rcgen::generate_simple_self_signed(vec![cn.into()]).unwrap();
    let pin: String = sha2::Sha256::digest(ck.cert.der())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let tls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![rustls::pki_types::CertificateDer::from(
                ck.cert.der().to_vec(),
            )],
            rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
                ck.key_pair.serialize_der(),
            )),
        )
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(tls_config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let tls = acceptor.accept(tcp).await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tls).await.unwrap();
        use futures::{SinkExt, StreamExt};
        while let Some(msg) = ws.next().await {
            let Ok(msg) = msg else { return };
            if msg.is_binary() {
                ws.send(msg).await.unwrap();
            }
        }
    });
    (port, pin, server)
}

/// The `tls=true` wire path: real TLS handshake + ws upgrade through the
/// `TlsLayer`, authenticated by `fingerprint` alone — a self-signed cert
/// with no `skip-cert-verify`, so a dropped pin-plumbing or TLS-skip
/// mutant cannot pass.
#[tokio::test]
async fn dial_wss_cert_pin_round_trip() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (port, pin, server) = start_wss_echo("test.example").await;
    let cfg = parse_opts(&format!(
        "mode=websocket;tls;mux=false;host=test.example;fingerprint={pin}"
    ))
    .unwrap();
    let tls = build_tls_layer(&cfg).unwrap();
    let ws = build_ws_layer(&cfg).unwrap();
    let dialer = crate::dialer::DirectDialer;
    let mut stream = dial(&cfg, tls.as_ref(), &ws, "127.0.0.1", port, &dialer, false)
        .await
        .unwrap();
    stream.write_all(b"ping").await.unwrap();
    let mut buf = [0u8; 4];
    stream.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"ping");
    server.abort();
}

/// A wrong pin must still reject under `skip-cert-verify` — the custom
/// verify callback overrides `VERIFY_NONE`, so the combination can never
/// silently downgrade to an unverified connection.
#[tokio::test]
async fn dial_wss_wrong_pin_rejected_despite_skip_cert_verify() {
    let (port, _pin, server) = start_wss_echo("test.example").await;
    let cfg = parse_opts(
        "mode=websocket;tls;mux=false;host=test.example;\
             skip-cert-verify=true;\
             fingerprint=0000000000000000000000000000000000000000000000000000000000000000",
    )
    .unwrap();
    let tls = build_tls_layer(&cfg).unwrap();
    let ws = build_ws_layer(&cfg).unwrap();
    let dialer = crate::dialer::DirectDialer;
    match dial(&cfg, tls.as_ref(), &ws, "127.0.0.1", port, &dialer, false).await {
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("TLS") || msg.contains("tls") || msg.contains("certificate"),
                "expected a TLS verification failure, got: {msg}"
            );
        }
        Ok(_) => panic!("a non-matching pin must reject the handshake"),
    }
    server.abort();
}

/// A `Host` entry in `headers` must win the wire Host header over `host`
/// (upstream: `config.Headers.Get("Host")` replaces `request.Host` and
/// drives SNI). Without it the request carries `host` verbatim.
#[tokio::test]
async fn dial_ws_host_header_overrides_host_opt() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn one_round(opts: &str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = tokio::sync::oneshot::channel::<String>();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut tx = Some(tx);
            let mut ws = tokio_tungstenite::accept_hdr_async(
                tcp,
                // tungstenite's `Callback` fixes the error type at
                // `Response<Option<String>>` — nothing to box.
                #[allow(
                    clippy::result_large_err,
                    reason = "tungstenite callback signature is fixed"
                )]
                move |req: &http::Request<()>,
                      resp: http::Response<()>|
                      -> std::result::Result<
                    http::Response<()>,
                    http::Response<Option<String>>,
                > {
                    if let Some(tx) = tx.take() {
                        let host = req
                            .headers()
                            .get(http::header::HOST)
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .to_string();
                        let _ = tx.send(host);
                    }
                    Ok(resp)
                },
            )
            .await
            .unwrap();
            use futures::{SinkExt, StreamExt};
            while let Some(msg) = ws.next().await {
                let Ok(msg) = msg else { return };
                if msg.is_binary() {
                    ws.send(msg).await.unwrap();
                }
            }
        });

        let cfg = parse_opts(opts).unwrap();
        let ws = build_ws_layer(&cfg).unwrap();
        let dialer = crate::dialer::DirectDialer;
        let mut stream = dial(&cfg, None, &ws, "127.0.0.1", port, &dialer, false)
            .await
            .unwrap();
        stream.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        stream.read_exact(&mut buf).await.unwrap();
        server.abort();
        rx.await.unwrap()
    }

    assert_eq!(
        one_round("mode=websocket;mux=false;host=test.example;header=Host:cdn.override").await,
        "cdn.override"
    );
    assert_eq!(
        one_round("mode=websocket;mux=false;host=test.example").await,
        "test.example"
    );
}

/// File-sourced `certificate`/`private-key` are re-stat per dial: a
/// change rebuilds the layer (new `Arc`), a corrupt rewrite keeps the
/// last-good pair (issue #621 — upstream fswatch parity).
#[test]
fn reloadable_tls_layer_reloads_changed_cert_files() {
    let dir = tempfile::tempdir().unwrap();
    let cert_path = dir.path().join("cert.pem");
    let key_path = dir.path().join("key.pem");
    let write_pair = |cn: &str, cert_pad: &str| {
        let ck = rcgen::generate_simple_self_signed(vec![cn.to_string()]).unwrap();
        std::fs::write(&cert_path, format!("{}{cert_pad}", ck.cert.pem())).unwrap();
        std::fs::write(&key_path, ck.key_pair.serialize_pem()).unwrap();
    };
    write_pair("one.example", "");

    let cfg = parse_opts(&format!(
        "mode=websocket;tls;mux=false;host=test.example;\
             certificate={};private-key={}",
        cert_path.display(),
        key_path.display()
    ))
    .unwrap();
    let src = cfg.client_cert_source.as_ref().unwrap();
    assert_eq!(src.cert_path.as_deref(), Some(cert_path.as_path()));
    assert_eq!(src.key_path.as_deref(), Some(key_path.as_path()));

    let holder = build_tls_layer(&cfg).unwrap().unwrap();
    let l1 = holder.current();
    assert!(
        Arc::ptr_eq(&l1, &holder.current()),
        "unchanged files must reuse the built layer"
    );

    // Valid rewrite: the trailing newline also changes the length, so
    // the stamp check trips even on coarse-granularity filesystems.
    write_pair("two.example", "\n");
    let l2 = holder.current();
    assert!(
        !Arc::ptr_eq(&l1, &l2),
        "a changed cert/key file must rebuild the layer"
    );

    // Corrupt rewrite: warn + keep the last-good layer; the failed
    // observation IS consumed so a durably-broken file doesn't cost a
    // rebuild per dial — the stamp covers ino/ctime, so any real fix
    // trips the detector and retries.
    std::fs::write(&key_path, b"definitely not a pem, and longer").unwrap();
    let l3 = holder.current();
    assert!(
        Arc::ptr_eq(&l2, &l3),
        "an invalid reload must keep the last-good layer"
    );
    // ...and while the file stays corrupt, later dials keep the
    // last-good layer without retrying.
    assert!(Arc::ptr_eq(&l3, &holder.current()));

    // Deleted file: last-good pair still serves (no panic, no error).
    std::fs::remove_file(&cert_path).unwrap();
    let l4 = holder.current();
    assert!(
        Arc::ptr_eq(&l3, &l4),
        "a missing file keeps the last-good layer"
    );
}

/// Inline PEM halves have no file source — `current()` is a pure
/// `Arc` clone and never reloads.
#[test]
fn reloadable_tls_layer_inline_pem_never_reloads() {
    let cfg = parse_opts(
        "mode=websocket;tls;mux=false;\
             certificate=-----BEGIN CERTIFICATE-----x;\
             private-key=-----BEGIN KEY-----k",
    )
    .unwrap();
    let src = cfg.client_cert_source.as_ref().unwrap();
    assert!(src.cert_path.is_none() && src.key_path.is_none());
    // `TlsLayer::new` rejects the bogus PEM at construction — the
    // parse-time validation, not the reload path, is what gates it.
    assert!(build_tls_layer(&cfg).is_err());
}
