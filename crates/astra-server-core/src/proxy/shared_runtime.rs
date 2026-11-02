use crate::{
    config::{self, Prepared, Protocol},
    net::{App, Stats},
    protocol::Inbound,
};
use anyhow::{ensure, Context, Result};
use astra_server_api::{Action, Reply, Request, VERSION};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{atomic::Ordering, Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;
struct Generation {
    app: Arc<App>,
    inbounds: Vec<Arc<Inbound>>,
}
type Current = Arc<arc_swap::ArcSwap<Generation>>;
type Logs = Arc<Mutex<VecDeque<String>>>;
fn log(logs: &Logs, message: String) {
    eprintln!("{message}");
    let mut lines = logs.lock().unwrap();
    if lines.len() >= 256 {
        lines.pop_front();
    }
    lines.push_back(message);
}
fn generation(
    prepared: Prepared,
    stats: Arc<Stats>,
    sessions: Arc<Semaphore>,
    udp: Arc<Semaphore>,
    bytes: Arc<Semaphore>,
    previous: Option<&Generation>,
) -> Result<Arc<Generation>> {
    let mut inbounds = vec![];
    for (index, (options, tls)) in prepared
        .config
        .listeners
        .iter()
        .cloned()
        .zip(prepared.tls)
        .enumerate()
    {
        inbounds.push(Inbound::new(
            options,
            tls,
            previous.map(|p| p.inbounds[index].as_ref()),
        )?);
    }
    Ok(Arc::new(Generation {
        app: App::new(prepared.config, stats, sessions, udp, bytes),
        inbounds,
    }))
}
pub async fn run(config: PathBuf, control: PathBuf, prepared: Prepared) -> Result<()> {
    let parent = control.parent().context("control directory")?;
    let new_directory = !parent.exists();
    std::fs::create_dir_all(parent)?;
    if new_directory {
        astra_ipc::platform::set_private(parent, 0o700)?;
    }
    // The control endpoint is claimed before any public listener is opened.
    let management = astra_ipc::platform::LocalListener::bind(&control)?;
    astra_ipc::platform::set_private(&control, 0o600)?;
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _cleanup = Cleanup(control.clone());
    let stats = Arc::new(Stats::default());
    let sessions = Arc::new(Semaphore::new(prepared.config.max_connections));
    let udp = Arc::new(Semaphore::new(prepared.config.max_udp_flows));
    let bytes = Arc::new(Semaphore::new(16 * 1024 * 1024));
    let first = generation(
        prepared,
        stats.clone(),
        sessions.clone(),
        udp.clone(),
        bytes.clone(),
        None,
    )?;
    let current: Current = Arc::new(arc_swap::ArcSwap::from(first.clone()));
    // Bind everything first. A failed startup cannot leave a partly active server.
    let mut tcp = vec![];
    let mut quic = vec![];
    let mut ssudp = vec![];
    for (index, inbound) in first.inbounds.iter().enumerate() {
        if inbound.options.protocol.quic() {
            quic.push((index, crate::quic::bind(inbound)?));
        } else {
            tcp.push((
                index,
                tokio::net::TcpListener::bind(inbound.options.listen).await?,
            ));
            if let Some(cfg) = &inbound.ss {
                if inbound.options.udp {
                    ssudp.push((
                        shadowsocks::relay::udprelay::proxy_socket::ProxySocket::bind(
                            inbound.context.clone(),
                            cfg,
                        )
                        .await?,
                        cfg.method().is_aead_2022(),
                    ));
                }
            }
        }
    }
    let logs: Logs = Arc::new(Mutex::new(VecDeque::new()));
    let mut listeners = tokio::task::JoinSet::new();
    let connections = Arc::new(Mutex::new(Vec::<tokio::task::AbortHandle>::new()));
    for (index, listener) in tcp {
        let current = current.clone();
        let logs = logs.clone();
        let connections = connections.clone();
        listeners.spawn(async move {
            loop {
                let Ok((stream, peer)) = listener.accept().await else {
                    break;
                };
                let g = current.load_full();
                let inbound = g.inbounds[index].clone();
                let app = g.app.clone();
                let logs = logs.clone();
                let tag = inbound.options.tag.clone();
                let stats = app.stats.clone();
                let task = tokio::spawn(async move {
                    if let Err(error) =
                        crate::protocol::serve(Box::new(stream), peer, inbound, app).await
                    {
                        stats.rejected.fetch_add(1, Ordering::Relaxed);
                        if std::env::var_os("ASTRA_SERVER_DEBUG").is_some() {
                            eprintln!("Server diagnostic: {error:#}");
                        }
                        log(
                            &logs,
                            format!("{tag}: connection closed or rejected ({peer})"),
                        );
                    }
                });
                let mut tasks = connections.lock().unwrap();
                tasks.retain(|h| !h.is_finished());
                tasks.push(task.abort_handle());
            }
        });
    }
    let mut endpoints = vec![];
    for (index, endpoint) in quic {
        endpoints.push(endpoint.clone());
        let current = current.clone();
        let logs = logs.clone();
        let connections = connections.clone();
        listeners.spawn(async move {
            while let Some(incoming) = endpoint.accept().await {
                let g = current.load_full();
                let inbound = g.inbounds[index].clone();
                let app = g.app.clone();
                let logs = logs.clone();
                let tag = inbound.options.tag.clone();
                let stats = app.stats.clone();
                let task = tokio::spawn(async move {
                    if crate::quic::serve(incoming, inbound, app).await.is_err() {
                        stats.rejected.fetch_add(1, Ordering::Relaxed);
                        log(&logs, format!("{tag}: QUIC session rejected"));
                    }
                });
                let mut tasks = connections.lock().unwrap();
                tasks.retain(|h| !h.is_finished());
                tasks.push(task.abort_handle());
            }
        });
    }
    for (socket, aead2022) in ssudp {
        let app = first.app.clone();
        listeners.spawn(async move {
            let _ = crate::protocol::ss_udp(socket, app, aead2022).await;
        });
    }
    let started = Instant::now();
    log(
        &logs,
        format!("Server ready: {} listeners", first.inbounds.len()),
    );
    let shutdown = astra_ipc::shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _=&mut shutdown=>break,
            accepted=management.accept()=>{let(mut stream,_)=accepted?;let request=tokio::time::timeout(Duration::from_secs(3),astra_ipc::receive_management::<Request>(&mut stream)).await;
                let mut reply=Reply{version:VERSION,pid:std::process::id(),uptime_secs:started.elapsed().as_secs(),active:stats.active.load(Ordering::Relaxed),accepted:stats.accepted.load(Ordering::Relaxed),rejected:stats.rejected.load(Ordering::Relaxed),upload:stats.upload.load(Ordering::Relaxed),download:stats.download.load(Ordering::Relaxed),config:config.clone(),..Default::default()};let mut stop=false;
                match request{Ok(Ok(request))if request.version==VERSION=>match request.action{Action::Status=>{},Action::Logs=>reply.logs=logs.lock().unwrap().iter().cloned().collect::<Vec<_>>().join("\n"),Action::Stop=>stop=true,Action::Reload{path}=>{
                    let result=reload(&config,path,&current);if let Err(error)=result{reply.error=Some(error.to_string())}else{log(&logs,"Configuration reloaded".into());}
                }},_=>reply.error=Some("invalid management request".into())}
                reply.listeners=current.load().inbounds.iter().map(|v|format!("{} {:?} {}",v.options.tag,v.options.protocol,v.options.listen)).collect();let _=tokio::time::timeout(Duration::from_secs(3),astra_ipc::send_management(&mut stream,&reply)).await;if stop{break}
            },
        }
    }
    listeners.abort_all();
    for endpoint in endpoints {
        endpoint.close(0u32.into(), b"server stopping");
    }
    let deadline =
        Instant::now() + Duration::from_secs(current.load().app.config.shutdown_timeout_secs);
    while stats.active.load(Ordering::Relaxed) > 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    for task in connections.lock().unwrap().drain(..) {
        task.abort();
    }
    log(&logs, "Server stopped".into());
    Ok(())
}

