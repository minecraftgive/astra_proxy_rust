use crate::App;
use crate::{inbound, platform::transparent, runtime::Current};
use anyhow::Result;
use astra_model::{Inbound, Target};
use meow_common::ErrorBackoff;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;

pub(crate) async fn accept(
    listener: Arc<TcpListener>,
    inbound: Inbound,
    current: Current,
) -> std::io::Result<()> {
    let mut backoff = ErrorBackoff::new();
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(value) => {
                backoff.succeeded();
                value
            }
            Err(error) => {
                if !astra_platform::supervision::transient_io(&error) {
                    return Err(error);
                }
                if backoff.failed(&error).await {
                    tracing::warn!(%error, ?inbound, "TCP accept error");
                } else {
                    tracing::debug!(%error, ?inbound, "TCP accept error");
                }
                continue;
            }
        };
        let app = current.load_full();
        if matches!(
            inbound,
            Inbound::Socks5 | Inbound::HttpConnect | Inbound::Mixed
        ) && !app.inbound_allowed(peer.ip())
        {
            continue;
        }
        let Ok(permit) = app.permits.clone().acquire_owned().await else {
            return Ok(());
        };
        let _ = stream.set_nodelay(true);
        tokio::spawn(async move {
            let _permit = permit;
            let cancellation = app.egress_signal.clone();
            let session = async move {
                if let Some(host) = app.hosted.clone() {
                    drop(app);
                    let local = stream.local_addr()?;
                    let destination = match inbound {
                        Inbound::Tproxy | Inbound::Redirect => {
                            Some(transparent::destination(&stream, inbound)?)
                        }
                        _ => None,
                    };
                    let process = crate::platform::process::lookup(
                        astra_model::Network::Tcp,
                        peer,
                        destination.unwrap_or(local),
                    )
                    .await;
                    return host
                        .relay(
                            stream,
                            crate::hosted::Metadata {
                                flow_id: None,
                                inbound,
                                source: peer,
                                local,
                                destination,
                                process,
                            },
                        )
                        .await;
                }
                match inbound {
                    Inbound::Socks5 => inbound::handle_socks5(stream, app).await,
                    Inbound::HttpConnect => inbound::handle_http(stream, app).await,
                    Inbound::Mixed => inbound::handle_mixed(stream, app).await,
                    Inbound::Tproxy | Inbound::Redirect => {
                        transparent::tcp(stream, app, inbound).await
                    }
                    Inbound::Tun | Inbound::ProcessCapture => unreachable!(),
                }
            };
            let result = tokio::select! {
                biased;
                _=cancellation.cancelled()=>Err(anyhow::anyhow!("physical egress changed; reconnect session")),
                result=session=>result,
            };
            if let Err(error) = result {
                tracing::debug!(%peer, %error, "session ended");
            }
        });
    }
}

pub(crate) async fn transparent_stream<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    app: Arc<App>,
    inbound: Inbound,
    source: SocketAddr,
    destination: SocketAddr,
) -> Result<()> {
    let original = Target {
        host: destination.ip().to_string(),
        port: destination.port(),
    };
    let sniff = inbound == Inbound::Tun
        && app.config.tun.as_ref().is_none_or(|tun| tun.sniff)
        && matches!(destination.port(), 80 | 443 | 8080 | 8443);
    let (prefix, hostname) = if sniff {
        super::target::preface(&mut stream).await?
    } else {
        (vec![], None)
    };
    let session = super::Flow::new(
        &original,
        inbound,
        astra_model::Network::Tcp,
        Some(source),
        Some(destination),
    )
    .plan(&app, super::FlowPolicy::RecoverName(hostname))
    .await?;
    let mut remote = session.connect_tcp(&app).await?;
    let _route_leases = app.route_leases.clone();
    drop(app);
    remote.write_all(&prefix).await?;
    drop(prefix);
    crate::proxy::relay::relay(&mut stream, &mut remote).await?;
    Ok(())
}
