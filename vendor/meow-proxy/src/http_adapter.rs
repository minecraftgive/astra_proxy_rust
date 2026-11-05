//! HTTP CONNECT outbound proxy adapter (M1.B-3).
//!
//! Implements `ProxyAdapter` for `type: http` config entries.  The adapter
//! dials TCP (optionally TLS-wrapped) to the proxy server, sends an HTTP/1.1
//! `CONNECT host:port` request, and returns the tunnelled stream.
//!
//! # Wire format
//!
//! ```text
//! CONNECT {host}:{port} HTTP/1.1\r\n
//! Host: {host}:{port}\r\n
//! [Proxy-Authorization: Basic {base64(user:pass)}\r\n]
//! [{extra_header_k}: {extra_header_v}\r\n ...]
//! \r\n
//! ```
//! Response: `HTTP/1.x {status} ...\r\n[headers]\r\n\r\n`
//! — status 2xx → tunnel open
//! — status 407 → `Err(ProxyAuthFailed)`  (Basic auth only in M1)
//! — other      → `Err(HttpConnectFailed(status))`
//!
//! upstream: `adapter/outbound/http.go`

use async_trait::async_trait;
use base64::Engine as _;
use meow_common::{
    AdapterType, MeowError, Metadata, ProxyAdapter, ProxyConn, ProxyHealth, ProxyPacketConn, Result,
};
use smol_str::SmolStr;
use std::fmt;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::debug;

use crate::stream_conn::StreamConn;

// ─── Adapter ─────────────────────────────────────────────────────────────────

