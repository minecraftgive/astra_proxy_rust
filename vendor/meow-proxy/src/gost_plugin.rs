//! Built-in `gost-plugin` SIP003 client transport (issue #533).
//!
//! Implements the WebSocket (+ optional TLS, optional smux) transport that
//! mihomo runs in-process for `plugin: gost-plugin`, natively in Rust —
//! no external `gost-plugin` binary is spawned.
//!
//! Upstream: `adapter/outbound/shadowsocks.go` (`gostObfsOption`) and
//! `transport/gost/websocket.go` (`NewGostWebsocket`).  Wire behaviour per
//! connection: TCP → optional TLS (ALPN `http/1.1`, SNI from `host`, or the
//! `Host` header when present) → WebSocket upgrade → when `mux` (upstream
//! default **true**) a fresh smux v1 session carries one stream.
//!
//! Option mapping (upstream `plugin-opts` map → flattened SIP003 tokens by
//! `meow-config::serialize_plugin_opts`):
//!
//! | upstream key          | flattened token      | notes                        |
//! |-----------------------|----------------------|------------------------------|
//! | `mode`                | `mode`               | required, must be `websocket`|
//! | `host`                | `host`               | default `bing.com`           |
//! | `path`                | `path`               | default `/`                  |
//! | `tls`                 | `tls`                | bool                         |
//! | `mux`                 | `mux`                | bool, default `true`         |
//! | `headers` map         | repeated `header=K:V`| Host entry also sets TLS SNI |
//! | `skip-cert-verify`    | `skip-cert-verify`   |                              |
//! | `name-cert-verify`    | `name-cert-verify`   | cert check name ≠ SNI        |
//! | `fingerprint`         | `fingerprint`        | SHA-256 cert pin (not uTLS)  |
//! | `certificate`         | `certificate`        | PEM cert or file path (mTLS) |
//! | `private-key`         | `private-key`        | PEM key or file path         |
//! | `ech-opts.enable`     | `ech-opts.enable`    | bool                         |
//! | `ech-opts.config`     | `ech-opts.config`    | base64 ECHConfigList         |
//!
//! Cert/key reload: upstream's `NewTLSKeyPairLoader` reloads
//! `certificate`/`private-key` files on fswatch events. We poll instead —
//! file-sourced PEMs are re-stat on every dial and the TLS layer rebuilt
//! when the (mtime, len, inode, ctime) stamp changed
//! (`ReloadableTlsLayer`); a failed reload keeps the last known-good pair
//! and retries next dial. Inline PEMs are immutable.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use meow_common::{MeowError, Result};
use meow_transport::{
    tls::{ClientCert, EchOpts, TlsConfig, TlsLayer},
    ws::{WsConfig, WsLayer},
    Transport,
};
use tracing::{debug, warn};

use crate::plugin_util::{
    load_pem_or_path, parse_bool_strict, parse_cert_pin, pem_source, read_cert_file, sip003_opts,
};
use crate::transport_to_proxy_err;

const PLUGIN: &str = "gost-plugin";

/// Parsed `gost-plugin` client options.
#[derive(Debug, Clone)]
pub struct GostPluginConfig {
    /// Host header and TLS SNI.  Upstream default: `bing.com` (the dial
    /// still goes to the SS server; `host` is only the camouflage name).
    pub host: String,
    /// WebSocket upgrade path.
    pub path: String,
    /// Wrap the WebSocket in TLS.
    pub tls: bool,
    /// Multiplex each connection through a fresh smux v1 session.
    /// Upstream defaults to `true`.
    pub mux: bool,
    /// Extra WebSocket request headers (`headers` map upstream).
    pub headers: HashMap<String, String>,
    pub skip_cert_verify: bool,
    /// Certificate-verification hostname override (`name-cert-verify`).
    pub name_cert_verify: Option<String>,
    /// SHA-256 certificate pin (`fingerprint` — SSL pinning per upstream
    /// `ca.NewFingerprintVerifier`, *not* a uTLS ClientHello profile;
    /// upstream reserves `client-fingerprint` for uTLS).
    pub cert_pin: Option<[u8; 32]>,
    /// mTLS client certificate (`certificate` + `private-key`, PEM).
    pub client_cert: Option<ClientCert>,
    /// Where `client_cert`'s halves came from — file-sourced sides are
    /// re-stat per dial and hot-reloaded (upstream fswatch parity).
    pub client_cert_source: Option<ClientCertSource>,
    /// ECH config (`ech-opts.enable` + base64 `ech-opts.config`).
    pub ech: Option<EchOpts>,
}

