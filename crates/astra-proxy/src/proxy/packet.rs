//! Shared bounded UDP flow dispatch for packet-oriented TUN and TPROXY inbounds.
use crate::{runtime::Current, udp, App};
use anyhow::{Context, Result};
use astra_ipc::{Inbound, Target};
use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    sync::mpsc,
    task::JoinSet,
    time::{timeout, Instant},
};

pub type Datagram = (Vec<u8>, SocketAddr, SocketAddr);
pub async fn dispatch(
    mut input: mpsc::Receiver<Datagram>,
    output: mpsc::Sender<Datagram>,
    current: Current,
    inbound: Inbound,
) {
    let mut flows = HashMap::<(SocketAddr, SocketAddr), mpsc::Sender<Vec<u8>>>::new();
    let mut workers = JoinSet::new();
    loop {
        tokio::select! {
            Some(_) = workers.join_next(), if !workers.is_empty() => { flows.retain(|_, tx| !tx.is_closed()); }
            item = input.recv() => {
                let Some((body, source, destination)) = item else { break; };
                let key = (source, destination);
                if flows.get(&key).is_some_and(|tx| tx.is_closed()) { flows.remove(&key); }
                if !flows.contains_key(&key) {
                    flows.retain(|_, tx| !tx.is_closed());
                    let app = current.load_full();
                    // A separate bound prevents completed-but-unreaped workers from growing.
                    if workers.len() >= app.config.max_udp_flows { continue; }
                    let Ok(permit) = app.udp_permits.clone().try_acquire_owned() else { continue; };
                    let (tx, rx) = mpsc::channel(4);
                    flows.insert(key, tx);
                    let output = output.clone();
                    workers.spawn(async move {
                        let _permit = permit;
                        if let Err(error) = flow(app, source, destination, rx, output, inbound).await {
                            tracing::debug!(%error, %source, %destination, "packet UDP flow ended");
                        }
                    });
                }
                let _ = flows[&key].try_send(body);
            }
        }
    }
}
async fn flow(
    app: Arc<App>,
    source: SocketAddr,
    destination: SocketAddr,
    mut requests: mpsc::Receiver<Vec<u8>>,
    output: mpsc::Sender<Datagram>,
    inbound: Inbound,
) -> Result<()> {
    let target = Target {
        host: destination.ip().to_string(),
        port: destination.port(),
    };
    let connection = timeout(
        Duration::from_secs(app.config.connect_timeout_secs),
        udp::open(&app, &target, inbound, source, destination),
    )
    .await
    .context("UDP open timeout")??;
    let activity = Arc::new(std::sync::Mutex::new(Instant::now()));
    let upload = async {
        while let Some(packet) = requests.recv().await {
            connection.send(&packet, destination).await?;
            *activity.lock().unwrap() = Instant::now();
        }
        Ok::<_, anyhow::Error>(())
    };
    let download = async {
        let mut packet = vec![0; 65536];
        loop {
            let (size, remote) = connection.recv(&mut packet).await?;
            let remote = if remote.ip().is_unspecified() {
                destination
            } else {
                remote
            };
            output
                .send((packet[..size].to_vec(), remote, source))
                .await?;
            *activity.lock().unwrap() = Instant::now();
        }
        #[allow(unreachable_code)]
        Ok::<_, anyhow::Error>(())
    };
    let expiry = async {
        let duration = Duration::from_secs(app.config.udp_idle_timeout_secs);
        loop {
            let deadline = *activity.lock().unwrap() + duration;
            tokio::time::sleep_until(deadline).await;
            if activity.lock().unwrap().elapsed() >= duration {
                return;
            }
        }
    };
    tokio::select! {
        // Do not cancel a framed receive when a new outgoing datagram arrives.
        result = async { tokio::try_join!(upload, download).map(|_| ()) } => result,
        _ = expiry => Ok(()),
    }
}