fn reload(config: &std::path::Path, path: Option<PathBuf>, current: &Current) -> Result<()> {
    ensure!(
        path.as_ref().is_none_or(|p| p == config),
        "reload uses the current configuration path"
    );
    let prepared = config::load(config)?;
    let old = current.load_full();
    ensure!(
        prepared.config.max_connections == old.app.config.max_connections
            && prepared.config.max_udp_flows == old.app.config.max_udp_flows,
        "session limit changes require restart"
    );
    ensure!(
        prepared.config.listeners.len() == old.inbounds.len(),
        "listener changes require restart"
    );
    for (new, previous) in prepared.config.listeners.iter().zip(&old.inbounds) {
        ensure!(
            new.tag == previous.options.tag
                && new.protocol == previous.options.protocol
                && new.listen == previous.options.listen
                && new.udp == previous.options.udp,
            "listener changes require restart"
        );
        if new.protocol.quic() || new.protocol == Protocol::Shadowsocks {
            ensure!(
                toml::to_string(new)? == toml::to_string(&previous.options)?,
                "QUIC/Shadowsocks listener changes require restart"
            );
        }
        if new.protocol == Protocol::Shadowsocks && new.udp {
            ensure!(
                prepared.config.allow_private_targets == old.app.config.allow_private_targets
                    && prepared.config.deny_cidrs == old.app.config.deny_cidrs
                    && prepared.config.idle_timeout_secs == old.app.config.idle_timeout_secs
                    && prepared.config.connect_timeout_secs == old.app.config.connect_timeout_secs,
                "Shadowsocks UDP policy changes require restart"
            );
        }
    }
    let new = generation(
        prepared,
        old.app.stats.clone(),
        old.app.sessions.clone(),
        old.app.udp.clone(),
        old.app.bytes.clone(),
        Some(&old),
    )?;
    current.store(new);
    Ok(())
}
