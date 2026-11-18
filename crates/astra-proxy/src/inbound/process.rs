//! Private data channel for OS capture backends. One stream represents one
//! TCP connection or one UDP destination. Existing streams pin their egress.
use crate::{proxy::session::Binding, runtime::Current, App};
use anyhow::{ensure, Context, Result};
use astra_model::{Inbound, Network, Target};
use astra_platform::platform::{LocalListener, LocalStream};
use serde::Deserialize;
use std::{io, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Open {
    version: u8,
    source_pid: u32,
    network: Network,
    target: Target,
    egress: Binding,
}

pub(crate) fn listener() -> io::Result<Option<LocalListener>> {
    let Some(path) = std::env::var_os("ASTRA_PROCESS_INGRESS").map(PathBuf::from) else {
        return Ok(None);
    };
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "process ingress needs an absolute private endpoint",
        ));
    }
    let listener = astra_platform::platform::bind_listener(&path)?;
    astra_platform::platform::set_private(&path, 0o600)?;
    Ok(Some(listener))
}

pub(crate) async fn serve(
    listener: Option<LocalListener>,
    current: Current,
    sessions: &mut tokio::task::JoinSet<()>,
) -> io::Result<()> {
    let Some(listener) = listener else {
        return std::future::pending().await;
    };
    loop {
        tokio::select! {
            Some(_) = sessions.join_next(), if !sessions.is_empty() => {}
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                if !crate::platform::capture_peer_allowed(&stream) { continue; }
                let app = current.load_full();
                let Ok(permit) = app.permits.clone().try_acquire_owned() else { continue };
                sessions.spawn(async move {
                    let _permit = permit;
                    if let Err(error) = session(stream, app).await {
                        tracing::debug!(%error, "process capture session ended");
                    }
                });
            }
        }
    }
}

async fn session(mut stream: LocalStream, app: Arc<App>) -> Result<()> {
    let header: Open = tokio::time::timeout(Duration::from_secs(5), async {
        let size = stream.read_u32().await? as usize;
        ensure!(
            (1..=8192).contains(&size),
            "invalid process ingress header size"
        );
        let mut bytes = vec![0; size];
        stream.read_exact(&mut bytes).await?;
        Ok::<_, anyhow::Error>(serde_json::from_slice(&bytes)?)
    })
    .await??;
    ensure!(
        header.version == 1
            && header.target.port != 0
            && !header.target.host.is_empty()
            && header.target.host.len() <= 253,
        "invalid process ingress target or version"
    );
    ensure!(
        app.hosted.is_none(),
        "process capture requires an in-process forwarding core"
    );
    ensure!(
        header.source_pid > 0 && !astra_process_routing::owner::own_process(header.source_pid),
        "recaptured Astra forwarder"
    );
    let session = super::Flow::new(
        &header.target,
        Inbound::ProcessCapture,
        header.network,
        None,
        None,
    )
    .plan(&app, super::FlowPolicy::Bound(header.egress))
    .await?;
    let cancel = app.egress_signal.clone();
    let _route_leases = app.route_leases.clone();
    match header.network {
        Network::Tcp => {
            let mut remote = session.connect_tcp(&app).await?;
            drop(app);
            stream.write_u8(1).await?;
            tokio::select! {
                result = crate::proxy::relay::relay(&mut stream, &mut remote) => { result?; }
                _ = cancel.cancelled() => {}
            }
        }
        Network::Udp => {
            let _permit = app
                .udp_permits
                .clone()
                .try_acquire_owned()
                .map_err(|_| anyhow::anyhow!("UDP flow limit reached"))?;
            let destination: SocketAddr = match header.target.host.parse::<std::net::IpAddr>() {
                Ok(ip) => (ip, header.target.port).into(),
                Err(_) => tokio::time::timeout(
                    Duration::from_secs(app.config.connect_timeout_secs),
                    app.lookup_real(&header.target.host, header.target.port),
                )
                .await??
                .into_iter()
                .next()
                .context("no address for captured UDP flow")?,
            };
            let connection = tokio::time::timeout(
                Duration::from_secs(app.config.connect_timeout_secs),
                session.open_udp(&app),
            )
            .await??;
            let idle = Duration::from_secs(app.config.udp_idle_timeout_secs);
            drop(app);
            stream.write_u8(1).await?;
            let (mut read, mut write) = stream.split();
            let (activity, mut last) = tokio::sync::watch::channel(tokio::time::Instant::now());
            let upload = async {
                let mut data = vec![0; 65535];
                loop {
                    let len = read.read_u16().await? as usize;
                    read.read_exact(&mut data[..len]).await?;
                    connection.send(&data[..len], destination).await?;
                    activity.send_replace(tokio::time::Instant::now());
                }
                #[allow(unreachable_code)]
                Ok::<(), anyhow::Error>(())
            };
            let download = async {
                let mut data = vec![0; 65535];
                loop {
                    let (size, _) = connection.recv(&mut data).await?;
                    write.write_u16(size.try_into()?).await?;
                    write.write_all(&data[..size]).await?;
                    activity.send_replace(tokio::time::Instant::now());
                }
                #[allow(unreachable_code)]
                Ok::<(), anyhow::Error>(())
            };
            let expiry = async {
                loop {
                    let deadline = *last.borrow_and_update() + idle;
                    tokio::select! {
                        _ = tokio::time::sleep_until(deadline) => return,
                        _ = last.changed() => {}
                    }
                }
            };
            tokio::select! {
                result = upload => result?,
                result = download => result?,
                _ = expiry => {}
                _ = cancel.cancelled() => {}
            }
        }
    }
    Ok(())
}
