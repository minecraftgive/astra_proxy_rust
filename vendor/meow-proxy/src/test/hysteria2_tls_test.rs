use super::*;
use boring::pkey::{PKey, Private};
use boring::x509::X509;
use tokio::net::UdpSocket;

#[test]
fn parses_sha256_pin_variants() {
    let raw = "sha256=AA:BB cc";
    let mut padded = String::from(raw);
    padded.push_str(&"00".repeat(29));
    let pin = parse_sha256_pin(&padded).unwrap().unwrap();
    assert_eq!(pin[0], 0xaa);
    assert_eq!(pin[1], 0xbb);
    assert_eq!(pin[2], 0xcc);
}

#[test]
fn rejects_invalid_sha256_pin() {
    assert!(parse_sha256_pin("abc").is_err());
}

#[test]
fn builds_config_for_insecure() {
    let cfg = Config {
        insecure: true,
        ..Config::default()
    };
    assert!(build_quiche_config(&cfg).is_ok());
}

/// `skip-cert-verify: true` alongside a `fingerprint` used to win, quietly
/// turning a pinned config into an unauthenticated one. The stricter
/// setting must survive (mihomo applies the pin last, for the same reason).
#[test]
fn a_pin_overrides_skip_cert_verify() {
    assert_eq!(
        server_cert_verifier(true, Some([0x22; 32])),
        CertVerifier::Pin([0x22; 32])
    );
    assert_eq!(
        server_cert_verifier(false, Some([0x22; 32])),
        CertVerifier::Pin([0x22; 32])
    );
}

#[test]
fn skip_cert_verify_without_a_pin_accepts_anything() {
    assert_eq!(server_cert_verifier(true, None), CertVerifier::Insecure);
}

#[test]
fn plain_config_keeps_the_webpki_roots() {
    assert_eq!(server_cert_verifier(false, None), CertVerifier::WebPki);
}

/// A self-signed certificate and its `fingerprint`, as a hysteria2 server
/// set up by the upstream install script would present.
struct SelfSigned {
    cert: X509,
    key: PKey<Private>,
    pin: String,
}

fn self_signed(name: &str) -> SelfSigned {
    let ck = rcgen::generate_simple_self_signed(vec![name.into()]).unwrap();
    SelfSigned {
        cert: X509::from_der(ck.cert.der()).unwrap(),
        key: PKey::private_key_from_der(&ck.key_pair.serialize_der()).unwrap(),
        pin: hex::encode(Sha256::digest(ck.cert.der())),
    }
}

/// Server TLS context presenting `leaf`, optionally with an unrelated
/// certificate appended to the chain.
fn server_ssl(leaf: &SelfSigned, appended: Option<&SelfSigned>) -> SslContextBuilder {
    let mut ssl = SslContextBuilder::new(SslMethod::tls()).unwrap();
    ssl.set_certificate(&leaf.cert).unwrap();
    ssl.set_private_key(&leaf.key).unwrap();
    if let Some(extra) = appended {
        ssl.add_extra_chain_cert(extra.cert.clone()).unwrap();
    }
    ssl
}

fn client_config(server_name: &str, insecure: bool, pin: &str) -> Config {
    Config {
        server_name: server_name.into(),
        insecure,
        pin_sha256: pin.into(),
        ..Config::default()
    }
}