/// Where the mTLS `certificate`/`private-key` PEMs came from. A `None`
/// side is inline PEM and never changes; a `Some` side is a resolved
/// filesystem path polled for changes (issue #621).
#[derive(Debug, Clone, Default)]
pub struct ClientCertSource {
    /// Resolved path when `certificate` named a file.
    pub cert_path: Option<PathBuf>,
    /// Resolved path when `private-key` named a file.
    pub key_path: Option<PathBuf>,
}

/// Parse a flattened SIP003 opts string for `gost-plugin`
/// (`mode=websocket;tls;host=example.com;mux=true;header=K:V`).
///
/// - Bare keys (e.g. `tls`) are treated as `key=true`.
/// - `mode` is required and must be `websocket` (upstream rejects any
///   other value with `obfs mode error`).
/// - Unknown keys are logged at `warn` level and ignored.
pub fn parse_opts(s: &str) -> Result<GostPluginConfig> {
    // Upstream seeds `gostObfsOption{Host: "bing.com", Mux: true}` before
    // decoding — empty plugin-opts still produces host=bing.com, mux=true.
    let mut cfg = GostPluginConfig {
        host: "bing.com".to_string(),
        path: "/".to_string(),
        tls: false,
        mux: true,
        headers: HashMap::new(),
        skip_cert_verify: false,
        name_cert_verify: None,
        cert_pin: None,
        client_cert: None,
        client_cert_source: None,
        ech: None,
    };
    let mut mode_seen = false;
    let mut cert_pem: Option<Vec<u8>> = None;
    let mut key_pem: Option<Vec<u8>> = None;
    let mut cert_src = None;
    let mut key_src = None;
    let mut ech_enable = false;
    let mut ech_config: Option<String> = None;

    for (key, value) in sip003_opts(s) {
        match key.as_str() {
            "mode" => {
                if value.eq_ignore_ascii_case("websocket") || value.eq_ignore_ascii_case("ws") {
                    mode_seen = true;
                } else {
                    return Err(MeowError::Config(format!(
                        "{PLUGIN}: unsupported mode '{value}' (only 'websocket'/'ws' is supported)"
                    )));
                }
            }
            "host" => {
                if value.is_empty() {
                    return Err(MeowError::Config(format!(
                        "{PLUGIN}: 'host' must not be empty"
                    )));
                }
                cfg.host = value;
            }
            // Go's URL writer normalizes an empty request path to `/`.
            // A `path` without a leading `/` is malformed as a request
            // target — prepend it (upstream accepts it because the value
            // lands in `http.Request.URL.Path` which re-escapes; our WsLayer
            // composes the request line from the raw string).
            "path" => {
                if value.chars().any(|c| c.is_ascii_control() || c == ' ') {
                    return Err(MeowError::Config(format!(
                        "{PLUGIN}: 'path' contains an invalid character: {value:?}"
                    )));
                }
                cfg.path = if value.is_empty() {
                    "/".into()
                } else if value.starts_with('/') {
                    value
                } else {
                    format!("/{value}")
                };
            }
            "tls" => cfg.tls = parse_bool_strict(&value, PLUGIN, "tls")?,
            "mux" => cfg.mux = parse_bool_strict(&value, PLUGIN, "mux")?,
            "skip-cert-verify" => {
                cfg.skip_cert_verify = parse_bool_strict(&value, PLUGIN, "skip-cert-verify")?;
            }
            // Upstream guards `NameCertVerify != ""` — an empty value is
            // ignored, not an (always-failing) empty verify name.
            "name-cert-verify" if !value.is_empty() => {
                cfg.name_cert_verify = Some(value);
            }
            // Upstream `fingerprint` is a SHA-256 certificate pin, not a
            // uTLS profile — `NewFingerprintVerifier` rejects the uTLS
            // names explicitly and hex-decodes the rest. Empty = ignored.
            "fingerprint" if !value.is_empty() => {
                cfg.cert_pin = Some(parse_cert_pin(&value, PLUGIN)?);
            }
            "name-cert-verify" | "fingerprint" => {}
            // Upstream `NewTLSKeyPairLoader` accepts inline PEM or file
            // paths; file paths are kept for per-dial reload.
            "certificate" => {
                cert_src = pem_source(&value).into_path();
                cert_pem = Some(load_pem_or_path(&value, "certificate", PLUGIN)?);
            }
            "private-key" => {
                key_src = pem_source(&value).into_path();
                key_pem = Some(load_pem_or_path(&value, "private-key", PLUGIN)?);
            }
            "ech-opts.enable" | "ech-enable" => {
                ech_enable = parse_bool_strict(&value, PLUGIN, "ech-opts.enable")?;
            }
            "ech-opts.config" | "ech-config" => ech_config = Some(value),
            "header" => {
                // Form: header=Key:Value (SIP003 convention shared with
                // v2ray-plugin). A `Host` entry doubles as the ws request
                // Host and TLS SNI; keep it under one canonical case so
                // `Host`/`host` duplicates cannot race in the map.
                if let Some((k, v)) = value.split_once(':') {
                    let k = if k.trim().eq_ignore_ascii_case("host") {
                        "Host"
                    } else {
                        k.trim()
                    };
                    // Same bound as v2ray-plugin / xhttp `headers` —
                    // provider opts repeat `header=` and the map is
                    // re-serialized per dial (issue #648).
                    if cfg.headers.len() >= meow_transport::MAX_EXTRA_HEADERS
                        && !cfg.headers.contains_key(k)
                    {
                        return Err(MeowError::Config(format!(
                            "{PLUGIN}: more than {} header= entries",
                            meow_transport::MAX_EXTRA_HEADERS
                        )));
                    }
                    cfg.headers.insert(k.to_string(), v.trim().to_string());
                } else {
                    // The value may be a credential — log shape, not content.
                    warn!("{PLUGIN}: ignoring malformed header entry (expected 'Key:Value')");
                }
            }
            other => {
                warn!("{PLUGIN}: ignoring unknown opt '{}'", other);
            }
        }
    }

    if !mode_seen {
        return Err(MeowError::Config(format!(
            "{PLUGIN}: missing required 'mode=websocket' opt"
        )));
    }

    if cfg.mux && !cfg!(feature = "mux") {
        return Err(MeowError::Config(format!(
            "{PLUGIN}: mux=true (the upstream default) needs the `mux` \
             cargo feature for smux — rebuild with it or set `mux: false`"
        )));
    }

    // mTLS: both halves or none — a lone cert or key is a config error.
    match (cert_pem, key_pem) {
        (Some(cert_pem), Some(key_pem)) => {
            cfg.client_cert = Some(ClientCert { cert_pem, key_pem });
            cfg.client_cert_source = Some(ClientCertSource {
                cert_path: cert_src,
                key_path: key_src,
            });
        }
        (None, None) => {}
        _ => {
            return Err(MeowError::Config(format!(
                "{PLUGIN}: 'certificate' and 'private-key' must both be set"
            )));
        }
    }

    if ech_enable {
        match ech_config {
            Some(b64) => {
                use base64::Engine;
                let list = base64::engine::general_purpose::STANDARD
                    .decode(&b64)
                    .map_err(|e| {
                        MeowError::Config(format!(
                            "{PLUGIN}: base64 decode ech-opts.config failed: {e}"
                        ))
                    })?;
                cfg.ech = Some(EchOpts::Config(list));
            }
            None => {
                // Upstream falls back to a DNS HTTPS-record query; that
                // resolver path is not implemented — declare it rather than
                // silently running without ECH.
                return Err(MeowError::Config(
                    "{PLUGIN}: ech-opts.enable without ech-opts.config \
                     (DNS-queried ECH is not supported)"
                        .into(),
                ));
            }
        }
    }

    Ok(cfg)
}

