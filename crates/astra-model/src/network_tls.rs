//! Bounded evidence from normal certificate validation and HTTP transactions.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TlsDetail {
    Trust {
        source: String,
        loaded_roots: u32,
        load_errors: Vec<String>,
        load_errors_truncated: bool,
        policy: String,
    },
    Handshake {
        server_name: String,
        verified: bool,
        /// Chain/name/time validation under system roots, independently of
        /// later handshake signature or transport failures.
        #[serde(default)]
        system_chain_verified: Option<bool>,
        verification_attempted: bool,
        verification_error: Option<String>,
        handshake_error: Option<String>,
        protocol: Option<String>,
        cipher: Option<String>,
        alpn: Option<String>,
        handshake_ms: f64,
        received_certificates: u32,
        retained_certificates: u32,
        retained_der_bytes: u32,
        truncated: bool,
        bundled_comparison: Option<TrustComparison>,
    },
    Certificate {
        index: u32,
        sha256: String,
        subject: Option<String>,
        issuer: Option<String>,
        not_before: Option<String>,
        not_after: Option<String>,
        subject_alt_names: Vec<String>,
        der_bytes: u32,
        metadata_truncated: bool,
        parse_error: Option<String>,
    },
    /// At most 24 KiB raw DER per event, base64 encoded for portable export.
    CertificateDer {
        index: u32,
        offset: u32,
        total_bytes: u32,
        base64: String,
    },
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrustComparison {
    pub source: String,
    pub verified: bool,
    pub error: Option<String>,
    /// Offline chain verification only, never another network connection.
    pub offline: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HttpTimings {
    /// Combined preparation, resolution, transport connect and proxy handshake.
    pub connect_ms: f64,
    /// Direct target DNS or proxy-entry DNS; remote target DNS is in proxy handshake.
    pub dns_ms: f64,
    pub transport_connect_ms: f64,
    pub proxy_handshake_ms: Option<f64>,
    pub tls_ms: Option<f64>,
    pub response_headers_ms: f64,
    pub body_ms: f64,
    pub total_ms: f64,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HttpDetail {
    Response {
        hop: u32,
        /// Origin only; paths, userinfo and query parameters are never exported.
        service: String,
        method: String,
        status: u16,
        version: String,
        timings: HttpTimings,
        header_bytes: u32,
        preview_bytes: u32,
        body_truncated: bool,
        headers_redacted: bool,
        preview_redacted: bool,
    },
    Headers {
        hop: u32,
        status: u16,
        version: String,
        headers: Vec<HttpHeader>,
    },
    Preview {
        hop: u32,
        offset: u32,
        text: String,
    },
    Redirect {
        hop: u32,
        from: String,
        to: String,
        followed: bool,
        reason: String,
    },
    Failure {
        /// None when the enclosing attempt deadline interrupted a pending hop.
        hop: Option<u32>,
        phase: String,
        message: String,
    },
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HttpHeader {
    pub name: String,
    pub value: String,
}