/// HTTP CONNECT outbound proxy adapter.
///
/// upstream: `adapter/outbound/http.go` — `HttpAdapter`
pub struct HttpAdapter {
    name: SmolStr,
    server: SmolStr,
    port: u16,
    /// `"server:port"` — returned by `addr()` for relay metadata building.
    addr_str: SmolStr,
    /// `Some((username, password))` — both present or neither (ADR-0002 Class A).
    auth: Option<(String, String)>,
    /// Built once at construction (rustls ClientConfig + root store are
    /// expensive); `TlsLayer::connect` is safe to call concurrently.
    tls_layer: Option<meow_transport::tls::TlsLayer>,
    /// Extra headers injected into the CONNECT request only.
    extra_headers: Vec<(String, String)>,
    health: ProxyHealth,
    dialer: Arc<dyn crate::dialer::TcpDialer>,
}
impl HttpAdapter {
    /// Create an `HttpAdapter`.
    ///
    /// `auth` — `Some((username, password))` for Basic auth; `None` for no auth.
    /// Both username and password must be set or neither (validated at parse time).
    #[allow(
        clippy::too_many_arguments,
        reason = "dialer param for pluggable TcpDialer"
    )]
    pub fn new(
        name: &str,
        server: &str,
        port: u16,
        auth: Option<(String, String)>,
        tls: bool,
        skip_cert_verify: bool,
        extra_headers: Vec<(String, String)>,
        dialer: Arc<dyn crate::dialer::TcpDialer>,
    ) -> Self {
        // Hoisted out of the dial path: TlsLayer::new clones the webpki root
        // store and builds verifier + crypto provider — per-adapter, not
        // per-connection (same pattern as TrojanAdapter::new).
        let tls_layer = tls.then(|| {
            use meow_transport::tls::{TlsConfig, TlsLayer};
            let tls_cfg = TlsConfig {
                skip_cert_verify,
                ..TlsConfig::new(server)
            };
            TlsLayer::new(&tls_cfg)
                .expect("HttpAdapter: failed to build TlsLayer — check TLS config")
        });

        Self {
            name: SmolStr::from(name),
            addr_str: SmolStr::from(format!("{server}:{port}")),
            server: SmolStr::from(server),
            port,
            auth,
            tls_layer,
            extra_headers,
            health: ProxyHealth::new(),
            dialer,
        }
    }

    /// Dial TCP to the proxy server, optionally wrapping in TLS.
    async fn dial_stream(&self, internal: bool) -> Result<Box<dyn meow_transport::Stream>> {
        let tcp = self
            .dialer
            .dial(&self.server, self.port, internal)
            .await
            .map_err(MeowError::Io)?;
        self.wrap_tls(tcp).await
    }

    /// Apply the configured TLS layer to `stream` if `tls: true`.
    /// `stream` must already terminate at this HTTP proxy — `dial_tcp`
    /// obtains it from `dialer.dial`, `connect_over` receives it from the
    /// relay chain.
    async fn wrap_tls(
        &self,
        stream: Box<dyn meow_transport::Stream>,
    ) -> Result<Box<dyn meow_transport::Stream>> {
        if let Some(tls_layer) = &self.tls_layer {
            use meow_transport::Transport;
            tls_layer
                .connect(Box::new(stream))
                .await
                .map_err(|e| match e {
                    meow_transport::TransportError::Io(e) => MeowError::Io(e),
                    other => MeowError::Proxy(other.to_string()),
                })
        } else {
            Ok(stream)
        }
    }

    /// Run the HTTP CONNECT handshake over `stream`.
    ///
    /// On success the stream is ready for tunnelled application data.
    /// On failure returns the appropriate `MeowError` variant.
    async fn run_connect<S>(
        &self,
        stream: &mut S,
        target: &(dyn fmt::Display + Send + Sync),
    ) -> Result<()>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        let request = self.build_connect_request(target)?;

        stream
            .write_all(request.as_bytes())
            .await
            .map_err(MeowError::Io)?;

        // ── Read status line ─────────────────────────────────────────────────
        let status_line = read_line(stream).await?;
        let status_str = std::str::from_utf8(&status_line)
            .map_err(|_| MeowError::Proxy("http proxy: non-UTF-8 status line".into()))?
            .trim_end_matches(['\r', '\n']);

        let status_code = parse_http_status(status_str)?;

        match status_code {
            200..=299 => {}
            407 => return Err(MeowError::ProxyAuthFailed),
            code => return Err(MeowError::HttpConnectFailed(code)),
        }

        // ── Drain response headers ───────────────────────────────────────────
        //
        // Cap at 100 headers to guard against a misbehaving proxy sending an
        // unbounded header list that would never emit \r\n\r\n.
        //
        // ADR-0002 Class A divergence: upstream http.go has no explicit cap
        // (relies on Go's default transport limits).
        const MAX_RESPONSE_HEADERS: usize = 100;
        let mut header_count = 0usize;
        loop {
            let line = read_line(stream).await?;
            if line == b"\r\n" {
                break;
            }
            header_count += 1;
            if header_count > MAX_RESPONSE_HEADERS {
                return Err(MeowError::Proxy(format!(
                    "http proxy: response has more than {MAX_RESPONSE_HEADERS} headers"
                )));
            }
        }

        Ok(())
    }

    fn build_connect_request(&self, target: &(dyn fmt::Display + Send + Sync)) -> Result<String> {
        use std::fmt::Write as _;

        let target = target.to_string();
        validate_connect_target(&target)?;

        let extra_len: usize = self
            .extra_headers
            .iter()
            .map(|(k, v)| k.len() + v.len() + 4)
            .sum();
        let mut request = String::with_capacity(64 + target.len() * 2 + extra_len);
        write!(request, "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n")
            .expect("writing to String cannot fail");

        if let Some((user, pass)) = &self.auth {
            let creds = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
            write!(request, "Proxy-Authorization: Basic {creds}\r\n")
                .expect("writing to String cannot fail");
        }

        for (name, value) in &self.extra_headers {
            validate_header_name(name)?;
            validate_header_value(name, value)?;
            write!(request, "{name}: {value}\r\n").expect("writing to String cannot fail");
        }
        request.push_str("\r\n");
        Ok(request)
    }
}

// ─── ProxyAdapter ─────────────────────────────────────────────────────────────

#[async_trait]
impl ProxyAdapter for HttpAdapter {
    fn name(&self) -> &str {
        &self.name
    }

    fn adapter_type(&self) -> AdapterType {
        AdapterType::Http
    }

    fn addr(&self) -> &str {
        &self.addr_str
    }

    fn support_udp(&self) -> bool {
        false
    }

