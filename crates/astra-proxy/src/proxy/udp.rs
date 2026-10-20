//! SOCKS5 UDP associations; RFC 1928 section 7. No fragment reassembly.
use crate::App;
use anyhow::{bail, Context, Result};
use astra_ipc::{Inbound, Network, RouteAction, Target};
use meow_common::ProxyPacketConn;
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
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

pub(crate) async fn open(
    app: &App,
    target: &Target,
    inbound: Inbound,
    source: SocketAddr,
    destination: SocketAddr,
) -> Result<PacketConnection> {
    let action = app
        .route_connection(
            target,
            inbound,
            Network::Udp,
            Some(source),
            Some(destination),
        )
        .await?;
    let (action, lease) = app.resolve_session(&action).await?;
    let traffic =
        super::traffic::Traffic::new(Some(source), target, inbound, Network::Udp, &action);
    let result: Result<PacketTransport> = async {
        match action {
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
                    if destination.ip().is_loopback() {
                        None
                    } else {
                        app.config
                            .tun
                            .as_ref()
                            .and_then(|tun| tun.interface.as_deref())
                    },
                    destination.is_ipv4(),
                )?;
                socket.connect(destination).await?;
                Ok(PacketTransport::Direct(socket))
            }
            RouteAction::Proxy(tag) => {
                let outbound = app
                    .native
                    .get(&tag)
                    .with_context(|| format!("outbound {tag} does not support UDP"))?;
                Ok(PacketTransport::Proxy(
                    crate::native::connect_udp(outbound, target).await?,
                ))
            }
        }
    }
    .await;
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
        open(&app, target, Inbound::Socks5, client, socket.local_addr()?),
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

use astra_protocols::socks5::udp::decode;

#[cfg(test)]
mod group_policy_tests {
    use super::*;
    #[tokio::test]
    async fn configured_group_switch_cancels_idle_udp_receive_and_prevents_more_packets() {
        let remote = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let destination = remote.local_addr().unwrap();
        let settings = |selected: &str| {
            format!("standalone=true\nstandalone_outbound='main'\nsocks5_listen='127.0.0.1:1080'\n[proxy_groups.main]\nmode='select'\nmembers=['DIRECT','REJECT']\nselected='{selected}'\ndisconnect_on_manual=true\n")
        };
        let app =
            App::from_text(std::path::Path::new("udp-groups.toml"), settings("DIRECT")).unwrap();
        let target = Target {
            host: destination.ip().to_string(),
            port: destination.port(),
        };
        let connection = open(
            &app,
            &target,
            Inbound::Socks5,
            "127.0.0.1:40000".parse().unwrap(),
            destination,
        )
        .await
        .unwrap();
        connection.send(b"before", destination).await.unwrap();
        let mut bytes = [0; 32];
        let (size, _) = remote.recv_from(&mut bytes).await.unwrap();
        assert_eq!(&bytes[..size], b"before");
        let mut next =
            App::from_text(std::path::Path::new("udp-groups.toml"), settings("REJECT")).unwrap();
        next.groups.inherit(&app, &next.config);
        next.groups.committed(&app, &next.config);
        assert!(timeout(Duration::from_secs(1), connection.recv(&mut bytes))
            .await
            .unwrap()
            .is_err());
        assert!(connection.send(b"after", destination).await.is_err());
        assert!(
            timeout(Duration::from_millis(20), remote.recv_from(&mut bytes))
                .await
                .is_err()
        );
    }
}