/// A `TlsLayer` that hot-reloads file-sourced `certificate`/`private-key`
/// PEMs — poll-based parity with upstream's `NewTLSKeyPairLoader` fswatch.
/// Each dial re-stats the source files; when the stamp (mtime, length,
/// inode, ctime) changed, the pair is re-read and the layer rebuilt. A
/// failed reload keeps the last known-good pair, warns, and retries on
/// the next dial — a half-written or invalid file must not break the
/// transport (issue #621). Inline PEMs never reload.
pub struct ReloadableTlsLayer {
    /// Config template — `client_cert` is swapped for the fresh pair on
    /// each successful reload; every other field is fixed.
    tls_config: TlsConfig,
    source: Option<ClientCertSource>,
    state: parking_lot::Mutex<TlsReloadState>,
}

struct TlsReloadState {
    layer: Arc<TlsLayer>,
    /// Last-good PEM pair — the reload source for inline sides.
    cert: ClientCert,
    /// Last observed stamp per file-sourced side; `None` when the side
    /// is inline or its file was missing/non-regular at last check.
    /// Stamps are consumed even on reload failure — every real fix path
    /// (rewrite, chmod, rename-replace, symlink repoint) changes the
    /// stamp, so a durably-broken file doesn't cost a read + rebuild +
    /// warn per dial while still self-healing on the next change
    /// (issue #621 review).
    cert_stamp: Option<FileStamp>,
    key_stamp: Option<FileStamp>,
}

