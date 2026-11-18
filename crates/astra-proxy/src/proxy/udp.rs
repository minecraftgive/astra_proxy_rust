//! Connected UDP transports and their session leases.
use crate::App;
use anyhow::{bail, Context, Result};
use astra_model::{Inbound, Network, RouteAction, Target};
use meow_common::ProxyPacketConn;
use std::net::SocketAddr;
use tokio::net::UdpSocket;

pub(crate) const MAX_PACKET: usize = 65507;

/// macOS rejects UDP sends larger than the socket send buffer (9216 bytes by
/// default) with EMSGSIZE. The relay must carry every datagram up to
/// [`MAX_PACKET`]; larger platform defaults (Linux) are left untouched.
pub(crate) fn allow_max_datagrams(socket: &UdpSocket) -> std::io::Result<()> {
    let socket = socket2::SockRef::from(socket);
    if socket.send_buffer_size()? < MAX_PACKET {
        socket.set_send_buffer_size(MAX_PACKET)?;
    }
    Ok(())
}
pub(crate) struct PacketConnection {
    transport: PacketTransport,
    traffic: Option<super::traffic::Traffic>,
    lease: super::group_sessions::Lease,
}
enum PacketTransport {
    Direct(UdpSocket),
    Proxy(Box<dyn ProxyPacketConn>),
}
impl Drop for PacketTransport {
    fn drop(&mut self) {
        if let Self::Proxy(conn) = self {
            let _ = conn.close();
        }
    }
}
impl PacketConnection {
    pub(crate) async fn send(&self, bytes: &[u8], target: SocketAddr) -> Result<()> {
        let result: Result<usize> = tokio::select! {
            biased;
            _ = self.lease.cancelled() => bail!("proxy group switched; UDP flow closed"),
            _ = super::traffic::Traffic::cancelled(&self.traffic) => bail!("UDP flow closed by user"),
            result = async { match &self.transport {
            PacketTransport::Direct(socket) => socket.send(bytes).await.map_err(Into::into),
            PacketTransport::Proxy(socket) => socket
                .write_packet(bytes, &target)
                .await
                .map_err(Into::into),
        }} => result,
        };
        let size = match result {
            Ok(size) if size == bytes.len() => size,
            result => {
                let error = result
                    .err()
                    .unwrap_or_else(|| anyhow::anyhow!("partial UDP write"));
                if let Some(traffic) = &self.traffic {
                    traffic.failed(&error);
                }
                return Err(error);
            }
        };
        if let Some(traffic) = &self.traffic {
            traffic.upload(size);
        }
        Ok(())
    }
    pub(crate) async fn recv(&self, bytes: &mut [u8]) -> Result<(usize, SocketAddr)> {
        let result: Result<_> = tokio::select! {
            biased;
            _ = self.lease.cancelled() => bail!("proxy group switched; UDP flow closed"),
            _ = super::traffic::Traffic::cancelled(&self.traffic) => bail!("UDP flow closed by user"),
            result = async {
            Ok(match &self.transport {
                PacketTransport::Direct(socket) => (socket.recv(bytes).await?, socket.peer_addr()?),
                PacketTransport::Proxy(socket) => socket.read_packet(bytes).await?,
            })
            } => result,
        };
        if let Some(traffic) = &self.traffic {
            match &result {
                Ok((size, _)) => traffic.download(*size),
                Err(error) => traffic.failed(error),
            }
        }
        result
    }
}

pub(crate) async fn open_planned(
    app: &App,
    target: &Target,
    inbound: Inbound,
    source: SocketAddr,
    action: RouteAction,
    lease: super::group_sessions::Lease,
) -> Result<PacketConnection> {
    let interface = lease.1.as_deref().or_else(|| {
        app.config
            .tun
            .as_ref()
            .and_then(|tun| tun.interface.as_deref())
    });
    let traffic = super::traffic::Traffic::new(
        &app.metrics,
        Some(source),
        target,
        inbound,
        Network::Udp,
        &action,
    );
    if let Some(traffic) = &traffic {
        traffic.set_path(app.describe_chain(&action, &lease));
    }
    let result: Result<PacketTransport> =
        meow_common::outbound_iface::with_outbound_interface(interface, async {
            match action {
                RouteAction::Block => bail!("UDP blocked"),
                RouteAction::Direct => {
                    let mut destination = app
                        .lookup_real(&target.host, target.port)
                        .await?
                        .into_iter()
                        .next()
                        .context("UDP destination has no addresses")?;
                    if interface.is_some() || app.config.tun.is_some() {
                        destination = crate::egress::scoped_destination(destination, interface)?;
                    }
                    let socket = UdpSocket::bind(if destination.is_ipv4() {
                        "0.0.0.0:0"
                    } else {
                        "[::]:0"
                    })
                    .await?;
                    allow_max_datagrams(&socket)?;
                    crate::egress::bind_interface(
                        &socket,
                        if destination.ip().is_loopback() && inbound != Inbound::ProcessCapture {
                            None
                        } else {
                            interface
                        },
                        destination.is_ipv4(),
                    )?;
                    socket.connect(destination).await?;
                    Ok(PacketTransport::Direct(socket))
                }
                RouteAction::Proxy(tag) => {
                    let outbound = app
                        .chain_adapter(&lease.3, interface, lease.epochs())
                        .with_context(|| format!("outbound {tag} chain unavailable"))?;
                    Ok(PacketTransport::Proxy(
                        crate::native::connect_udp(&outbound, target).await?,
                    ))
                }
            }
        })
        .await?;
    match result {
        Ok(transport) => {
            if let Some(traffic) = &traffic {
                traffic.connected();
            }
            Ok(PacketConnection {
                transport,
                traffic,
                lease,
            })
        }
        Err(error) => {
            if let Some(traffic) = &traffic {
                traffic.failed(&error);
            }
            Err(error)
        }
    }
}
