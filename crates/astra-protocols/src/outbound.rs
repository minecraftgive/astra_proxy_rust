use crate::config::OutboundConfig;
use anyhow::{bail, Context, Result};
use astra_model::Target;
use std::sync::{Arc, OnceLock};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
};
use tokio_rustls::{
    rustls::{self, pki_types::ServerName, RootCertStore},
    TlsConnector,
};

pub trait Duplex: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T> Duplex for T where T: AsyncRead + AsyncWrite + Unpin + Send {}
pub type Stream = Box<dyn Duplex>;

use crate::{http::http_connect, socks5::socks5, trojan::trojan_tls, vless::vless_tls};

pub async fn open(target: &Target, outbound: Option<&OutboundConfig>) -> Result<Stream> {
    match outbound {
        Some(OutboundConfig::Native { .. }) => bail!("native outbound was not initialized"),
        None => {
            let stream = crate::egress::connect((target.host.as_str(), target.port)).await?;
            stream.set_nodelay(true)?;
            Ok(Box::new(stream))
        }
        Some(OutboundConfig::Socks5 {
            server,
            username,
            password,
            ..
        }) => socks5(server, username.as_deref(), password.as_deref(), target).await,
        Some(OutboundConfig::Http {
            server,
            username,
            password,
            ..
        }) => http_connect(server, username.as_deref(), password.as_deref(), target).await,
        Some(OutboundConfig::TrojanTls {
            server,
            sni,
            password,
            ..
        }) => trojan_tls(server, sni, password, target).await,
        Some(OutboundConfig::VlessTls {
            server, sni, uuid, ..
        }) => vless_tls(server, sni, uuid, target).await,
    }
}

pub(crate) async fn tls_connect(
    server: &str,
    sni: &str,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let tcp = crate::egress::connect(server)
        .await
        .context("TLS endpoint TCP connect")?;
    tcp.set_nodelay(true)?;
    tls_on(tcp, sni).await
}
pub(crate) async fn tls_on<S: AsyncRead + AsyncWrite + Unpin>(
    tcp: S,
    sni: &str,
) -> Result<tokio_rustls::client::TlsStream<S>> {
    static TLS_CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    let config = TLS_CONFIG.get_or_init(|| {
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        )
    });
    let connector = TlsConnector::from(config.clone());
    let name = ServerName::try_from(sni.to_owned()).context("invalid TLS SNI")?;
    connector
        .connect(name, tcp)
        .await
        .context("TLS handshake failed")
}

/// SOCKS5 addressing, also used by Trojan but NOT VLESS.
pub fn append_socks_address(out: &mut Vec<u8>, target: &Target) -> Result<()> {
    out.extend(crate::wire::encode(target, 0)?);
    Ok(())
}

#[cfg(test)]
#[path = "test/outbound_test.rs"]
mod tests;

/// All legacy handshakes use the supplied dialer; they cannot bypass a chain.
pub async fn open_with_dialer(
    target: &Target,
    config: &OutboundConfig,
    dialer: &dyn meow_proxy::dialer::TcpDialer,
) -> Result<Stream> {
    let server = match config {
        OutboundConfig::Socks5 { server, .. }
        | OutboundConfig::Http { server, .. }
        | OutboundConfig::TrojanTls { server, .. }
        | OutboundConfig::VlessTls { server, .. } => server,
        _ => bail!("native outbound requires its compiled adapter"),
    };
    let endpoint = crate::http::authority_target(server)?;
    let stream = dialer.dial(&endpoint.host, endpoint.port, false).await?;
    match config {
        OutboundConfig::Socks5 {
            username, password, ..
        } => {
            crate::socks5::socks5_on(stream, username.as_deref(), password.as_deref(), target).await
        }
        OutboundConfig::Http {
            username, password, ..
        } => {
            crate::http::http_connect_on(stream, username.as_deref(), password.as_deref(), target)
                .await
        }
        OutboundConfig::TrojanTls { sni, password, .. } => {
            crate::trojan::trojan_on(tls_on(stream, sni).await?, password, target).await
        }
        OutboundConfig::VlessTls { sni, uuid, .. } => {
            crate::vless::vless_on(tls_on(stream, sni).await?, uuid, target).await
        }
        _ => unreachable!(),
    }
}