async fn flush(
    conn: &mut quiche::Connection,
    socket: &UdpSocket,
) -> std::result::Result<(), String> {
    let mut out = [0u8; 1500];
    loop {
        match conn.send(&mut out) {
            Ok((n, info)) => {
                socket
                    .send_to(&out[..n], info.to)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Err(quiche::Error::Done) => return Ok(()),
            Err(e) => return Err(format!("send: {e}")),
        }
    }
}

/// Drive a real QUIC handshake between the production client config and a
/// local quiche server presenting `server_ssl`'s certificate. Returns the
/// client's verdict: the whole BoringSSL path (certificate *and*
/// `CertificateVerify` signature), not just the digest comparison.
async fn handshake(
    client_cfg: &Config,
    server_ssl: SslContextBuilder,
) -> std::result::Result<(), String> {
    tokio::time::timeout(
        Duration::from_secs(10),
        handshake_inner(client_cfg, server_ssl),
    )
    .await
    .map_err(|_| "local QUIC handshake must finish".to_string())?
}

async fn handshake_inner(
    client_cfg: &Config,
    server_ssl: SslContextBuilder,
) -> std::result::Result<(), String> {
    let mut server_config =
        quiche::Config::with_boring_ssl_ctx_builder(quiche::PROTOCOL_VERSION, server_ssl)
            .map_err(|e| e.to_string())?;
    server_config.set_application_protos(&[ALPN_H3]).unwrap();
    server_config.set_max_idle_timeout(5_000);
    server_config.set_initial_max_data(1 << 20);
    server_config.set_initial_max_streams_bidi(16);
    server_config.set_initial_max_streams_uni(16);
    server_config.set_initial_max_stream_data_bidi_remote(64 * 1024);
    server_config.set_initial_max_stream_data_uni(64 * 1024);

    let server_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let client_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let server_addr = server_sock.local_addr().unwrap();
    let client_addr = client_sock.local_addr().unwrap();

    let mut client_config = build_quiche_config(client_cfg).map_err(|e| e.to_string())?;
    let scid = quiche::ConnectionId::from_ref(&[7u8; quiche::MAX_CONN_ID_LEN]);
    let mut client = quiche::connect(
        Some(&client_cfg.server_name),
        &scid,
        client_addr,
        server_addr,
        &mut client_config,
    )
    .map_err(|e| e.to_string())?;
    let mut server: Option<quiche::Connection> = None;
    let mut sbuf = [0u8; 65535];
    let mut cbuf = [0u8; 65535];

    loop {
        flush(&mut client, &client_sock).await?;
        if let Some(conn) = server.as_mut() {
            flush(conn, &server_sock).await?;
        }
        if client.is_established() {
            return Ok(());
        }
        if client.is_closed() || client.local_error().is_some() || client.peer_error().is_some() {
            return Err(format!(
                "client closed: local={:?} peer={:?}",
                client.local_error(),
                client.peer_error()
            ));
        }
        let wait = client
            .timeout()
            .into_iter()
            .chain(server.as_ref().and_then(quiche::Connection::timeout))
            .min()
            .unwrap_or(Duration::from_millis(100));
        tokio::select! {
            r = server_sock.recv_from(&mut sbuf) => {
                let (n, from) = r.map_err(|e| e.to_string())?;
                if server.is_none() {
                    let hdr = quiche::Header::from_slice(&mut sbuf[..n], quiche::MAX_CONN_ID_LEN)
                        .map_err(|e| e.to_string())?;
                    let conn = quiche::accept(&hdr.dcid, None, server_addr, from, &mut server_config)
                        .map_err(|e| e.to_string())?;
                    server = Some(conn);
                }
                let conn = server.as_mut().unwrap();
                let _ = conn.recv(&mut sbuf[..n], quiche::RecvInfo { from, to: server_addr });
            }
            r = client_sock.recv_from(&mut cbuf) => {
                let (n, from) = r.map_err(|e| e.to_string())?;
                // A rejected certificate surfaces here as `TlsFail` and
                // sets the client's local error.
                let _ = client.recv(&mut cbuf[..n], quiche::RecvInfo { from, to: client_addr });
            }
            () = tokio::time::sleep(wait) => {
                client.on_timeout();
                if let Some(conn) = server.as_mut() {
                    conn.on_timeout();
                }
            }
        }
    }
}

/// The point of pinning: a self-signed server plus the matching
/// `fingerprint` completes a handshake whatever `skip-cert-verify` says, a
/// wrong fingerprint aborts it even with `skip-cert-verify`, and without a
/// pin the two insecure settings keep their usual meaning.
#[tokio::test]
async fn production_quic_config_pin_matrix() {
    let server = self_signed("hy2.example");
    for (insecure, pin, expected) in [
        (false, server.pin.clone(), true),
        (true, server.pin.clone(), true),
        (false, "11".repeat(32), false),
        (true, "11".repeat(32), false),
        (false, String::new(), false),
        (true, String::new(), true),
    ] {
        let result = handshake(
            &client_config("hy2.example", insecure, &pin),
            server_ssl(&server, None),
        )
        .await;
        assert_eq!(
            result.is_ok(),
            expected,
            "insecure={insecure}, pin={}, result={result:?}",
            !pin.is_empty()
        );
        if let Err(e) = &result {
            assert!(is_tls_alert(e), "expected a TLS alert, got {e}");
        }
    }
}

/// A refused certificate closes the connection with a CRYPTO_ERROR
/// (0x0100 + the TLS alert), not an application or transport error.
fn is_tls_alert(err: &str) -> bool {
    err.split("error_code: ")
        .nth(1)
        .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|code| code.parse::<u64>().ok())
        .is_some_and(|code| (0x100..0x200).contains(&code))
}

/// The pin is the trust decision: it also replaces hostname validation,
/// as in mihomo's leaf-fingerprint path.
#[tokio::test]
async fn a_pin_replaces_hostname_validation() {
    let server = self_signed("hy2.example");
    handshake(
        &client_config("other.example", false, &server.pin),
        server_ssl(&server, None),
    )
    .await
    .expect("a pinned certificate is trusted regardless of the SNI");
}

/// Only the end-entity certificate is compared: an unrelated certificate
/// appended to the chain cannot satisfy the pin.
#[tokio::test]
async fn appending_pinned_certificate_does_not_authenticate_another_leaf() {
    let pinned = self_signed("hy2.example");
    let other = self_signed("hy2.example");
    let result = handshake(
        &client_config("hy2.example", true, &pinned.pin),
        server_ssl(&other, Some(&pinned)),
    )
    .await;
    let err = result.expect_err("an unrelated leaf must be rejected");
    assert!(is_tls_alert(&err), "expected a TLS alert, got {err}");
}
