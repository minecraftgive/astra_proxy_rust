use anyhow::{ensure, Context, Result};
use std::{path::Path, sync::Arc};

pub use astra_model::config::server::{Config, Decryption, Listener, Protocol, Reality, Transport};
pub struct Prepared {
    pub config: Config,
    pub tls: Vec<Option<Arc<tokio_rustls::rustls::ServerConfig>>>,
    pub digest: u64,
}
pub fn load(path: &Path) -> Result<Prepared> {
    ensure!(
        std::fs::metadata(path)?.len() <= 1024 * 1024,
        "server configuration exceeds 1 MiB"
    );
    let text = std::fs::read_to_string(path)?;
    ensure!(
        text.len() <= 1024 * 1024,
        "server configuration exceeds 1 MiB"
    );
    prepare_text(path, &text)
}
pub fn prepare_text(path: &Path, text: &str) -> Result<Prepared> {
    ensure!(
        text.len() <= 1024 * 1024,
        "server configuration exceeds 1 MiB"
    );
    let mut config: Config = toml::from_str(text)?;
    config.validate_structure()?;
    let mut tls = vec![];
    for listener in &mut config.listeners {
        if listener.protocol == Protocol::Shadowsocks {
            let cipher: shadowsocks::crypto::CipherKind = listener
                .cipher
                .as_deref()
                .context("Shadowsocks requires cipher")?
                .parse()
                .map_err(|_| anyhow::anyhow!("unknown Shadowsocks cipher"))?;
            ensure!(
                cipher.is_aead() || cipher.is_aead_2022(),
                "only authenticated Shadowsocks AEAD/2022 ciphers are accepted"
            );
            let password = listener
                .password
                .as_deref()
                .filter(|v| !v.is_empty())
                .context("Shadowsocks password is required")?;
            shadowsocks::config::ServerConfig::new(listener.listen, password, cipher)
                .map_err(|e| anyhow::anyhow!("Shadowsocks key: {e}"))?;
        }
        if let Some(reality) = &listener.reality {
            crate::security::RealityState::validate(reality)?;
        }
        if let Some(encryption) = &listener.decryption {
            crate::encryption::prepare(encryption)?;
        }
        let compiled = if let Some(options) = &mut listener.tls {
            let base = path.parent().context("config has no directory")?;
            if options.certificate.is_relative() {
                options.certificate = base.join(&options.certificate);
            }
            if options.private_key.is_relative() {
                options.private_key = base.join(&options.private_key);
            }
            let certs = rustls_pemfile::certs(&mut std::io::BufReader::new(std::fs::File::open(
                &options.certificate,
            )?))
            .collect::<std::io::Result<Vec<_>>>()?;
            let key = rustls_pemfile::private_key(&mut std::io::BufReader::new(
                std::fs::File::open(&options.private_key)?,
            ))?
            .context("no private key in PEM")?;
            let mut tls = tokio_rustls::rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(certs, key)?;
            tls.alpn_protocols = if listener.protocol.quic() {
                vec![b"h3".to_vec()]
            } else if matches!(listener.transport, Transport::Grpc | Transport::Xhttp) {
                vec![b"h2".to_vec()]
            } else {
                vec![b"http/1.1".to_vec()]
            };
            Some(Arc::new(tls))
        } else {
            None
        };
        tls.push(compiled);
    }
    Ok(Prepared {
        config,
        tls,
        digest: astra_ipc::control::config_digest(text),
    })
}

#[cfg(test)]
#[path = "test/config_test.rs"]
mod tests;
