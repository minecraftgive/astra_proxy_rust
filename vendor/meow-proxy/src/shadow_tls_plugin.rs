//! In-process `shadow-tls` SIP003 plugin for Shadowsocks (mihomo
//! `transport/sing-shadowtls` parity — the record layer lives in
//! [`meow_transport::shadow_tls`]).
//!
//! `plugin: shadow-tls` with `plugin-opts` keys:
//!
//! | opt | meaning | default |
//! |-----|---------|---------|
//! | `host` | cover server name — TLS SNI of the relayed handshake | *(required)* |
//! | `password` | per-user PSK (v2 handshake HMAC / v3 session-id tag) | `""` |
//! | `version` | protocol version 1, 2 or 3 | *(required)* |
//! | `alpn` | comma-separated ALPN list for the cover handshake | `h2,http/1.1` |
//! | `skip-cert-verify` | disable cover cert verification | `false` |
//! | `name-cert-verify` | cert-verify hostname when it differs from SNI — meow extension, upstream ignores this key | `host` |
//! | `fingerprint` | SHA-256 **certificate pin** (upstream
//!   `FingerprintVerifier`), *not* a uTLS profile | — |
//! | `certificate` / `private-key` | mTLS pair — inline PEM or file path | — |
//! | `strict-mode` | v3 only: refuse a cover that negotiates below TLS 1.3 (upstream `ClientConfig.StrictMode`) | `false` |
//!
//! The node-level `client-fingerprint` option shapes the cover
//! ClientHello (uTLS profile → `TlsConfig::fingerprint`), matching
//! upstream's `ShadowTLSOption.ClientFingerprint`.
//!
//! Version bounds on the cover handshake mirror upstream
//! (`MinVersion = TLS 1.2`, `v1` caps at TLS 1.2).  v3 keeps the 1.2
//! floor: the ClientHello still offers TLS 1.3 (so the compat session id
//! the HMAC patch needs is emitted), and a TLS 1.2 cover authorizes at
//! its ServerHello exactly as upstream (`authorized = !isTLS13`).

use meow_common::error::{MeowError, Result};
use meow_transport::{
    shadow_tls,
    tls::{ClientCert, TlsConfig, TlsLayer, TlsVersion},
    Stream,
};
use tracing::{debug, warn};

use crate::plugin_util::{load_pem_or_path, parse_bool_strict, parse_cert_pin, sip003_opts};
use crate::transport_to_proxy_err;

const PLUGIN: &str = "shadow-tls";

/// Parsed `shadow-tls` client options.
#[derive(Debug, Clone)]
pub struct ShadowTlsConfig {
    /// Cover server name — the TLS handshake's SNI; the shadow-tls server
    /// relays the handshake to this host.
    pub host: String,
    /// Per-user PSK (ignored by v1, matching upstream).
    pub password: String,
    /// Protocol version 1, 2 or 3 — required; upstream `obfs:"version"`
    /// has no default (absent → 0 → `NewClient` errors).
    pub version: u8,
    /// ALPN for the cover handshake (upstream default `h2,http/1.1`).
    pub alpn: Vec<String>,
    pub skip_cert_verify: bool,
    /// Cert-verify hostname when it differs from `host`/SNI
    /// (`name-cert-verify` — a meow extension; mihomo's
    /// `shadowTLSOption` has no such field and warn-ignores it).
    pub verify_name: Option<String>,
    /// SHA-256 certificate pin (`fingerprint` — SSL pinning per upstream
    /// `ca.NewFingerprintVerifier`, *not* a uTLS ClientHello profile).
    pub cert_pin: Option<[u8; 32]>,
    /// mTLS client certificate (`certificate` + `private-key`, PEM or path).
    pub client_cert: Option<ClientCert>,
    /// `strict-mode` — v3 only: refuse a cover that negotiates below
    /// TLS 1.3 (upstream `ClientConfig.StrictMode`; v1/v2 ignore it,
    /// matching upstream where the check lives in the v3 arm).
    pub strict_mode: bool,
}

