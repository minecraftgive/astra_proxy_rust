use crate::{
    config::{self, Prepared, Protocol},
    net::{App, Stats},
    protocol::Inbound,
};
use anyhow::{ensure, Context, Result};
use astra_management::server::{Action, Reply, Request, VERSION};
use astra_platform::supervision::{transient_io, Task};
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
    digest: u64,
}
type Current = Arc<arc_swap::ArcSwap<Generation>>;
type Logs = Arc<Mutex<VecDeque<String>>>;
const BUFFER_BUDGET: usize = 16 * 1024 * 1024;

fn internal_snapshot(
    app: &App,
    listeners: usize,
    tasks: usize,
    log_lines: usize,
) -> astra_model::monitor::InternalSnapshot {
    use astra_model::monitor::{
        Metric,
        MetricKind::{Counter, Gauge},
    };
    let mut result = astra_observe::snapshot(astra_platform::lifecycle::boot_id());
    let sampled_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .to_string();
    let mut metric = |name: &str, value: u64, kind, unit: &str, description: &str| {
        result.metrics.push(Metric {
            sampled_at_ms: Some(sampled_at_ms.clone()),
            name: name.into(),
            value: value.to_string(),
            kind,
            unit: unit.into(),
            description: description.into(),
        });
    };
    metric(
        "server.sessions.accepted",
        app.stats.accepted.load(Ordering::Relaxed),
        Counter,
        "count",
        "服务端累计接受会话",
    );
    metric(
        "server.sessions.rejected",
        app.stats.rejected.load(Ordering::Relaxed),
        Counter,
        "count",
        "服务端累计拒绝或出错的会话",
    );
    metric(
        "server.sessions.active",
        app.stats.active.load(Ordering::Relaxed),
        Gauge,
        "count",
        "服务端当前活跃会话",
    );
    metric(
        "server.sessions.available",
        app.sessions.available_permits() as u64,
        Gauge,
        "count",
        "可用会话许可",
    );
    metric(
        "server.sessions.capacity",
        app.config.max_connections as u64,
        Gauge,
        "count",
        "会话许可上限",
    );
    metric(
        "server.udp.available",
        app.udp.available_permits() as u64,
        Gauge,
        "count",
        "可用 UDP 流许可",
    );
    metric(
        "server.udp.capacity",
        app.config.max_udp_flows as u64,
        Gauge,
        "count",
        "UDP 流许可上限",
    );
    let available = app.bytes.available_permits();
    metric(
        "server.udp.buffer_available",
        available as u64,
        Gauge,
        "bytes",
        "UDP 缓冲预算剩余额度",
    );
    metric(
        "server.udp.buffer_used",
        BUFFER_BUDGET.saturating_sub(available) as u64,
        Gauge,
        "bytes",
        "UDP 包正在占用的缓冲预算（非堆内存统计）",
    );
    metric(
        "server.udp.buffer_capacity",
        BUFFER_BUDGET as u64,
        Gauge,
        "bytes",
        "UDP 缓冲预算上限",
    );
    metric(
        "server.listeners",
        listeners as u64,
        Gauge,
        "count",
        "已配置监听器数量",
    );
    metric(
        "server.connection_tasks",
        tasks as u64,
        Gauge,
        "count",
        "尚未结束的 TCP / QUIC 连接任务",
    );
    metric(
        "server.log_buffer_lines",
        log_lines as u64,
        Gauge,
        "count",
        "内存日志缓冲行数",
    );
    result
}
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
        digest: prepared.digest,
    }))
}
pub async fn run(config: PathBuf, control: PathBuf, prepared: Prepared) -> Result<()> {
    run_supervised(config, control, prepared, Vec::new()).await
}
async fn run_supervised(
    config: PathBuf,
    control: PathBuf,
    prepared: Prepared,
    mut listeners: Vec<Task>,
) -> Result<()> {
    let candidate = astra_platform::lifecycle::Candidate::from_env()?;
    let parent = control.parent().context("control directory")?;
    let new_directory = !parent.exists();
    std::fs::create_dir_all(parent)?;
    if new_directory {
        astra_platform::platform::set_private(parent, 0o700)?;
    }
    // The control endpoint is claimed before any public listener is opened.
    let management = astra_platform::platform::bind_listener(&control)?;
    astra_platform::platform::set_private(&control, 0o600)?;
    let stats = Arc::new(Stats::default());
    let sessions = Arc::new(Semaphore::new(prepared.config.max_connections));
    let udp = Arc::new(Semaphore::new(prepared.config.max_udp_flows));
    let bytes = Arc::new(Semaphore::new(BUFFER_BUDGET));
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
    let connections = Arc::new(Mutex::new(Vec::<tokio::task::AbortHandle>::new()));
    for (index, listener) in tcp {
        let current = current.clone();
        let logs = logs.clone();
        let connections = connections.clone();
        listeners.push(Task::spawn_result(
            format!("server TCP {}", first.inbounds[index].options.tag),
            async move {
                loop {
                    let (stream, peer) = match listener.accept().await {
                        Ok(value) => value,
                        Err(error) if transient_io(&error) => {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            continue;
                        }
                        Err(error) => return Err::<(), _>(error),
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
            },
        ));
    }
    let mut endpoints = vec![];
    for (index, endpoint) in quic {
        endpoints.push(endpoint.clone());
        let current = current.clone();
        let logs = logs.clone();
        let connections = connections.clone();
        listeners.push(Task::spawn(
            format!("server QUIC {}", first.inbounds[index].options.tag),
            async move {
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
            },
        ));
    }
    for (socket, aead2022) in ssudp {
        let app = first.app.clone();
        listeners.push(Task::spawn_result("server Shadowsocks UDP", async move {
            crate::protocol::ss_udp(socket, app, aead2022).await
        }));
    }
    let started = Instant::now();
    log(
        &logs,
        format!("Server ready: {} listeners", first.inbounds.len()),
    );
    let shutdown = astra_ipc::shutdown_signal();
    tokio::pin!(shutdown);
    let mut health = tokio::time::interval(Duration::from_millis(100));
    health.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let outcome: Result<()> = 'service: loop {
        tokio::select! {
            signal=&mut shutdown=>break signal.map_err(Into::into),
            error=candidate.wait_owner_loss()=>break Err(error.into()),
            _=health.tick()=>{
                for listener in &mut listeners {
                    if let Some(error)=listener.check().await {break 'service Err(anyhow::anyhow!(error));}
                }
            }
            accepted=management.accept()=>{let(mut stream,_)=match accepted {
                Ok(value)=>value,
                Err(error) if transient_io(&error)=>{tokio::time::sleep(Duration::from_millis(100)).await;continue;}
                Err(error)=>break Err(error.into())
            };let request=tokio::time::timeout(Duration::from_secs(3),astra_ipc::receive_management_versioned::<Request>(&mut stream,VERSION)).await;
                let mut reply=Reply{version:VERSION,pid:std::process::id(),uptime_secs:started.elapsed().as_secs(),active:stats.active.load(Ordering::Relaxed),accepted:stats.accepted.load(Ordering::Relaxed),rejected:stats.rejected.load(Ordering::Relaxed),upload:stats.upload.load(Ordering::Relaxed),download:stats.download.load(Ordering::Relaxed),config:config.clone(),..Default::default()};let mut stop=false;let mut force=false;
                match request{Ok(Ok(request))if request.version==VERSION=>match request.action{Action::Status=>{},Action::CommitService{boot_id}=>{
                    reply.error=if boot_id==astra_platform::lifecycle::boot_id(){candidate.commit().err().map(|error|error.to_string())}else{Some("server identity changed; refusing commit".into())};
                },Action::Monitor=>{
                    let current = current.load();
                    let tasks = connections.lock().unwrap().iter().filter(|handle| !handle.is_finished()).count();
                    let log_lines = logs.lock().unwrap().len();
                    match serde_json::to_string(&internal_snapshot(&current.app, current.inbounds.len(), tasks, log_lines)) {
                        Ok(value) => reply.logs=value,
                        Err(error) => reply.error=Some(error.to_string()),
                    }
                },Action::Logs=>reply.logs=logs.lock().unwrap().iter().cloned().collect::<Vec<_>>().join("\n"),Action::Stop=>stop=true,Action::Reload{path}=>{
                    let result=reload(&config,path,&current);if let Err(error)=result{reply.error=Some(error.to_string())}else{log(&logs,"Configuration reloaded".into());}
                },Action::StopIdentified{boot_id,force:requested_force}=>{
                    if boot_id==astra_platform::lifecycle::boot_id(){stop=true;force=requested_force;}else{reply.error=Some("server identity changed; refusing stop".into());}
                }},_=>reply.error=Some("invalid management request".into())}
                reply.identity=astra_platform::lifecycle::identity(Some(astra_model::control::Applied {path:config.clone(),digest:current.load().digest})).ok();
                reply.listeners=current.load().inbounds.iter().map(|v|format!("{} {:?} {}",v.options.tag,v.options.protocol,v.options.listen)).collect();let _=tokio::time::timeout(Duration::from_secs(3),astra_ipc::send_management(&mut stream,&reply)).await;
                if force {std::thread::spawn(||{std::thread::sleep(Duration::from_millis(25));std::process::exit(1);});}
                if stop{break Ok(())}
            },
        }
    };
    drop(listeners);
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
    log(
        &logs,
        match &outcome {
            Ok(()) => "Server stopped".into(),
            Err(error) => format!("Server failed: {error:#}"),
        },
    );
    outcome
}

fn reload(config: &std::path::Path, path: Option<PathBuf>, current: &Current) -> Result<()> {
    let operation = astra_observe::operation!("server.reload", "服务端配置热重载");
    let result = reload_inner(config, path, current);
    operation.finish(result.is_ok());
    result
}
fn reload_inner(config: &std::path::Path, path: Option<PathBuf>, current: &Current) -> Result<()> {
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

#[cfg(test)]
#[path = "../test/proxy_runtime_failure_test.rs"]
mod failure_tests;
