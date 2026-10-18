use astra_ipc::Target;
use std::net::SocketAddr;
pub fn from_ss(a: shadowsocks::relay::socks5::Address) -> Target {
    match a {
        shadowsocks::relay::socks5::Address::SocketAddress(a) => Target {
            host: a.ip().to_string(),
            port: a.port(),
        },
        shadowsocks::relay::socks5::Address::DomainNameAddress(host, port) => Target { host, port },
    }
}
pub fn to_ss(a: &Target) -> shadowsocks::relay::socks5::Address {
    match a.host.parse() {
        Ok(ip) => shadowsocks::relay::socks5::Address::SocketAddress(SocketAddr::new(ip, a.port)),
        Err(_) => shadowsocks::relay::socks5::Address::DomainNameAddress(a.host.clone(), a.port),
    }
}

mod adapter {
    use crate::native::*;
    use anyhow::{bail, Result};
    use meow_common::ProxyAdapter;
    use std::{path::Path, sync::Arc};
    pub fn build(
        tag: &str,
        cfg: &NodeConfig,
        _base: &Path,
        dialer: Arc<meow_proxy::dialer::DirectDialer>,
        mux: Option<meow_proxy::mux::MuxOptions>,
    ) -> Result<Arc<dyn ProxyAdapter>> {
        if cfg.tls.is_some() || cfg.transport.kind != TransportKind::Tcp {
            bail!("Shadowsocks transport options are not supported yet");
        }
        if mux.is_some() {
            bail!("Shadowsocks mux is not supported yet");
        }
        Ok(Arc::new(meow_proxy::ShadowsocksAdapter::new(
            tag,
            &cfg.server,
            cfg.port,
            required(&cfg.password, "password")?,
            required(&cfg.cipher, "cipher")?,
            true,
            None,
            None,
            None,
            dialer,
        )?))
    }
}
pub(crate) use adapter::build;

pub async fn accept<S>(
    stream: S,
    context: shadowsocks::context::SharedContext,
    config: &shadowsocks::config::ServerConfig,
) -> anyhow::Result<(shadowsocks::relay::tcprelay::ProxyServerStream<S>, Target)>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut stream = shadowsocks::relay::tcprelay::ProxyServerStream::from_stream(
        context,
        stream,
        config.method(),
        config.key(),
    );
    let target = from_ss(stream.handshake().await?);
    Ok((stream, target))
}