/// Change-detection stamp for a cert/key file. `mtime`/`len` alone miss
/// `cp -p` rewrites (preserved timestamps) and chmod-only repairs —
/// inode + ctime (unix) catch those, since ctime bumps on every content
/// or metadata change and can't be forged via `utimensat` (issue #621
/// review).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileStamp {
    mtime: SystemTime,
    len: u64,
    /// (ino, ctime, ctime_nsec) on unix; zeros elsewhere — non-unix
    /// detection is mtime+len only, so a chmod-only fix won't trip it
    /// there (accepted: cert files are a unix-ops feature in practice).
    ext: (u64, i64, i64),
}

/// The current stamp of a file — `None` when it can't be stat'ed **or
/// isn't a regular file**: a FIFO/device read would block indefinitely
/// while holding the reload mutex, wedging every later dial (issue #621
/// review — a subscription-supplied path can name anything).
fn file_stamp(path: &std::path::Path) -> Option<FileStamp> {
    let md = std::fs::metadata(path).ok()?;
    if !md.is_file() {
        return None;
    }
    #[cfg(unix)]
    let ext = {
        use std::os::unix::fs::MetadataExt;
        (md.ino(), md.ctime(), md.ctime_nsec())
    };
    #[cfg(not(unix))]
    let ext = (0, 0, 0);
    Some(FileStamp {
        mtime: md.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        len: md.len(),
        ext,
    })
}

impl ReloadableTlsLayer {
    /// The layer to dial with, reloading cert/key files that changed since
    /// the last build. Cheap: two `stat`s per dial for file-sourced PEMs,
    /// a mutex `Arc` clone otherwise.
    pub fn current(&self) -> Arc<TlsLayer> {
        let Some(src) = &self.source else {
            return Arc::clone(&self.state.lock().layer);
        };
        // Inline sides observe `None` forever (as stored) and never trip
        // the change check; a missing file observes `None` and differs
        // from a previously-good stamp exactly once.
        let cert_obs = src.cert_path.as_deref().and_then(file_stamp);
        let key_obs = src.key_path.as_deref().and_then(file_stamp);
        let mut st = self.state.lock();
        if cert_obs == st.cert_stamp && key_obs == st.key_stamp {
            return Arc::clone(&st.layer);
        }

        // A file side whose stamp is None is missing, unstat'able, or not
        // a regular file — never read it: a FIFO/device blocks the read
        // under this mutex and wedges every later dial (issue #621
        // review). Any failure consumes BOTH observed stamps: the stamp
        // covers mtime/len/ino/ctime, so every real fix path trips the
        // change detector next dial — while a durably-broken file costs
        // no read/rebuild/warn per dial. (The good side's discarded bytes
        // are not stranded: when the broken side's stamp moves, both are
        // re-read.)
        let cert_pem = match &src.cert_path {
            Some(p) => match cert_obs.and_then(|_| read_cert_file(p).ok()) {
                Some(b) => b,
                None => {
                    warn!(
                        "{PLUGIN}: cert reload: {} missing, non-regular, or unreadable \
                         — keeping last pair",
                        p.display()
                    );
                    st.cert_stamp = cert_obs;
                    st.key_stamp = key_obs;
                    return Arc::clone(&st.layer);
                }
            },
            None => st.cert.cert_pem.clone(),
        };
        let key_pem = match &src.key_path {
            Some(p) => match key_obs.and_then(|_| read_cert_file(p).ok()) {
                Some(b) => b,
                None => {
                    warn!(
                        "{PLUGIN}: key reload: {} missing, non-regular, or unreadable \
                         — keeping last pair",
                        p.display()
                    );
                    st.cert_stamp = cert_obs;
                    st.key_stamp = key_obs;
                    return Arc::clone(&st.layer);
                }
            },
            None => st.cert.key_pem.clone(),
        };

        let mut cfg = self.tls_config.clone();
        cfg.client_cert = Some(ClientCert {
            cert_pem: cert_pem.clone(),
            key_pem: key_pem.clone(),
        });
        match TlsLayer::new(&cfg) {
            Ok(layer) => {
                debug!("{PLUGIN}: reloaded client certificate/key files");
                st.layer = Arc::new(layer);
                st.cert = ClientCert { cert_pem, key_pem };
            }
            Err(e) => {
                warn!("{PLUGIN}: cert/key reload invalid, keeping last pair: {e}");
            }
        }
        // Stamps record the pre-read observation either way: a rewrite
        // landing between the stat and the read yields a stale stamp that
        // trips once more next dial — reloading twice beats stranding an
        // update — and a parse failure is consumed so a durably-invalid
        // file doesn't rebuild an SSL_CTX per dial (see TlsReloadState).
        st.cert_stamp = cert_obs;
        st.key_stamp = key_obs;
        Arc::clone(&st.layer)
    }
}

