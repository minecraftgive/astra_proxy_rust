//! One bounded datagram channel per generation-pinned TUN/TPROXY flow.
use super::{receive, send, Auth, Command, Host, Metadata, WorkerHost, WorkerRequest};
use crate::App;
use anyhow::{ensure, Context, Result};
use astra_ipc::host::receive_datagram;
use astra_ipc::host::send_datagram;
use astra_model::Inbound;
use astra_model::Network;
use astra_model::Target;
use astra_platform::platform::LocalStream;
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite},
    sync::{mpsc, OwnedSemaphorePermit},
    time::timeout,
};

impl Host {
    /// packet::dispatch owns the instance-wide UDP permit for this entire call.
    pub async fn relay_udp(
        &self,
        metadata: Metadata,
        mut requests: mpsc::Receiver<Vec<u8>>,
        output: mpsc::Sender<crate::packet::Datagram>,
    ) -> Result<()> {
        let source = metadata.source;
        let lease = self.lease().await?;
        let metadata = self.metadata(metadata, lease.0.id);
        let mut stream = astra_platform::platform::connect_trusted(&lease.0.endpoint).await?;
        send(
            &mut stream,
            Auth {
                token: lease.0.token.clone(),
                command: WorkerRequest::Udp(metadata),
            },
        )
        .await?;
        let ready: std::result::Result<(), String> =
            timeout(Duration::from_secs(15), receive(&mut stream)).await??;
        ready.map_err(anyhow::Error::msg)?;
        let app = self.app()?;
        let (activity, _) = tokio::sync::watch::channel(tokio::time::Instant::now());
        let (mut reader, mut writer) = tokio::io::split(stream);
        let upload = async {
            while let Some(packet) = requests.recv().await {
                send_datagram(&mut writer, &packet).await?;
                activity.send_replace(tokio::time::Instant::now());
            }
            Ok::<_, anyhow::Error>(())
        };
        let download = async {
            loop {
                let (remote, packet) = receive_reply(&mut reader).await?;
                activity.send_replace(tokio::time::Instant::now());
                output
                    .send((packet, remote, source))
                    .await
                    .context("UDP ingress closed")?;
            }
            #[allow(unreachable_code)]
            Ok::<_, anyhow::Error>(())
        };
        // Cancelling either framed direction always drops the whole channel;
        // partial reads/writes are never reused for a different datagram.
        let expiry = async {
            let duration = Duration::from_secs(app.config.udp_idle_timeout_secs);
            loop {
                let deadline = *activity.borrow() + duration;
                tokio::time::sleep_until(deadline).await;
                if activity.borrow().elapsed() >= duration {
                    return;
                }
            }
        };
        tokio::select! {
            result = upload => result,
            result = download => result,
            _ = expiry => Ok(()),
            _ = app.egress_cancelled() => Ok(()),
        }
    }

    pub(super) async fn hold_udp_permit<S: AsyncRead + AsyncWrite + Unpin>(
        &self,
        stream: &mut S,
        token: &str,
        admission: OwnedSemaphorePermit,
    ) -> Result<()> {
        let permit = async {
            self.authenticate(token).await?;
            self.app()?
                .udp_permits
                .clone()
                .try_acquire_owned()
                .map_err(|_| anyhow::anyhow!("instance UDP flow limit reached"))
        }
        .await;
        send(
            stream,
            permit
                .as_ref()
                .map(|_| ())
                .map_err(|error| format!("{error:#}")),
        )
        .await?;
        let _permit = permit?;
        // Established leases have their own UDP quota and must not consume the
        // control-plane handshake budget while an association stays alive.
        drop(admission);
        let mut unexpected = [0];
        let _ = stream.read(&mut unexpected).await?;
        Ok(())
    }
}