/// Parse a flattened SIP003 opts string for `shadow-tls`
/// (`host=cover.example.com;password=…;version=3;alpn=h2,http/1.1`).
///
/// - `host` is required (upstream `obfs:"host"` has no `omitempty`).
/// - `version` accepts `1`/`2`/`3`; anything else is a config error.
/// - Unknown keys are logged at `warn` level and ignored.
pub fn parse_opts(s: &str) -> Result<ShadowTlsConfig> {
    let mut cfg = ShadowTlsConfig {
        host: String::new(),
        password: String::new(),
        version: 0,
        alpn: Vec::new(),
        skip_cert_verify: false,
        verify_name: None,
        cert_pin: None,
        client_cert: None,
        strict_mode: false,
    };
    let mut version_seen = false;
    let mut alpn_seen = false;
    let mut cert_pem: Option<Vec<u8>> = None;
    let mut key_pem: Option<Vec<u8>> = None;

    for (key, value) in sip003_opts(s) {
        match key.as_str() {
            "host" => cfg.host = value,
            "password" => cfg.password = value,
            "version" => {
                version_seen = true;
                cfg.version = value
                    .parse::<u8>()
                    .ok()
                    .filter(|v| (1..=3).contains(v))
                    .ok_or_else(|| {
                        MeowError::Config(format!(
                            "{PLUGIN}: 'version' must be 1, 2 or 3, got '{value}'"
                        ))
                    })?;
            }
            "alpn" => {
                alpn_seen = true;
                cfg.alpn = value
                    .split(',')
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .map(str::to_string)
                    .collect();
            }
            "skip-cert-verify" => {
                cfg.skip_cert_verify = parse_bool_strict(&value, PLUGIN, "skip-cert-verify")?;
            }
            "name-cert-verify" if !value.is_empty() => cfg.verify_name = Some(value),
            "fingerprint" if !value.is_empty() => {
                cfg.cert_pin = Some(parse_cert_pin(&value, PLUGIN)?);
            }
            // Explicit-empty value clears the opt — same as absent.
            "name-cert-verify" | "fingerprint" => {}
            "strict-mode" => {
                cfg.strict_mode = parse_bool_strict(&value, PLUGIN, "strict-mode")?;
            }
            "certificate" => cert_pem = Some(load_pem_or_path(&value, "certificate", PLUGIN)?),
            "private-key" => key_pem = Some(load_pem_or_path(&value, "private-key", PLUGIN)?),
            other => warn!("{PLUGIN}: ignoring unknown opt '{other}'"),
        }
    }

    if cfg.host.is_empty() {
        return Err(MeowError::Config(format!(
            "{PLUGIN}: missing required 'host' opt (cover server name)"
        )));
    }
    // Upstream has no version default — absent maps to 0 and NewClient
    // rejects it.  Fail loud the same way rather than silently picking
    // a wire protocol the operator did not ask for.
    if !version_seen {
        return Err(MeowError::Config(format!(
            "{PLUGIN}: missing required 'version' opt (1, 2 or 3)"
        )));
    }
    // Absent `alpn` → upstream DefaultALPN; `alpn=` (explicit empty) is
    // honored as no-ALPN, matching upstream's `opt.ALPN != nil` check.
    if !alpn_seen {
        cfg.alpn = shadow_tls::DEFAULT_ALPN
            .iter()
            .map(ToString::to_string)
            .collect();
    }
    // mTLS: both halves or none — a lone cert or key is a config error.
    match (cert_pem, key_pem) {
        (Some(cert_pem), Some(key_pem)) => {
            cfg.client_cert = Some(ClientCert { cert_pem, key_pem });
        }
        (None, None) => {}
        _ => {
            return Err(MeowError::Config(format!(
                "{PLUGIN}: 'certificate' and 'private-key' must both be set"
            )));
        }
    }
    if cfg.version != 1 && cfg.password.is_empty() {
        warn!(
            "{PLUGIN}: empty password — v{} authentication will fail",
            cfg.version
        );
    }
    Ok(cfg)
}

