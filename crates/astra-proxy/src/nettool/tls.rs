//! Capture evidence before normal rustls validation; never turn failures into acceptance.
use anyhow::{Context, Result};
use astra_model::{
    nettool::Event,
    network_tls::{TlsDetail, TrustComparison},
};
use base64::Engine;
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;
use tokio_rustls::{
    client::TlsStream,
    rustls::{
        self,
        client::{
            danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
            WebPkiServerVerifier,
        },
        pki_types::{CertificateDer, ServerName, UnixTime},
    },
    TlsConnector,
};

const MAX_CERTS: usize = 16;
const MAX_DER: usize = 256 * 1024;
const DER_CHUNK: usize = 24 * 1024;
fn bound(value: impl ToString, n: usize) -> String {
    let value = value.to_string();
    let mut end = value.len().min(n);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}
#[derive(Debug)]
pub(crate) struct TrustSnapshot {
    roots: Arc<rustls::RootCertStore>,
    source: String,
    errors: Vec<String>,
    errors_truncated: bool,
}
impl TrustSnapshot {
    pub(crate) fn native() -> Self {
        let result = rustls_native_certs::load_native_certs();
        let overrides = ["SSL_CERT_FILE", "SSL_CERT_DIR"]
            .into_iter()
            .filter(|key| std::env::var_os(key).is_some())
            .collect::<Vec<_>>();
        let source = if overrides.is_empty() {
            format!(
                "rustls-native-certs: {} account-visible system root snapshot",
                std::env::consts::OS
            )
        } else {
            format!(
                "rustls-native-certs: environment override {} (path values omitted)",
                overrides.join(", ")
            )
        };
        let mut roots = rustls::RootCertStore::empty();
        let (added, ignored) = roots.add_parsable_certificates(result.certs);
        let mut errors = result
            .errors
            .iter()
            .take(32)
            .map(|e| bound(e, 512))
            .collect::<Vec<_>>();
        if ignored > 0 {
            errors.push(format!(
                "{ignored} loaded certificates could not be parsed as roots"
            ));
        }
        if added == 0 {
            errors.push(
                "No usable system root certificates; bundled roots are not substituted".into(),
            );
        }
        Self {
            roots: Arc::new(roots),
            source,
            errors,
            errors_truncated: result.errors.len() > 32,
        }
    }
    #[cfg(test)]
    pub(crate) fn fixture(roots: rustls::RootCertStore) -> Self {
        Self {
            roots: Arc::new(roots),
            source: "local fixture system-root snapshot".into(),
            errors: vec![],
            errors_truncated: false,
        }
    }
    fn verifier(&self) -> Option<Arc<WebPkiServerVerifier>> {
        WebPkiServerVerifier::builder_with_provider(
            self.roots.clone(),
            Arc::new(rustls::crypto::aws_lc_rs::default_provider()),
        )
        .build()
        .ok()
    }
    fn detail(&self) -> TlsDetail {
        let mut detail=TlsDetail::Trust {source:self.source.clone(),loaded_roots:self.roots.len() as u32,load_errors:self.errors.clone(),load_errors_truncated:self.errors_truncated,policy:"System root certificate snapshot verification by rustls; not the complete OS certificate policy. No network chain fetching and no bundled-root fallback.".into()};
        while serde_json::to_vec(&detail).is_ok_and(|bytes| bytes.len() > 40 * 1024) {
            if let TlsDetail::Trust {
                load_errors,
                load_errors_truncated,
                ..
            } = &mut detail
            {
                *load_errors_truncated = true;
                load_errors.pop();
            }
        }
        detail
    }
}
#[derive(Debug, Default)]
struct Capture {
    chain: Vec<Vec<u8>>,
    received: u32,
    retained_bytes: usize,
    truncated: bool,
    verification_error: Option<String>,
    system_chain_verified: Option<bool>,
    comparison: Option<TrustComparison>,
    handshake: Option<TlsDetail>,
    reported: bool,
}
#[derive(Debug)]
struct RecordingVerifier {
    inner: Option<Arc<WebPkiServerVerifier>>,
    comparison: Option<Arc<WebPkiServerVerifier>>,
    capture: Arc<Mutex<Capture>>,
}
impl ServerCertVerifier for RecordingVerifier {
    fn verify_server_cert(
        &self,
        end: &CertificateDer<'_>,
        chain: &[CertificateDer<'_>],
        name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        {
            let mut capture = self.capture.lock().unwrap();
            capture.received = (chain.len() + 1).min(u32::MAX as usize) as u32;
            for certificate in std::iter::once(end).chain(chain.iter()).take(MAX_CERTS) {
                if capture.retained_bytes + certificate.len() > MAX_DER {
                    capture.truncated = true;
                    break;
                }
                capture.chain.push(certificate.to_vec());
                capture.retained_bytes += certificate.len();
            }
            capture.truncated |= capture.chain.len() < chain.len() + 1;
        }
        let verified = match &self.inner {
            Some(inner) => inner.verify_server_cert(end, chain, name, ocsp, now),
            None => Err(rustls::Error::General(
                "No usable system root snapshot; validation cannot succeed".into(),
            )),
        };
        let comparison = self.comparison.as_ref().map(|inner| {
            let result = inner.verify_server_cert(end, chain, name, ocsp, now);
            TrustComparison {
                source: "bundled webpki-roots (offline comparison)".into(),
                verified: result.is_ok(),
                error: result.err().map(|e| bound(e, 2048)),
                offline: true,
            }
        });
        let mut capture = self.capture.lock().unwrap();
        capture.system_chain_verified = Some(verified.is_ok());
        capture.verification_error = verified.as_ref().err().map(|e| bound(e, 2048));
        capture.comparison = comparison;
        verified
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        sig: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        let result = self
            .inner
            .as_ref()
            .ok_or_else(|| rustls::Error::General("System root verifier unavailable".into()))?
            .verify_tls12_signature(message, cert, sig);
        if let Err(error) = &result {
            self.capture.lock().unwrap().verification_error =
                Some(bound(format!("Handshake signature: {error}"), 2048));
        }
        result
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        sig: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        let result = self
            .inner
            .as_ref()
            .ok_or_else(|| rustls::Error::General("System root verifier unavailable".into()))?
            .verify_tls13_signature(message, cert, sig);
        if let Err(error) = &result {
            self.capture.lock().unwrap().verification_error =
                Some(bound(format!("Handshake signature: {error}"), 2048));
        }
        result
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.inner.as_ref().map_or_else(
            || {
                rustls::crypto::aws_lc_rs::default_provider()
                    .signature_verification_algorithms
                    .supported_schemes()
            },
            |inner| inner.supported_verify_schemes(),
        )
    }
    fn requires_raw_public_keys(&self) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|inner| inner.requires_raw_public_keys())
    }
    fn root_hint_subjects(&self) -> Option<&[rustls::DistinguishedName]> {
        self.inner
            .as_ref()
            .and_then(|inner| inner.root_hint_subjects())
    }
}
type PendingHandshake = (u32, String, Instant, Arc<Mutex<Capture>>);
#[derive(Default)]
pub(crate) struct TlsState {
    trust: OnceLock<Arc<TrustSnapshot>>,
    pending: Mutex<Vec<PendingHandshake>>,
}
// A Rust future may be dropped by an enclosing DNS/NAT timeout. Persist the
// already-captured chain before that future releases its local route state.
struct EvidenceGuard<'a, F: Fn(Event) -> Result<()>> {
    state: &'a TlsState,
    seq: u32,
    emit: &'a F,
    armed: bool,
}
impl<F: Fn(Event) -> Result<()>> Drop for EvidenceGuard<'_, F> {
    fn drop(&mut self) {
        if self.armed {
            let _ = self
                .state
                .flush(self.seq, "TLS handshake cancelled or timed out", self.emit);
        }
    }
}
impl TlsState {
    #[cfg(test)]
    pub(crate) fn set_fixture(&self, trust: TrustSnapshot) {
        self.trust.set(Arc::new(trust)).unwrap();
    }
    pub(crate) async fn connect<S>(
        &self,
        seq: u32,
        name: &str,
        stream: S,
        compare: bool,
        emit: &impl Fn(Event) -> Result<()>,
    ) -> Result<TlsStream<S>>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        self.connect_with_alpn(seq, name, stream, compare, false, emit)
            .await
    }
    pub(crate) async fn connect_http<S>(
        &self,
        seq: u32,
        name: &str,
        stream: S,
        compare: bool,
        emit: &impl Fn(Event) -> Result<()>,
    ) -> Result<TlsStream<S>>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        self.connect_with_alpn(seq, name, stream, compare, true, emit)
            .await
    }
    async fn connect_with_alpn<S>(
        &self,
        seq: u32,
        name: &str,
        stream: S,
        compare: bool,
        http: bool,
        emit: &impl Fn(Event) -> Result<()>,
    ) -> Result<TlsStream<S>>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let trust = self.trust.get_or_init(|| Arc::new(TrustSnapshot::native()));
        emit(Event::Tls {
            seq,
            detail: Box::new(trust.detail()),
        })?;
        let name_owned =
            ServerName::try_from(name.to_owned()).context("Invalid TLS server name")?;
        let capture = Arc::new(Mutex::new(Capture::default()));
        let comparison = if compare {
            let roots =
                rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            Some(
                WebPkiServerVerifier::builder_with_provider(
                    Arc::new(roots),
                    Arc::new(rustls::crypto::aws_lc_rs::default_provider()),
                )
                .build()?,
            )
        } else {
            None
        };
        let verifier = RecordingVerifier {
            inner: trust.verifier(),
            comparison,
            capture: capture.clone(),
        };
        let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
        if http {
            config.alpn_protocols = vec![b"http/1.1".to_vec()];
        }
        let start = Instant::now();
        self.pending
            .lock()
            .unwrap()
            .push((seq, name.to_owned(), start, capture.clone()));
        let mut evidence_guard = EvidenceGuard {
            state: self,
            seq,
            emit,
            armed: true,
        };
        let result = TlsConnector::from(Arc::new(config))
            .connect(name_owned, stream)
            .await;
        {
            let mut capture = capture.lock().unwrap();
            let info = result.as_ref().ok().map(|s| s.get_ref().1);
            capture.handshake = Some(TlsDetail::Handshake {
                server_name: name.to_owned(),
                verified: result.is_ok(),
                system_chain_verified: capture.system_chain_verified,
                verification_attempted: capture.received > 0,
                verification_error: capture.verification_error.clone(),
                handshake_error: result.as_ref().err().map(|e| bound(e, 2048)),
                protocol: info
                    .and_then(|c| c.protocol_version())
                    .map(|v| format!("{v:?}")),
                cipher: info
                    .and_then(|c| c.negotiated_cipher_suite())
                    .map(|v| format!("{:?}", v.suite())),
                alpn: info
                    .and_then(|c| c.alpn_protocol())
                    .map(|v| bound(String::from_utf8_lossy(v), 128)),
                handshake_ms: start.elapsed().as_secs_f64() * 1000.,
                received_certificates: capture.received,
                retained_certificates: capture.chain.len() as u32,
                retained_der_bytes: capture.retained_bytes as u32,
                truncated: capture.truncated,
                bundled_comparison: capture.comparison.clone(),
            });
        }
        evidence_guard.armed = false;
        self.flush(seq, "TLS handshake interrupted", emit)?;
        result.context("TLS system-root snapshot validation/handshake failed")
    }
    /// Also called after the enclosing attempt times out, preserving evidence
    /// received before the pending handshake future was dropped.
    pub(crate) fn flush(
        &self,
        seq: u32,
        error: &str,
        emit: &impl Fn(Event) -> Result<()>,
    ) -> Result<()> {
        let mut pending = self.pending.lock().unwrap();
        for (id, name, start, capture) in pending.iter().filter(|(id, ..)| *id == seq) {
            let mut capture = capture.lock().unwrap();
            if capture.reported {
                continue;
            }
            let handshake = capture
                .handshake
                .clone()
                .unwrap_or_else(|| TlsDetail::Handshake {
                    server_name: name.clone(),
                    verified: false,
                    system_chain_verified: capture.system_chain_verified,
                    verification_attempted: capture.received > 0,
                    verification_error: capture.verification_error.clone(),
                    handshake_error: Some(error.into()),
                    protocol: None,
                    cipher: None,
                    alpn: None,
                    handshake_ms: start.elapsed().as_secs_f64() * 1000.,
                    received_certificates: capture.received,
                    retained_certificates: capture.chain.len() as u32,
                    retained_der_bytes: capture.retained_bytes as u32,
                    truncated: capture.truncated,
                    bundled_comparison: capture.comparison.clone(),
                });
            emit(Event::Tls {
                seq: *id,
                detail: Box::new(handshake),
            })?;
            for (index, der) in capture.chain.iter().enumerate() {
                emit(Event::Tls {
                    seq: *id,
                    detail: Box::new(certificate(index as u32, der)),
                })?;
                for (chunk_index, chunk) in der.chunks(DER_CHUNK).enumerate() {
                    emit(Event::Tls {
                        seq: *id,
                        detail: Box::new(TlsDetail::CertificateDer {
                            index: index as u32,
                            offset: (chunk_index * DER_CHUNK) as u32,
                            total_bytes: der.len() as u32,
                            base64: base64::engine::general_purpose::STANDARD.encode(chunk),
                        }),
                    })?;
                }
            }
            capture.reported = true;
        }
        pending.retain(|(_, _, _, capture)| !capture.lock().unwrap().reported);
        Ok(())
    }
}
fn certificate(index: u32, der: &[u8]) -> TlsDetail {
    use x509_parser::{certificate::X509Certificate, prelude::FromDer};
    let digest = format!("{:x}", Sha256::digest(der));
    let mut detail = TlsDetail::Certificate {
        index,
        sha256: digest,
        subject: None,
        issuer: None,
        not_before: None,
        not_after: None,
        subject_alt_names: vec![],
        der_bytes: der.len() as u32,
        metadata_truncated: false,
        parse_error: None,
    };
    if let TlsDetail::Certificate {
        subject,
        issuer,
        not_before,
        not_after,
        subject_alt_names,
        metadata_truncated,
        parse_error,
        ..
    } = &mut detail
    {
        match X509Certificate::from_der(der) {
            Ok((_,certificate))=>{
                let raw_subject=certificate.subject().to_string();let raw_issuer=certificate.issuer().to_string();
                *metadata_truncated=raw_subject.len()>1024||raw_issuer.len()>1024;
                *subject=Some(bound(raw_subject,1024));*issuer=Some(bound(raw_issuer,1024));*not_before=Some(certificate.validity().not_before.to_string());*not_after=Some(certificate.validity().not_after.to_string());
                if let Ok(Some(names))=certificate.subject_alternative_name(){
                    *metadata_truncated|=names.value.general_names.len()>32;
                    for name in names.value.general_names.iter().take(32){let text=name.to_string();*metadata_truncated|=text.len()>512;subject_alt_names.push(bound(text,512));}
                }
            },
            Err(_)=>*parse_error=Some("Certificate DER could not be parsed for display; normal validation result is recorded separately".into()),
        }
    }
    while serde_json::to_vec(&detail).is_ok_and(|bytes| bytes.len() > 40 * 1024) {
        if let TlsDetail::Certificate {
            subject_alt_names,
            metadata_truncated,
            ..
        } = &mut detail
        {
            *metadata_truncated = true;
            subject_alt_names.pop();
        }
    }
    detail
}

#[cfg(test)]
#[path = "../test/nettool_tls_test.rs"]
pub(super) mod tests;