/// Build a reusable, cert/key-reloading `TlsLayer` holder for a gost
/// config with `tls=true`.
///
/// Call once at adapter construction time. Returns `None` when TLS is
/// disabled.  SNI is `host`, overridden by a `Host` entry in `headers`
/// (upstream: `config.Headers.Get("Host")` replaces `ServerName`).
pub fn build_tls_layer(cfg: &GostPluginConfig) -> Result<Option<ReloadableTlsLayer>> {
    if !cfg.tls {
        return Ok(None);
    }
    let sni = cfg
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("host"))
        .map(|(_, v)| v)
        .filter(|h| !h.is_empty())
        .cloned()
        .unwrap_or_else(|| cfg.host.clone());
    let mut tls_config = TlsConfig::new(sni);
    tls_config.alpn = vec!["http/1.1".to_string()];
    tls_config.skip_cert_verify = cfg.skip_cert_verify;
    tls_config.verify_name = cfg.name_cert_verify.clone();
    tls_config.cert_pin = cfg.cert_pin;
    tls_config.client_cert = cfg.client_cert.clone();
    tls_config.ech = cfg.ech.clone();
    let layer = TlsLayer::new(&tls_config).map_err(transport_to_proxy_err)?;
    Ok(Some(ReloadableTlsLayer {
        tls_config,
        source: cfg.client_cert_source.clone(),
        state: parking_lot::Mutex::new(TlsReloadState {
            layer: Arc::new(layer),
            cert: cfg.client_cert.clone().unwrap_or(ClientCert {
                cert_pem: Vec::new(),
                key_pem: Vec::new(),
            }),
            // Stamps start unknown: the first dial reloads once, closing
            // the parse→stat gap — a file swapped between `TlsLayer::new`
            // and a post-hoc stat here would otherwise pin a stamp newer
            // than the loaded bytes (issue #621 review).
            cert_stamp: None,
            key_stamp: None,
        }),
    }))
}

/// Build a reusable `WsLayer` for a gost config.
///
/// Call once at adapter construction time — `WsLayer::new` validates the
/// request shape (path, header names/values) eagerly, so a malformed
/// `headers` map fails at config load instead of on every dial.
///
/// Upstream lets a `Host` entry in `headers` override the request's Host
/// header (`request.Host = host`; the same entry already drove TLS SNI),
/// so it is lifted out of `extra_headers` into `host_header` instead of
/// being sent twice.
pub fn build_ws_layer(cfg: &GostPluginConfig) -> Result<WsLayer> {
    let host_header = cfg
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("host"))
        .map(|(_, v)| v)
        .filter(|h| !h.is_empty())
        .cloned()
        .unwrap_or_else(|| cfg.host.clone());
    let mut extra_headers: Vec<(String, String)> = cfg
        .headers
        .iter()
        .filter(|(k, _)| !k.eq_ignore_ascii_case("host"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    // Upstream dials the ws upgrade through Go's net/http, which injects
    // `User-Agent: Go-http-client/1.1` when the request doesn't set one —
    // match that default (a user-supplied UA wins, as upstream's does).
    if !cfg
        .headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("user-agent"))
    {
        extra_headers.push(("User-Agent".into(), "Go-http-client/1.1".into()));
    }
    WsLayer::new(WsConfig {
        path: cfg.path.clone(),
        host_header: Some(host_header),
        extra_headers,
        ..WsConfig::default()
    })
    .map_err(transport_to_proxy_err)
}