impl WorkerHost {
    /// Keep the returned authenticated connection alive for the target's life.
    pub(crate) async fn udp_permit(&self) -> Result<LocalStream> {
        timeout(Duration::from_secs(3), async {
            let mut stream = astra_platform::platform::connect_trusted(&self.endpoint).await?;
            send(
                &mut stream,
                Command::UdpPermit {
                    token: self.token.clone(),
                },
            )
            .await?;
            let ready: std::result::Result<(), String> = receive(&mut stream).await?;
            ready.map_err(anyhow::Error::msg)?;
            Ok(stream)
        })
        .await
        .context("host UDP admission timeout")?
    }
}

pub(super) async fn serve<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    app: Arc<App>,
    meta: Metadata,
) -> Result<()> {
    let prepared = async {
        ensure!(
            matches!(meta.inbound, Inbound::Tun | Inbound::Tproxy),
            "unsupported hosted UDP inbound"
        );
        let destination = meta
            .destination
            .context("UDP ingress requires original destination")?;
        let original = Target {
            host: destination.ip().to_string(),
            port: destination.port(),
        };
        let (target, dial) = if meta.inbound == Inbound::Tun {
            crate::proxy::target::select(&app, &original, None)?
        } else {
            (original.clone(), original)
        };
        let mapped = target.host != destination.ip().to_string();
        let action = app
            .route_with_context(
                &target,
                meta.inbound,
                Network::Udp,
                meta.process,
                Some(destination.ip()).filter(|ip| !app.synthetic_ip(*ip)),
            )
            .await?;
        let connection = timeout(
            Duration::from_secs(app.config.connect_timeout_secs),
            crate::udp::open_action(&app, &dial, meta.inbound, meta.source, action),
        )
        .await
        .context("UDP open timeout")??;
        Ok::<_, anyhow::Error>((destination, mapped, connection))
    }
    .await;
    send(
        &mut stream,
        prepared
            .as_ref()
            .map(|_| ())
            .map_err(|error| format!("{error:#}")),
    )
    .await?;
    let (destination, mapped, connection) = prepared?;
    let (activity, _) = tokio::sync::watch::channel(tokio::time::Instant::now());
    let (mut reader, mut writer) = tokio::io::split(stream);
    let upload = async {
        loop {
            let packet = receive_datagram(&mut reader).await?;
            connection.send(&packet, destination).await?;
            activity.send_replace(tokio::time::Instant::now());
        }
        #[allow(unreachable_code)]
        Ok::<_, anyhow::Error>(())
    };
    let download = async {
        let mut buffer = vec![0; 65536];
        loop {
            let (size, remote) = connection.recv(&mut buffer).await?;
            let remote = if mapped || remote.ip().is_unspecified() {
                destination
            } else {
                remote
            };
            send_reply(&mut writer, remote, &buffer[..size]).await?;
            activity.send_replace(tokio::time::Instant::now());
        }
        #[allow(unreachable_code)]
        Ok::<_, anyhow::Error>(())
    };
    let expiry = async {
        let duration = Duration::from_secs(app.config.udp_idle_timeout_secs);
        loop {
            let deadline = *activity.borrow() + duration;
            tokio::time::sleep_until(deadline).await;
            if activity.borrow().elapsed() >= duration {
                return;
            }
        }
    };
    tokio::select! {
        result = upload => result,
        result = download => result,
        _ = expiry => Ok(()),
        _ = app.egress_cancelled() => Ok(()),
    }
}

// Keep address metadata outside the length-limited datagram, so even the
// maximum payload fits and IPv6 scope information is not discarded.
async fn send_reply<S: AsyncWrite + Unpin>(
    stream: &mut S,
    remote: SocketAddr,
    payload: &[u8],
) -> Result<()> {
    send(stream, remote).await?;
    send_datagram(stream, payload).await?;
    Ok(())
}
async fn receive_reply<S: AsyncRead + Unpin>(stream: &mut S) -> Result<(SocketAddr, Vec<u8>)> {
    let remote = receive(stream).await?;
    let payload = receive_datagram(stream).await?;
    Ok((remote, payload))
}

#[cfg(test)]
#[path = "../test/hosted_udp_test.rs"]
mod tests;
