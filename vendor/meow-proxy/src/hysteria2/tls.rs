//! quiche QUIC configuration for the hysteria2 client, built on BoringSSL.
//!
//! quiche links the same vendored BoringSSL as `meow-transport` (via the
//! `boring` crate), so this reuses `boring::ssl::SslContextBuilder` for the
//! server-certificate trust decision — a SHA-256 leaf pin, `insecure` (no
//! verification), or normal chain and hostname validation against the Mozilla
//! CA bundle — and hands the builder to quiche.

use super::{Config, Error, Result};
use boring::ssl::{SslContextBuilder, SslMethod, SslVerifyMode};
use boring::x509::X509StoreContextRef;
use sha2::{Digest, Sha256};
use std::time::Duration;

const ALPN_H3: &[u8] = b"h3";
const STREAM_RECEIVE_WINDOW: u64 = 8 * 1024 * 1024;
const CONN_RECEIVE_WINDOW: u64 = STREAM_RECEIVE_WINDOW * 5 / 2;
const MAX_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CONCURRENT_STREAMS: u64 = 1024;
const DGRAM_QUEUE_LEN: usize = 1024;

/// Build a client `quiche::Config` for the given hysteria2 config.
pub fn build_quiche_config(config: &Config) -> Result<quiche::Config> {
    let mut ssl = SslContextBuilder::new(SslMethod::tls())
        .map_err(|e| Error::tls(format!("boring SslContextBuilder: {e}")))?;

    let pin = parse_sha256_pin(&config.pin_sha256)?;
    match server_cert_verifier(config.insecure, pin) {
        CertVerifier::Pin(expected) => {
            ssl.set_verify_callback(SslVerifyMode::PEER, move |_preverify_ok, ctx| {
                leaf_matches_pin(ctx, &expected)
            });
        }
        CertVerifier::Insecure => ssl.set_verify(SslVerifyMode::NONE),
        CertVerifier::WebPki => {
            seed_roots(&mut ssl)?;
            ssl.set_verify(SslVerifyMode::PEER);
        }
    }

    // quiche installs the SNI as the verify-param hostname per connection and
    // only touches the verify mode through `verify_peer`, which the client
    // never calls, so the choice above is what BoringSSL enforces.
    let mut quic = quiche::Config::with_boring_ssl_ctx_builder(quiche::PROTOCOL_VERSION, ssl)
        .map_err(|e| Error::tls(format!("quiche config: {e}")))?;

    quic.set_application_protos(&[ALPN_H3])
        .map_err(|e| Error::tls(format!("quiche alpn: {e}")))?;
    quic.set_max_idle_timeout(u64::try_from(MAX_IDLE_TIMEOUT.as_millis()).unwrap_or(u64::MAX));
    quic.set_initial_max_data(CONN_RECEIVE_WINDOW);
    quic.set_initial_max_stream_data_bidi_local(STREAM_RECEIVE_WINDOW);
    quic.set_initial_max_stream_data_bidi_remote(STREAM_RECEIVE_WINDOW);
    quic.set_initial_max_stream_data_uni(STREAM_RECEIVE_WINDOW);
    quic.set_initial_max_streams_bidi(MAX_CONCURRENT_STREAMS);
    quic.set_initial_max_streams_uni(MAX_CONCURRENT_STREAMS);
    // hysteria2 disables the QUIC bit greasing.
    quic.grease(false);
    // UDP relay rides QUIC datagrams.
    quic.enable_dgram(true, DGRAM_QUEUE_LEN, DGRAM_QUEUE_LEN);

    Ok(quic)
}

/// How the server certificate is trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CertVerifier {
    /// Trust exactly the certificate whose DER SHA-256 digest matches.
    Pin([u8; 32]),
    /// `skip-cert-verify` without a pin: accept anything.
    Insecure,
    /// Chain and hostname validation against the bundled Mozilla roots.
    WebPki,
}

/// Pick the certificate verifier for a client config.
///
/// A configured fingerprint remains mandatory even when `insecure` is set.
///
/// A leaf pin replaces chain and hostname validation in meow-rs, matching
/// mihomo's leaf-fingerprint path and allowing self-signed certificates.
/// Hysteria's CLI differs: its pin is an additional leaf check, while its
/// `insecure` setting independently controls chain and hostname validation.
fn server_cert_verifier(insecure: bool, pin: Option<[u8; 32]>) -> CertVerifier {
    match (pin, insecure) {
        (Some(expected), _) => CertVerifier::Pin(expected),
        (None, true) => CertVerifier::Insecure,
        (None, false) => CertVerifier::WebPki,
    }
}

/// BoringSSL verify callback for a leaf pin (`fingerprint` / `pin-sha256`).
///
/// BoringSSL invokes the callback once per chain position and once per
/// verification error, passing its own verdict as `preverify_ok`. That verdict
/// is ignored: a pinned self-signed certificate fails chain building, and the
/// SNI need not match either. Positions above the leaf are accepted so
/// verification reaches depth 0, where only the end-entity digest decides.
/// Unrelated certificates appended to the chain therefore cannot satisfy the
/// pin. Possession of the private key is still proven: BoringSSL checks the
/// `CertificateVerify` signature independently of this callback.
fn leaf_matches_pin(ctx: &mut X509StoreContextRef, expected: &[u8; 32]) -> bool {
    if ctx.error_depth() != 0 {
        return true;
    }
    let leaf = ctx
        .chain()
        .and_then(|chain| chain.get(0))
        .or_else(|| ctx.current_cert());
    let Some(leaf) = leaf else {
        return false;
    };
    let Ok(der) = leaf.to_der() else {
        return false;
    };
    Sha256::digest(&der).as_slice() == expected
}

/// Seed the BoringSSL verify store with the Mozilla CA bundle (mirrors
/// `meow-transport`'s BoringSSL backend). The default store is empty.
fn seed_roots(ssl: &mut SslContextBuilder) -> Result<()> {
    let mut store = boring::x509::store::X509StoreBuilder::new()
        .map_err(|e| Error::tls(format!("X509StoreBuilder: {e}")))?;
    for cert in webpki_root_certs::TLS_SERVER_ROOT_CERTS {
        let x509 = boring::x509::X509::from_der(cert.as_ref())
            .map_err(|e| Error::tls(format!("root cert parse: {e}")))?;
        store
            .add_cert(x509)
            .map_err(|e| Error::tls(format!("root store add_cert: {e}")))?;
    }
    ssl.set_cert_store_builder(store);
    Ok(())
}

fn parse_sha256_pin(raw: &str) -> Result<Option<[u8; 32]>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    let without_prefix = raw
        .strip_prefix("sha256=")
        .or_else(|| raw.strip_prefix("SHA256="))
        .unwrap_or(raw);
    let normalized: String = without_prefix
        .chars()
        .filter(|c| !c.is_ascii_whitespace() && *c != ':')
        .flat_map(char::to_lowercase)
        .collect();
    if normalized.len() != 64 || !normalized.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Error::config(
            "pin-sha256/fingerprint must be a SHA-256 hex digest",
        ));
    }
    let decoded = hex::decode(normalized)
        .map_err(|e| Error::config(format!("invalid SHA-256 fingerprint: {e}")))?;
    let mut pin = [0u8; 32];
    pin.copy_from_slice(&decoded);
    Ok(Some(pin))
}

#[cfg(test)]
#[path = "../test/hysteria2_tls_test.rs"]
mod tests;
