use anyhow::Result;
use std::sync::{Arc, OnceLock};
pub(crate) fn connector(https: bool) -> Result<tokio_rustls::TlsConnector> {
    static DOT: OnceLock<Arc<tokio_rustls::rustls::ClientConfig>> = OnceLock::new();
    static DOH: OnceLock<Arc<tokio_rustls::rustls::ClientConfig>> = OnceLock::new();
    let config_slot = if https { &DOH } else { &DOT };
    if let Some(config) = config_slot.get() {
        return Ok(config.clone().into());
    }
    use tokio_rustls::rustls::{self, RootCertStore};
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_root_certificates(roots)
    .with_no_client_auth();
    if https {
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    }
    let config = Arc::new(config);
    let _ = config_slot.set(config.clone());
    Ok(config.into())
}
