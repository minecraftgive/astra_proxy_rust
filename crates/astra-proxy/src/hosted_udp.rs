//! One bounded datagram channel per generation-pinned TUN/TPROXY flow.
use super::{receive, send, Auth, Command, Host, Metadata, WorkerHost, WorkerRequest};
use crate::App;
use anyhow::{ensure, Context, Result};
use astra_ipc::{
    host::{receive_datagram, send_datagram},
    platform::LocalStream,
    Inbound, Network, Target,
};
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
        let mut stream = astra_ipc::platform::connect_trusted(&lease.0.endpoint).await?;
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
        let activity = Arc::new(std::sync::Mutex::new(tokio::time::Instant::now()));
        let (mut reader, mut writer) = tokio::io::split(stream);
        let upload = async {
            while let Some(packet) = requests.recv().await {
                send_datagram(&mut writer, &packet).await?;
                *activity.lock().unwrap() = tokio::time::Instant::now();
            }
            Ok::<_, anyhow::Error>(())
        };
        let download = async {
            loop {
                let (remote, packet) = receive_reply(&mut reader).await?;
                *activity.lock().unwrap() = tokio::time::Instant::now();
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
                let deadline = *activity.lock().unwrap() + duration;
                tokio::time::sleep_until(deadline).await;
                if activity.lock().unwrap().elapsed() >= duration {
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
            let mut stream = astra_ipc::platform::connect_trusted(&self.endpoint).await?;
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
    let activity = Arc::new(std::sync::Mutex::new(tokio::time::Instant::now()));
    let (mut reader, mut writer) = tokio::io::split(stream);
    let upload = async {
        loop {
            let packet = receive_datagram(&mut reader).await?;
            connection.send(&packet, destination).await?;
            *activity.lock().unwrap() = tokio::time::Instant::now();
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
            *activity.lock().unwrap() = tokio::time::Instant::now();
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
mod tests {
    use super::*;
    fn app() -> Arc<App> {
        Arc::new(App::from_text(&std::env::temp_dir().join("hosted-udp-test.toml"),
            "standalone=true\nstandalone_outbound='direct'\nsocks5_listen='127.0.0.1:1080'\nmax_udp_flows=1\nudp_targets_per_association=1\n[proxy_groups.direct]\nmembers=['DIRECT']\n".into()).unwrap())
    }

    #[tokio::test]
    async fn worker_channel_preserves_empty_udp_and_releases_flow_on_transport_close() {
        let echo = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let destination = echo.local_addr().unwrap();
        let echo = tokio::spawn(async move {
            let mut buffer = [0; 1024];
            for _ in 0..2 {
                let (size, peer) = echo.recv_from(&mut buffer).await.unwrap();
                echo.send_to(&buffer[..size], peer).await.unwrap();
            }
        });
        let (mut host, worker) = tokio::io::duplex(128);
        let worker = tokio::spawn(serve(
            worker,
            app(),
            Metadata {
                flow_id: None,
                inbound: Inbound::Tun,
                source: "127.0.0.1:45000".parse().unwrap(),
                local: destination,
                destination: Some(destination),
                process: None,
            },
        ));
        let ready: std::result::Result<(), String> = receive(&mut host).await.unwrap();
        ready.unwrap();
        for body in [vec![], vec![7; 512]] {
            // Concurrent read keeps the deliberately tiny test channel bounded.
            let (mut read, mut write) = tokio::io::split(&mut host);
            let (_, response) =
                tokio::join!(send_datagram(&mut write, &body), receive_reply(&mut read));
            let (remote, payload) = response.unwrap();
            assert_eq!(remote, destination);
            assert_eq!(payload, body);
        }
        drop(host);
        assert!(timeout(Duration::from_secs(5), worker)
            .await
            .unwrap()
            .unwrap()
            .is_err());
        echo.await.unwrap();
    }

    #[tokio::test]
    async fn generations_share_udp_quota_until_the_lease_connection_closes() {
        use super::super::{Generation, State};
        use std::sync::atomic::{AtomicU64, AtomicUsize};
        let app = app();
        let current = Arc::new(arc_swap::ArcSwap::from(app.clone()));
        let mut generations = std::collections::BTreeMap::new();
        for id in 1..=2 {
            // Short-lived test-harness children supply real process handles;
            // no worker setup, environment mutation or network takeover occurs.
            let child = tokio::process::Command::new(std::env::current_exe().unwrap())
                .arg("--list")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            generations.insert(
                id,
                Arc::new(Generation {
                    id,
                    directory: std::env::temp_dir(),
                    log: std::env::temp_dir().join(format!("unused-worker-{id}.log")),
                    endpoint: std::env::temp_dir().join(format!("unused-worker-{id}")),
                    token: format!("test-generation-{id}"),
                    child: tokio::sync::Mutex::new(crate::platform::worker::WorkerChild::Normal(
                        child,
                    )),
                    flows: AtomicUsize::new(0),
                    rule_socket: String::new(),
                    snapshot: Default::default(),
                    failed: Default::default(),
                    route_leases: Default::default(),
                }),
            );
        }
        let host = Arc::new(Host {
            boot: "quota-test".into(),
            next_flow: AtomicU64::new(1),
            endpoint: std::env::temp_dir()
                .join(format!("astra-quota-test-{}", uuid::Uuid::new_v4())),
            path: std::env::temp_dir().join("hosted-udp-test.toml"),
            current: Arc::downgrade(&current),
            state: Arc::new(tokio::sync::Mutex::new(State {
                active: 2,
                prepared: None,
                paused: false,
                generations,
            })),
            admissions: Default::default(),
            transition: Default::default(),
            telemetry: Default::default(),
            connection_ids: Default::default(),
            retired: Default::default(),
            tokens: Arc::new(std::sync::Mutex::new(
                [
                    "test-generation-1".to_owned(),
                    "test-generation-2".to_owned(),
                ]
                .into_iter()
                .collect(),
            )),
            mappings: Arc::new(astra_dns::LocalMappingStore::default()),
            sequence: AtomicU64::new(3),
            tasks: Default::default(),
        });
        let admission = Arc::new(tokio::sync::Semaphore::new(1));
        let lease = |host: Arc<Host>,
                     token: &'static str,
                     admission: Arc<tokio::sync::Semaphore>| async move {
            let (mut client, mut server) = tokio::io::duplex(4096);
            let permit = admission.acquire_owned().await.unwrap();
            let task =
                tokio::spawn(async move { host.hold_udp_permit(&mut server, token, permit).await });
            let ready: std::result::Result<(), String> = receive(&mut client).await.unwrap();
            (client, task, ready)
        };
        let (first, first_task, ready) =
            lease(host.clone(), "test-generation-1", admission.clone()).await;
        ready.unwrap();
        // An idle quota connection must leave management admission available.
        let (second, second_task, rejected) = timeout(
            Duration::from_secs(5),
            lease(host.clone(), "test-generation-2", admission.clone()),
        )
        .await
        .unwrap();
        assert!(rejected.unwrap_err().contains("UDP flow limit"));
        drop(second);
        assert!(second_task.await.unwrap().is_err());
        assert!(app.udp_permits.clone().try_acquire_owned().is_err());
        drop(first);
        first_task.await.unwrap().unwrap();
        let (second, second_task, ready) =
            lease(host.clone(), "test-generation-2", admission).await;
        ready.unwrap();
        drop(second);
        second_task.await.unwrap().unwrap();
        assert!(app.udp_permits.clone().try_acquire_owned().is_ok());
    }

    #[tokio::test]
    async fn replies_keep_ipv6_scope_empty_and_maximum_payloads() {
        let remote: SocketAddr = "[fe80::123%7]:443".parse().unwrap();
        let (mut writer, mut reader) = tokio::io::duplex(128);
        let task = tokio::spawn(async move {
            send_reply(&mut writer, remote, &[]).await.unwrap();
            send_reply(
                &mut writer,
                remote,
                &vec![123; astra_ipc::host::MAX_DATAGRAM_BYTES],
            )
            .await
            .unwrap();
        });
        let (address, body) = receive_reply(&mut reader).await.unwrap();
        assert_eq!(address, remote);
        assert!(body.is_empty());
        let (address, body) = receive_reply(&mut reader).await.unwrap();
        assert_eq!(address, remote);
        assert_eq!(body, vec![123; astra_ipc::host::MAX_DATAGRAM_BYTES]);
        task.await.unwrap();
    }
}
