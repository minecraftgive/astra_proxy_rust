//! SOCKS5 UDP associations; RFC 1928 section 7. No fragment reassembly.
use crate::App;
use anyhow::{bail, Context, Result};
use astra_ipc::{Inbound, Network, RouteAction, Target};
use meow_common::ProxyPacketConn;
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpStream, UdpSocket},
    sync::mpsc,
    task::JoinSet,
    time::timeout,
};

const MAX_PACKET: usize = 65507;
const QUEUE_PACKETS: usize = 4;
type Activity = Arc<Mutex<Instant>>;
fn touch(activity: &Activity) {
    *activity.lock().unwrap() = Instant::now();
}
async fn idle(activity: Activity, duration: Duration) {
    loop {
        let deadline = *activity.lock().unwrap() + duration;
        tokio::time::sleep_until(deadline.into()).await;
        if activity.lock().unwrap().elapsed() >= duration {
            return;
        }
    }
}

pub async fn associate(mut control: TcpStream, requested: Target, app: Arc<App>) -> Result<()> {
    let peer = control.peer_addr()?;
    // Never accept a UDP source from an IP other than the TCP control peer.
    if let Ok(ip) = requested.host.parse::<IpAddr>() {
        if !ip.is_unspecified() && ip != peer.ip() {
            control.write_all(&[5, 2, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
            bail!("UDP source differs from control peer");
        }
    } else {
        control.write_all(&[5, 8, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
        bail!("UDP association source must be an IP address");
    }
    let socket = Arc::new(UdpSocket::bind(SocketAddr::new(control.local_addr()?.ip(), 0)).await?);
    let local = socket.local_addr()?;
    let mut reply = vec![5, 0, 0];
    crate::outbound::append_socks_address(
        &mut reply,
        &Target {
            host: local.ip().to_string(),
            port: local.port(),
        },
    )?;
    control.write_all(&reply).await?;
    let mut source = (requested.port != 0).then(|| SocketAddr::new(peer.ip(), requested.port));
    let activity = Arc::new(Mutex::new(Instant::now()));
    let idle_time = Duration::from_secs(app.config.udp_idle_timeout_secs);
    let expiry = idle(activity.clone(), idle_time);
    tokio::pin!(expiry);
    let mut workers = JoinSet::new();
    let mut flows: HashMap<Target, mpsc::Sender<Vec<u8>>> = HashMap::new();
    let mut packet = vec![0; 65536];
    let mut control_byte = [0];
    loop {
        tokio::select! {
            // Terminating the control connection terminates every UDP task.
            _ = control.read(&mut control_byte) => break,
            _ = &mut expiry => break,
            Some(_) = workers.join_next(), if !workers.is_empty() => {
                flows.retain(|_, sender| !sender.is_closed());
            }
            received = socket.recv_from(&mut packet) => {
                let (len, from) = received?;
                if from.ip() != peer.ip() || source.is_some_and(|s| s != from) { continue; }
                let Ok((target, body)) = decode(&packet[..len]) else { continue; };
                // Pin the port only after a well-formed unfragmented datagram.
                source = Some(from);
                touch(&activity);
                if flows.get(&target).is_some_and(|s| s.is_closed()) { flows.remove(&target); }
                if !flows.contains_key(&target) {
                    flows.retain(|_, s| !s.is_closed());
                    if flows.len() >= app.config.udp_targets_per_association || workers.len() >= app.config.udp_targets_per_association { continue; }
                    let Ok(permit) = app.udp_permits.clone().try_acquire_owned() else { continue; };
                    let (send, receive) = mpsc::channel(QUEUE_PACKETS);
                    flows.insert(target.clone(), send);
                    let (app, socket, activity, target) = (app.clone(), socket.clone(), activity.clone(), target.clone());
                    workers.spawn(async move {
                        let _permit = permit;
                        if let Err(error) = flow(app, socket, from, &target, receive, activity).await {
                            tracing::debug!(%error, host=%target.host, port=target.port, "UDP flow ended");
                        }
                    });
                }
                // UDP overload drops packets instead of blocking all destinations.
                let _ = flows[&target].try_send(body.to_vec());
            }
        }
    }
    workers.shutdown().await;
    Ok(())
}

pub(crate) enum PacketConnection {
    Direct(UdpSocket),
    Proxy(Box<dyn ProxyPacketConn>),
}
impl Drop for PacketConnection {
    fn drop(&mut self) {
        if let Self::Proxy(conn) = self {
            let _ = conn.close();
        }
    }
}
impl PacketConnection {
    pub(crate) async fn send(&self, bytes: &[u8], target: SocketAddr) -> Result<()> {
        let n = match self {
            Self::Direct(s) => s.send(bytes).await?,
            Self::Proxy(s) => s.write_packet(bytes, &target).await?,
        };
        if n != bytes.len() {
            bail!("partial UDP write");
        }
        Ok(())
    }
    pub(crate) async fn recv(&self, bytes: &mut [u8]) -> Result<(usize, SocketAddr)> {
        Ok(match self {
            Self::Direct(s) => (s.recv(bytes).await?, s.peer_addr()?),
            Self::Proxy(s) => s.read_packet(bytes).await?,
        })
    }
}

pub(crate) async fn open(app: &App, target: &Target, inbound: Inbound) -> Result<PacketConnection> {
    match app.route_network(target, inbound, Network::Udp).await? {
        RouteAction::Block => bail!("UDP blocked"),
        RouteAction::Direct => {
            let destination = tokio::net::lookup_host((target.host.as_str(), target.port))
                .await?
                .next()
                .context("UDP destination has no addresses")?;
            let socket = UdpSocket::bind(if destination.is_ipv4() {
                "0.0.0.0:0"
            } else {
                "[::]:0"
            })
            .await?;
            crate::egress::bind_interface(
                &socket,
                app.config
                    .tun
                    .as_ref()
                    .and_then(|tun| tun.interface.as_deref()),
                destination.is_ipv4(),
            )?;
            socket.connect(destination).await?;
            Ok(PacketConnection::Direct(socket))
        }
        RouteAction::Proxy(tag) => {
            let outbound = app
                .native
                .get(&tag)
                .with_context(|| format!("outbound {tag} does not support UDP"))?;
            Ok(PacketConnection::Proxy(
                crate::native::connect_udp(outbound, target).await?,
            ))
        }
    }
}

async fn flow(
    app: Arc<App>,
    socket: Arc<UdpSocket>,
    client: SocketAddr,
    target: &Target,
    mut requests: mpsc::Receiver<Vec<u8>>,
    association_activity: Activity,
) -> Result<()> {
    let connection = timeout(
        Duration::from_secs(app.config.connect_timeout_secs),
        open(&app, target, Inbound::Socks5),
    )
    .await
    .context("UDP open timeout")??;
    // Domain-bound adapters carry the original name remotely. Never resolve it locally.
    let destination = SocketAddr::new(
        target
            .host
            .parse()
            .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
        target.port,
    );
    let activity = Arc::new(Mutex::new(Instant::now()));
    let upload = async {
        while let Some(packet) = requests.recv().await {
            connection.send(&packet, destination).await?;
            touch(&activity);
        }
        Ok::<_, anyhow::Error>(())
    };
    let download = async {
        let mut bytes = vec![0; 65536];
        loop {
            let (len, remote) = connection.recv(&mut bytes).await?;
            let sender = if remote.ip().is_unspecified() {
                target.clone()
            } else {
                Target {
                    host: remote.ip().to_string(),
                    port: remote.port(),
                }
            };
            let mut reply = Vec::with_capacity(len + 32);
            reply.extend_from_slice(&[0, 0, 0]);
            crate::outbound::append_socks_address(&mut reply, &sender)?;
            if reply.len() + len > MAX_PACKET {
                continue;
            }
            reply.extend_from_slice(&bytes[..len]);
            socket.send_to(&reply, client).await?;
            touch(&activity);
            touch(&association_activity);
        }
        #[allow(unreachable_code)]
        Ok::<_, anyhow::Error>(())
    };
    // Reads are not cancelled for every outgoing packet: framed stream adapters may
    // have consumed part of a length prefix. Cancel only when destroying the flow.
    tokio::select! {
        result = async { tokio::try_join!(upload, download).map(|_| ()) } => result,
        _ = idle(activity.clone(), Duration::from_secs(app.config.udp_idle_timeout_secs)) => Ok(()),
    }
}

fn decode(packet: &[u8]) -> Result<(Target, &[u8])> {
    if packet.len() > MAX_PACKET || packet.len() < 4 || packet[..3] != [0, 0, 0] {
        bail!("invalid or fragmented UDP packet");
    }
    let mut cursor = std::io::Cursor::new(&packet[4..]);
    let host = match packet[3] {
        1 => {
            let mut ip = [0; 4];
            std::io::Read::read_exact(&mut cursor, &mut ip)?;
            Ipv4Addr::from(ip).to_string()
        }
        4 => {
            let mut ip = [0; 16];
            std::io::Read::read_exact(&mut cursor, &mut ip)?;
            Ipv6Addr::from(ip).to_string()
        }
        3 => {
            let mut size = [0];
            std::io::Read::read_exact(&mut cursor, &mut size)?;
            if size[0] == 0 || size[0] > 253 {
                bail!("invalid UDP hostname length");
            }
            let mut host = vec![0; size[0] as usize];
            std::io::Read::read_exact(&mut cursor, &mut host)?;
            String::from_utf8(host)?
        }
        _ => bail!("invalid UDP address type"),
    };
    let mut port = [0; 2];
    std::io::Read::read_exact(&mut cursor, &mut port)?;
    let port = u16::from_be_bytes(port);
    if port == 0 {
        bail!("UDP destination port is zero");
    }
    Ok((
        Target { host, port },
        &packet[4 + cursor.position() as usize..],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn udp_headers_reject_truncation_fragments_and_preserve_empty_payloads() {
        for host in ["127.0.0.1", "::1", "example.test"] {
            let expected = Target {
                host: host.into(),
                port: 53,
            };
            let mut packet = vec![0, 0, 0];
            crate::outbound::append_socks_address(&mut packet, &expected).unwrap();
            for size in 0..packet.len() {
                assert!(decode(&packet[..size]).is_err());
            }
            let (target, body) = decode(&packet).unwrap();
            assert_eq!(target, expected);
            assert!(body.is_empty());
            packet.extend_from_slice(b"payload");
            assert_eq!(decode(&packet).unwrap().1, b"payload");
            packet[2] = 1;
            assert!(decode(&packet).is_err());
            packet[2] = 0;
            packet[0] = 1;
            assert!(decode(&packet).is_err());
        }
    }
}