    async fn dial_tcp(&self, metadata: &Metadata) -> Result<Box<dyn ProxyConn>> {
        let target = connect_target(metadata);
        debug!(
            "http proxy: CONNECT {} via {}:{}",
            target, self.server, self.port
        );

        let mut stream = self.dial_stream(metadata.is_internal()).await?;
        self.run_connect(&mut stream, &target).await?;
        Ok(Box::new(StreamConn(stream)))
    }

    async fn dial_udp(&self, _metadata: &Metadata) -> Result<Box<dyn ProxyPacketConn>> {
        Err(MeowError::NotSupported(
            "http proxy: UDP not supported (HTTP CONNECT is TCP-only)".into(),
        ))
    }

    /// Run the HTTP CONNECT handshake over an already-established stream.
    ///
    /// The stream already terminates at this HTTP proxy — it carries
    /// whichever outer transport the relay's preceding hop established, so
    /// the adapter's own TLS layer (when `tls: true`) still applies before
    /// CONNECT.
    ///
    /// upstream: `adapter/outbound/http.go` — `DialContextWithDialer`
    async fn connect_over(
        &self,
        stream: Box<dyn ProxyConn>,
        metadata: &Metadata,
    ) -> Result<Box<dyn ProxyConn>> {
        let target = connect_target(metadata);
        debug!(
            "http proxy: CONNECT (relay) {} over existing stream",
            target
        );
        let mut stream = self.wrap_tls(Box::new(stream)).await?;
        self.run_connect(&mut stream, &target).await?;
        Ok(Box::new(StreamConn(stream)))
    }

    fn health(&self) -> &ProxyHealth {
        &self.health
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Zero-alloc display wrapper for `host:port` from metadata.
///
/// Prefers `metadata.host` (domain name) over raw IP; this preserves the
/// domain name for SNI and logging on the upstream proxy.
fn connect_target(metadata: &Metadata) -> meow_common::AddrDisplay<'_> {
    metadata.remote_address()
}

/// Parse `"HTTP/1.x NNN ..."` → status code as `u16`.
fn parse_http_status(line: &str) -> Result<u16> {
    let mut parts = line.splitn(3, ' ');
    let version = parts.next().unwrap_or("");
    if !version.starts_with("HTTP/") {
        return Err(MeowError::Proxy(format!(
            "http proxy: unexpected response line: {line:?}"
        )));
    }
    let code_str = parts.next().unwrap_or("");
    code_str
        .parse::<u16>()
        .map_err(|_| MeowError::Proxy(format!("http proxy: invalid status code: {code_str:?}")))
}

fn validate_connect_target(target: &str) -> Result<()> {
    if target.is_empty() || target.bytes().any(|b| b <= b' ' || b == 0x7f) {
        return Err(MeowError::Config(
            "http proxy: CONNECT target contains whitespace or control bytes".into(),
        ));
    }
    Ok(())
}

fn validate_header_name(name: &str) -> Result<()> {
    if name.is_empty() || !name.bytes().all(is_header_token_byte) {
        return Err(MeowError::Config(format!(
            "http proxy: invalid extra header name {name:?}"
        )));
    }
    Ok(())
}

fn validate_header_value(name: &str, value: &str) -> Result<()> {
    if value.bytes().any(|b| matches!(b, b'\r' | b'\n' | 0)) {
        return Err(MeowError::Config(format!(
            "http proxy: invalid value for extra header {name:?}"
        )));
    }
    Ok(())
}

fn is_header_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

/// Read one `\r\n`-terminated line from an async reader.
///
/// Returns the line bytes including the trailing `\r\n`.
/// Errors with `InvalidData` if the line exceeds 8 KiB (guards against a
/// misbehaving proxy sending an infinite stream without a newline).
async fn read_line<R: tokio::io::AsyncRead + Unpin>(reader: &mut R) -> Result<Vec<u8>> {
    let mut line = Vec::with_capacity(64);
    let mut byte = [0u8; 1];
    loop {
        reader.read_exact(&mut byte).await.map_err(MeowError::Io)?;
        line.push(byte[0]);
        if line.ends_with(b"\r\n") {
            return Ok(line);
        }
        if line.len() > 8192 {
            return Err(MeowError::Proxy(
                "http proxy: response header line too long (> 8 KiB)".into(),
            ));
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "test/http_adapter_test.rs"]
mod tests;