/// Dial a TCP (+ optional TLS) + WebSocket (+ optional smux) connection to
/// `server_host:server_port` and return the framed stream ready to be
/// wrapped by the SS encryption layer.
///
/// When `tls_layer` is `Some`, [`ReloadableTlsLayer::current`] snapshots
/// the current layer per dial (reloading changed cert/key files first),
/// so the BoringSSL `SSL_CTX` and root cert store are rebuilt only when
/// the source PEMs actually change.
///
/// With `mux=true` a fresh smux session wraps the WebSocket and the
/// returned stream is its first `open_stream` — upstream
/// `NewGostWebsocket` creates one session per dial, so the session dies
/// when the stream drops.
pub async fn dial(
    cfg: &GostPluginConfig,
    tls_layer: Option<&ReloadableTlsLayer>,
    ws_layer: &WsLayer,
    server_host: &str,
    server_port: u16,
    dialer: &dyn crate::dialer::TcpDialer,
    internal: bool,
) -> Result<Box<dyn meow_transport::Stream>> {
    debug!(
        "{PLUGIN}: dialing {}:{} tls={} host={} path={} mux={}",
        server_host, server_port, cfg.tls, cfg.host, cfg.path, cfg.mux
    );

    // 1) Raw TCP.
    let tcp = dialer
        .dial(server_host, server_port, internal)
        .await
        .map_err(MeowError::Io)?;

    // 2) Optional TLS handshake via the reloadable layer — `current()`
    //    re-stats the cert/key files and rebuilds on change.
    //  `connect` already returns `Box<dyn Stream>` — no double-boxing.
    // A `tls=true` config without a layer would silently dial plaintext —
    // refuse rather than ship an unencrypted stream (issue #621 review).
    if cfg.tls && tls_layer.is_none() {
        return Err(MeowError::Config(format!(
            "{PLUGIN}: tls=true but no TLS layer was built"
        )));
    }
    let tls_snapshot = tls_layer.map(ReloadableTlsLayer::current);
    let stream: Box<dyn meow_transport::Stream> = if let Some(tls) = tls_snapshot.as_deref() {
        tls.connect(tcp).await.map_err(transport_to_proxy_err)?
    } else {
        tcp
    };

    // 3) WebSocket upgrade via the pre-built WsLayer.
    let stream = ws_layer
        .connect(stream)
        .await
        .map_err(transport_to_proxy_err)?;

    // 4) Optional smux session (upstream `smux.DefaultConfig()` +
    //    `KeepAliveDisabled` — meow's smux v1 never *sends* keepalive NOPs,
    //    matching upstream, but inbound NOP frames from the peer are
    //    handled: `CMD_NOP` is consumed in the session read loop).
    //    The session is single-stream by construction, so the stream gets
    //    the whole session receive budget (upstream: per-stream
    //    MaxReceiveBuffer).
    if !cfg.mux {
        return Ok(stream);
    }
    #[cfg(feature = "mux")]
    {
        let session = std::sync::Arc::new(
            crate::mux::smux::Session::client_with_stream_buffer(
                stream,
                crate::mux::smux::MAX_RECEIVE_BUFFER,
            )
            .map_err(MeowError::Io)?,
        );
        let stream = session.open_stream().await.map_err(MeowError::Io)?;
        Ok(Box::new(stream))
    }
    #[cfg(not(feature = "mux"))]
    {
        // Unreachable — `parse_opts` rejects mux=true without the feature.
        Err(MeowError::Config(format!(
            "{PLUGIN}: mux requires the `mux` cargo feature"
        )))
    }
}

#[cfg(test)]
#[path = "test/gost_plugin_test.rs"]
mod tests;
