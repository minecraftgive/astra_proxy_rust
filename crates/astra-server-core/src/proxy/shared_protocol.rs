use crate::{
    config::{Listener, Protocol},
    net::{self, App, Hub, Stream},
};
use anyhow::{bail, ensure, Result};
use astra_ipc::Target;
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub struct Inbound {
    pub options: Listener,
    pub tls: Option<Arc<tokio_rustls::rustls::ServerConfig>>,
    pub vmess: astra_protocols::vmess::Server,
    pub ss: Option<shadowsocks::config::ServerConfig>,
    pub context: shadowsocks::context::SharedContext,
    pub reality: Option<Arc<crate::security::RealityState>>,
    pub xhttp: Arc<crate::xhttp::State>,
    pub decryption: Option<Arc<meow_proxy::vless_server_decryption::ServerInstance>>,
}
impl Inbound {
    pub fn new(
        options: Listener,
        tls: Option<Arc<tokio_rustls::rustls::ServerConfig>>,
        previous: Option<&Self>,
    ) -> Result<Arc<Self>> {
        let ss = if options.protocol == Protocol::Shadowsocks {
            Some(
                shadowsocks::config::ServerConfig::new(
                    options.listen,
                    options.password.as_deref().unwrap(),
                    options
                        .cipher
                        .as_deref()
                        .unwrap()
                        .parse()
                        .map_err(|_| anyhow::anyhow!("unknown cipher"))?,
                )
                .map_err(|e| anyhow::anyhow!("{e}"))?,
            )
        } else {
            None
        };
        let mut vmess = astra_protocols::vmess::Server::new(
            options
                .users
                .iter()
                .filter_map(|u| u.uuid.map(|v| *v.as_bytes())),
        );
        let mut context =
            shadowsocks::context::Context::new(shadowsocks::config::ServerType::Server);
        context.set_replay_attack_policy(shadowsocks::config::ReplayAttackPolicy::Reject);
        if let Some(previous) = previous {
            vmess = vmess.with_replay_from(&previous.vmess);
        }
        let context = previous
            .map(|p| p.context.clone())
            .unwrap_or_else(|| Arc::new(context));
        let reality = options
            .reality
            .clone()
            .map(|o| {
                crate::security::RealityState::new(o, previous.and_then(|p| p.reality.as_deref()))
            })
            .transpose()?;
        let decryption = if let Some(config) = &options.decryption {
            if previous.is_some_and(|p| p.options.decryption.as_ref() == Some(config)) {
                previous.unwrap().decryption.clone()
            } else {
                Some(crate::encryption::prepare(config)?)
            }
        } else {
            None
        };
        Ok(Arc::new(Self {
            reality,
            decryption,
            xhttp: previous
                .map(|p| p.xhttp.clone())
                .unwrap_or_else(crate::xhttp::State::new),
            options,
            tls,
            vmess,
            ss,
            context,
        }))
    }
}
pub use astra_protocols::accept::secret;
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn vmess_replay_is_rejected_across_configuration_generations() {
        use meow_proxy::vmess::{
            header::{cmd_key, seal_request_header},
            server::Server,
            Security,
        };
        let id = [7u8; 16];
        let metadata = meow_common::Metadata {
            host: "example.test".into(),
            dst_port: 443,
            ..Default::default()
        };
        let header =
            seal_request_header(&cmd_key(&id), Security::Aes128Gcm, &metadata, false).unwrap();
        let original = Server::new([id]);
        let (mut client, stream) = tokio::io::duplex(4096);
        client.write_all(&header.bytes).await.unwrap();
        let accepted = original.accept(Box::new(stream)).await.unwrap();
        assert_eq!(accepted.host, "example.test");
        assert_eq!(accepted.port, 443);
        drop(accepted);
        drop(client);
        let reloaded = Server::new([id]).with_replay_from(&original);
        let (mut client, stream) = tokio::io::duplex(4096);
        client.write_all(&header.bytes).await.unwrap();
        let error = reloaded
            .accept(Box::new(stream))
            .await
            .err()
            .expect("replayed VMess request accepted");
        assert!(error.to_string().contains("replay"));
    }
}
use astra_protocols::accept::Request as Route;
pub async fn serve(
    mut stream: Stream,
    peer: SocketAddr,
    inbound: Arc<Inbound>,
    app: Arc<App>,
) -> Result<()> {
    let _session = app.session()?;
    let timeout = Duration::from_secs(app.config.handshake_timeout_secs);
    let (accepted, mut raw) = match tokio::time::timeout(
        timeout,
        crate::security::accept(stream, &inbound, app.clone()),
    )
    .await??
    {
        crate::security::Accepted::Proxy(stream, raw) => (stream, raw),
        crate::security::Accepted::Cover(stream, remote) => {
            return net::relay(stream, remote, app).await
        }
    };
    stream = if inbound.options.transport == crate::config::Transport::Xhttp {
        accepted
    } else {
        tokio::time::timeout(
            timeout,
            crate::transport::accept(accepted, &inbound.options),
        )
        .await??
    };
    if inbound.options.transport != crate::config::Transport::Tcp {
        raw = None;
    }
    if inbound.options.transport == crate::config::Transport::Xhttp {
        return crate::xhttp::serve(stream, peer, inbound, app).await;
    }
    serve_inner(stream, peer, inbound, app, raw).await
}
pub(crate) async fn serve_inner(
    mut stream: Stream,
    peer: SocketAddr,
    inbound: Arc<Inbound>,
    app: Arc<App>,
    mut raw: Option<crate::security::RawSwitch>,
) -> Result<()> {
    let timeout = Duration::from_secs(app.config.handshake_timeout_secs);
    if let Some(decryption) = &inbound.decryption {
        let (accepted, control) =
            tokio::time::timeout(timeout, crate::encryption::accept(stream, decryption)).await??;
        stream = accepted;
        raw = Some(control);
    }

    let handshake_inbound = inbound.clone();
    let (route, mut stream, vision) = tokio::time::timeout(timeout, async move {
        let inbound = handshake_inbound;
        let mut vision = None;
        let route = match inbound.options.protocol {
            Protocol::Vless => {
                let (route, negotiated) = astra_protocols::vless::accept(
                    &mut stream,
                    &inbound.options.users,
                    inbound.options.multiplex,
                    inbound.options.flow.as_deref(),
                )
                .await?;
                vision = negotiated;
                route
            }
            Protocol::Trojan => {
                astra_protocols::trojan::accept(&mut stream, &inbound.options.users).await?
            }
            Protocol::Vmess => {
                let accepted = inbound.vmess.accept(Box::new(Adapt(stream))).await?;
                stream = Box::new(AdaptBack(accepted.stream));
                let target = Target {
                    host: accepted.host,
                    port: accepted.port,
                };
                if accepted.udp {
                    Route::Udp(target)
                } else {
                    Route::Tcp(target)
                }
            }
            Protocol::Shadowsocks => {
                let cfg = inbound.ss.as_ref().unwrap();
                let (ss, target) =
                    astra_protocols::shadowsocks::accept(stream, inbound.context.clone(), cfg)
                        .await?;
                stream = Box::new(ss);
                Route::Tcp(target)
            }
            Protocol::Http => Route::Tcp(
                astra_protocols::http::accept(&mut stream, &inbound.options.users).await?,
            ),
            Protocol::Socks5 => {
                astra_protocols::socks5::accept(&mut stream, &inbound.options.users).await?
            }
            _ => bail!("QUIC protocol on TCP listener"),
        };
        Ok::<_, anyhow::Error>((route, stream, vision))
    })
    .await??;
    match route {
        Route::MuxCool => {
            ensure!(inbound.options.multiplex, "Mux.Cool disabled");
            stream.write_all(&[0, 0]).await?;
            stream.flush().await?;
            crate::multiplex::cool(stream, app, inbound.options.udp).await
        }
        Route::Tcp(target) => {
            if target.host == "sp.mux.sing-box.arpa" && target.port == 444 {
                ensure!(
                    inbound.options.multiplex && vision.is_none(),
                    "multiplex disabled"
                );
                if inbound.options.protocol == Protocol::Vless {
                    stream.write_all(&[0, 0]).await?;
                    stream.flush().await?;
                }
                return crate::multiplex::sing(stream, app, inbound.options.udp).await;
            }
            let result = app.connect(&target).await;
            match result {
                Ok(remote) => {
                    match inbound.options.protocol {
                        Protocol::Vless => stream.write_all(&[0, 0]).await?,
                        Protocol::Socks5 => {
                            stream.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await?
                        }
                        Protocol::Http => {
                            stream
                                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                                .await?
                        }
                        _ => {}
                    }
                    stream.flush().await?;
                    if let Some(id) = vision {
                        stream = crate::vision::wrap(stream, id, raw);
                    }
                    net::relay(stream, remote, app).await
                }
                Err(error) => {
                    match inbound.options.protocol {
                        Protocol::Socks5 => {
                            let _ = stream.write_all(&[5, 2, 0, 1, 0, 0, 0, 0, 0, 0]).await;
                        }
                        Protocol::Http => {
                            let _ = stream
                                .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
                                .await;
                        }
                        _ => {}
                    }
                    Err(error)
                }
            }
        }
        Route::Udp(target) => {
            ensure!(inbound.options.udp, "UDP disabled");
            if inbound.options.protocol == Protocol::Vless {
                stream.write_all(&[0, 0]).await?;
            }
            framed_udp(stream, target, app).await
        }
        Route::TrojanUdp => {
            ensure!(inbound.options.udp, "UDP disabled");
            trojan_udp(stream, app).await
        }
        Route::SocksUdp => {
            ensure!(inbound.options.udp, "UDP disabled");
            socks_udp(stream, peer, app).await
        }
    }
}
// The outbound library's stream marker is distinct from this server's marker.
pub(crate) struct Adapt(pub Stream);
pub(crate) struct AdaptBack(pub Box<dyn meow_transport::Stream>);
macro_rules! adapt {
    ($name:ident) => {
        impl tokio::io::AsyncRead for $name {
            fn poll_read(
                mut self: std::pin::Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
                b: &mut tokio::io::ReadBuf<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::pin::Pin::new(&mut self.0).poll_read(cx, b)
            }
        }
        impl tokio::io::AsyncWrite for $name {
            fn poll_write(
                mut self: std::pin::Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
                b: &[u8],
            ) -> std::task::Poll<std::io::Result<usize>> {
                std::pin::Pin::new(&mut self.0).poll_write(cx, b)
            }
            fn poll_flush(
                mut self: std::pin::Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::pin::Pin::new(&mut self.0).poll_flush(cx)
            }
            fn poll_shutdown(
                mut self: std::pin::Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::pin::Pin::new(&mut self.0).poll_shutdown(cx)
            }
        }
    };
}
adapt!(Adapt);
adapt!(AdaptBack);

pub use astra_protocols::http::authority_target;

pub async fn framed_udp(stream: Stream, target: Target, app: Arc<App>) -> Result<()> {
    let mut hub = Hub::new(app.clone());
    let (mut rd, mut wr) = tokio::io::split(stream);
    let send = hub.send.clone();
    let input = async {
        loop {
            let n = rd.read_u16().await? as usize;
            let mut payload = vec![0; n];
            rd.read_exact(&mut payload).await?;
            send.send(app.packet(0, target.clone(), payload)?).await?;
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    };
    let output = async {
        while let Some(packet) = hub.receive.recv().await {
            wr.write_u16(packet.payload.len() as u16).await?;
            wr.write_all(&packet.payload).await?;
            wr.flush().await?;
        }
        Ok::<(), anyhow::Error>(())
    };
    tokio::select! {r=input=>r,r=output=>r}
}
pub use crate::protocols::shadowsocks::ss_udp;
use crate::protocols::{socks5::socks_udp, trojan::trojan_udp};