/// Build the cover-handshake `TlsLayer`.  `client_fingerprint` is the
/// node-level `client-fingerprint` option (uTLS profile shaping).
///
/// Version bounds mirror upstream: TLS 1.2 floor for every version, TLS
/// 1.2 cap for v1.  v3 needs no 1.3 floor — the compat session id is
/// emitted whenever 1.3 is *offered* (the floor does not remove it), and
/// a TLS 1.2 cover still authorizes the session at its ServerHello.
/// Upstream also drops `client-fingerprint` when the cap is 1.2
/// (`uTLSHandshakeFunc` forces `""`) — a 1.2-only ClientHello should not
/// carry a 1.3-shaped profile.
pub fn build_tls_layer(
    cfg: &ShadowTlsConfig,
    client_fingerprint: Option<&str>,
) -> Result<TlsLayer> {
    if cfg.version == 3
        && (cfg.cert_pin.is_some() || cfg.verify_name.is_some() || cfg.skip_cert_verify)
    {
        warn!(
            "{PLUGIN}: 'fingerprint'/'name-cert-verify'/'skip-cert-verify' only \
             take effect on a TLS 1.2 cover — on a 1.3 cover the post-ServerHello \
             flight is undecryptable, so the embedded record tags are the only \
             authenticator"
        );
    }
    let tls_config = tls_config_for(cfg, client_fingerprint);
    TlsLayer::new(&tls_config).map_err(transport_to_proxy_err)
}

/// Build the [`TlsConfig`] for the cover handshake. Split out from
/// [`build_tls_layer`] so tests can pin the per-version shaping matrix
/// (v1 fingerprint drop, v1 max-version cap, v2 ML-KEM strip).
fn tls_config_for(cfg: &ShadowTlsConfig, client_fingerprint: Option<&str>) -> TlsConfig {
    let fingerprint = client_fingerprint
        .filter(|_| cfg.version != 1)
        .map(str::to_string);
    TlsConfig {
        alpn: cfg.alpn.clone(),
        skip_cert_verify: cfg.skip_cert_verify,
        verify_name: cfg.verify_name.clone(),
        cert_pin: cfg.cert_pin,
        client_cert: cfg.client_cert.clone(),
        // Upstream removes X25519MLKEM768 from the v2 ClientHello after
        // uTLS shaping (`BuildRemovedX25519MLKEM768HandshakeState` — a
        // hybrid-PQ keyshare breaks v2 servers). The strip is surgical:
        // it must not clobber a resolved profile's own group list
        // (firefox advertises P-521, android orders P-256 first), and
        // every shipped profile already excludes ML-KEM — so the classic
        // pin is needed only when no profile shapes the hello and the
        // vendored BoringSSL default (which does offer ML-KEM) applies.
        curves: (cfg.version == 2
            && client_fingerprint
                .is_none_or(|fp| !meow_transport::tls::is_supported_fingerprint(fp)))
        .then(|| "X25519:P-256:P-384".to_string()),
        fingerprint,
        min_version: Some(TlsVersion::Tls12),
        max_version: (cfg.version == 1).then_some(TlsVersion::Tls12),
        ..TlsConfig::new(&cfg.host)
    }
}

/// Dial a TCP + shadow-tls connection to `server_host:server_port` and
/// return the framed stream ready to be wrapped by the SS layer.
pub async fn dial(
    cfg: &ShadowTlsConfig,
    tls: &TlsLayer,
    server_host: &str,
    server_port: u16,
    dialer: &dyn crate::dialer::TcpDialer,
    internal: bool,
) -> Result<Box<dyn Stream>> {
    debug!(
        "{PLUGIN}: dialing {server_host}:{server_port} via cover={} version={}",
        cfg.host, cfg.version
    );
    let tcp = dialer
        .dial(server_host, server_port, internal)
        .await
        .map_err(MeowError::Io)?;
    shadow_tls::dial(
        Box::new(tcp),
        tls,
        cfg.version,
        cfg.password.as_bytes(),
        cfg.strict_mode,
    )
    .await
    .map_err(transport_to_proxy_err)
}

#[cfg(test)]
#[path = "test/shadow_tls_plugin_test.rs"]
mod tests;
