//! SOCKS5 UDP associations; RFC 1928 section 7. No fragment reassembly.
use crate::proxy::udp::{allow_max_datagrams, PacketConnection, MAX_PACKET};
use crate::App;
use anyhow::{bail, Context, Result};
use astra_model::Inbound;
use astra_model::Network;
use astra_model::Target;
use bytes::Bytes;
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::UdpSocket,
    sync::mpsc,
    task::JoinSet,
    time::timeout,
};

const QUEUE_PACKETS: usize = 4;
type Activity = tokio::sync::watch::Sender<Instant>;
fn touch(activity: &Activity) {
    activity.send_replace(Instant::now());
}
async fn idle(activity: Activity, duration: Duration) {
    loop {
        let deadline = *activity.borrow() + duration;
        tokio::time::sleep_until(deadline.into()).await;
        if activity.borrow().elapsed() >= duration {
            return;
        }
    }
}

pub async fn associate_stream<S>(
    mut control: S,
    requested: Target,
    app: Arc<App>,
    peer: SocketAddr,
    local: SocketAddr,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
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
    let socket = UdpSocket::bind(SocketAddr::new(local.ip(), 0)).await?;
    allow_max_datagrams(&socket)?;
    let socket = Arc::new(socket);
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
    let (activity, _) = tokio::sync::watch::channel(Instant::now());
    let idle_time = Duration::from_secs(app.config.udp_idle_timeout_secs);
    let routing = app.routing.clone();
    drop(app);
    let expiry = idle(activity.clone(), idle_time);
    tokio::pin!(expiry);
    let mut workers = JoinSet::new();
    let mut flows: HashMap<Target, mpsc::Sender<Bytes>> = HashMap::new();
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
                    if flows.len() >= routing.config.udp_targets_per_association || workers.len() >= routing.config.udp_targets_per_association { continue; }
                    let Ok(permit) = routing.udp_permits.clone().try_acquire_owned() else { continue; };
                    let (send, receive) = mpsc::channel(QUEUE_PACKETS);
                    flows.insert(target.clone(), send);
                    let (app, socket, activity, target) = (Arc::new(App::routing_view(routing.clone())), socket.clone(), activity.clone(), target.clone());
                    let process = crate::hosted::INGRESS_PROCESS.try_with(Clone::clone).ok();
                    workers.spawn(async move {
                        let _permit = permit;
                        // A hosted association can outlive several core versions.
                        // Keep one resident-host lease per destination so all
                        // workers share the same global UDP limit.
                        let _host_permit = if let Some(host) = &app.worker_host {
                            match host.udp_permit().await {
                                Ok(permit) => Some(permit),
                                Err(error) => {
                                    tracing::debug!(%error, host=%target.host, port=target.port, "host UDP admission rejected");
                                    return;
                                }
                            }
                        } else { None };
                        let session = flow(app, socket, from, &target, receive, activity);
                        let result = if let Some(process) = process { crate::hosted::INGRESS_PROCESS.scope(process,session).await } else { session.await };
                        if let Err(error) = result {
                            tracing::debug!(%error, host=%target.host, port=target.port, "UDP flow ended");
                        }
                    });
                }
                // UDP overload drops packets instead of blocking all destinations.
                // Copy only the payload so a queued datagram never pins the receive buffer.
                let _ = flows[&target].try_send(Bytes::copy_from_slice(body));
            }
        }
    }
    workers.shutdown().await;
    Ok(())
}

pub(crate) async fn open(
    app: &App,
    target: &Target,
    inbound: Inbound,
    source: SocketAddr,
    destination: SocketAddr,
) -> Result<PacketConnection> {
    let session = crate::inbound::Flow::new(
        target,
        inbound,
        Network::Udp,
        Some(source),
        Some(destination),
    )
    .plan(app, crate::inbound::FlowPolicy::Requested)
    .await?;
    session.open_udp(app).await
}

async fn flow(
    app: Arc<App>,
    socket: Arc<UdpSocket>,
    client: SocketAddr,
    target: &Target,
    mut requests: mpsc::Receiver<Bytes>,
    association_activity: Activity,
) -> Result<()> {
    let connection = timeout(
        Duration::from_secs(app.config.connect_timeout_secs),
        open(&app, target, Inbound::Socks5, client, socket.local_addr()?),
    )
    .await
    .context("UDP open timeout")??;
    let idle_time = Duration::from_secs(app.config.udp_idle_timeout_secs);
    let _route_leases = app.route_leases.clone();
    drop(app);
    // Domain-bound adapters carry the original name remotely. Never resolve it locally.
    let destination = SocketAddr::new(
        target
            .host
            .parse()
            .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
        target.port,
    );
    let (activity, _) = tokio::sync::watch::channel(Instant::now());
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
        _ = idle(activity.clone(), idle_time) => Ok(()),
    }
}

use astra_protocols::socks5::udp::decode;

#[cfg(test)]
#[path = "../test/proxy_udp_group_policy_test.rs"]
mod group_policy_tests;
