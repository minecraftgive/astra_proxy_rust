use astra_ipc::platform::{LocalListener as UnixListener, LocalStream as UnixStream};
mod engine;
use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use astra_ipc::{RouteAction, RouteRequest, RouteResponse, API_VERSION};
use engine::{RuleConfig, RuleEngine};
use std::{path::PathBuf, sync::Arc};
use tokio::{
    io::AsyncReadExt,
    sync::{watch, Mutex},
};
struct Generation {
    rules: RuleEngine,
    revision: u64,
}
struct Bound {
    listener: UnixListener,
    #[cfg_attr(windows, allow(dead_code))]
    path: PathBuf,
}
impl Drop for Bound {
    fn drop(&mut self) {
        astra_ipc::platform::remove_socket(&self.path);
    }
}
fn bind(path: &str) -> Result<Bound> {
    let listener = astra_ipc::platform::bind_listener(std::path::Path::new(path))
        .with_context(|| format!("bind Rule Core routing endpoint {path} (active endpoints and non-marker files are preserved)"))?;
    let bound = Bound {
        listener,
        path: path.into(),
    };
    astra_ipc::platform::set_private(astra_ipc::platform::endpoint_path(path), 0o600)?;
    Ok(bound)
}
#[derive(Default)]
struct ResourceWatch {
    paths: Vec<PathBuf>,
    stamps: Vec<Option<(std::time::SystemTime, u64)>>,
    next: Option<std::time::Instant>,
    interval: u64,
}
impl ResourceWatch {
    fn new(config: &RuleConfig, path: &std::path::Path) -> Result<Self> {
        anyhow::ensure!(
            config.geoip_interval <= 31_536_000,
            "geoip_interval exceeds one year"
        );
        anyhow::ensure!(
            config.geosite_interval <= 31_536_000,
            "geosite_interval exceeds one year"
        );
        let base = path.parent().context("configuration has no parent")?;
        let mut paths = Vec::new();
        let mut intervals = Vec::new();
        for source in config.rule_sets.values() {
            if !source.enabled {
                continue;
            }
            if let Some(file) = &source.path {
                paths.push(astra_rule_data::resolve(base, file));
            }
            if source.url.is_some() {
                intervals.push(source.interval.max(1));
            }
        }
        if let Some(file) = &config.geoip_database {
            paths.push(astra_rule_data::resolve(base, file));
        }
        if config.geoip_url.is_some() {
            intervals.push(config.geoip_interval.max(1));
        }
        if let Some(file) = &config.geosite_database {
            paths.push(astra_rule_data::resolve(base, file));
        }
        if config.geosite_url.is_some() {
            intervals.push(config.geosite_interval.max(1));
        }
        let mut unique = std::collections::HashSet::new();
        for resource in &paths {
            let resolved = std::fs::canonicalize(resource).unwrap_or_else(|_| resource.clone());
            anyhow::ensure!(
                resolved != path,
                "resource path must differ from the rule configuration"
            );
            anyhow::ensure!(
                unique.insert(resolved),
                "resource cache paths must be unique"
            );
        }
        let interval = intervals.into_iter().min().unwrap_or(30);
        let mut out = Self {
            paths,
            stamps: Vec::new(),
            next: None,
            interval,
        };
        out.stamps = out.snapshot();
        if config.geoip_url.is_some()
            || config.geosite_url.is_some()
            || config
                .rule_sets
                .values()
                .any(|source| source.enabled && source.url.is_some())
        {
            out.next = Some(std::time::Instant::now() + std::time::Duration::from_secs(interval));
        }
        Ok(out)
    }
    fn snapshot(&self) -> Vec<Option<(std::time::SystemTime, u64)>> {
        self.paths
            .iter()
            .map(|path| {
                std::fs::metadata(path)
                    .ok()
                    .and_then(|m| m.modified().ok().map(|time| (time, m.len())))
            })
            .collect()
    }
    fn due(&self, local: bool) -> bool {
        self.next
            .is_some_and(|next| std::time::Instant::now() >= next)
            || (local && self.snapshot() != self.stamps)
    }
    fn attempted(&mut self) {
        self.stamps = self.snapshot();
        if self.next.is_some() {
            self.next =
                Some(std::time::Instant::now() + std::time::Duration::from_secs(self.interval));
        }
    }
}
struct State {
    path: PathBuf,
    text: String,
    socket: String,
    watch_file: bool,
    resources: ResourceWatch,
    generation: Arc<ArcSwap<Generation>>,
    listener: Arc<ArcSwap<Bound>>,
    revisions: watch::Sender<u64>,
}
fn load(path: &std::path::Path) -> Result<(String, RuleConfig, RuleEngine)> {
    let text = std::fs::read_to_string(path)?;
    let config: RuleConfig = toml::from_str(&text)?;
    ResourceWatch::new(&config, path)?;
    let rules = RuleEngine::compile_at(
        &config,
        path.parent().context("configuration has no parent")?,
    )?;
    Ok((text, config, rules))
}
impl State {
    async fn reload(&mut self, path: Option<PathBuf>) -> Result<()> {
        let path = std::fs::canonicalize(path.unwrap_or_else(|| self.path.clone()))?;
        let load_path = path.clone();
        let (text, config, rules) = tokio::task::spawn_blocking(move || load(&load_path)).await??;
        let next_listener = if config.socket != self.socket {
            Some(bind(&config.socket)?)
        } else {
            None
        };
        let resources = ResourceWatch::new(&config, &path)?;
        let revision = self.generation.load().revision.wrapping_add(1).max(1);
        self.generation
            .store(Arc::new(Generation { rules, revision }));
        if let Some(listener) = next_listener {
            self.listener.store(Arc::new(listener));
        }
        self.socket = config.socket;
        self.path = path;
        self.text = text;
        astra_ipc::control::applied(&self.path, &self.text);
        self.watch_file = config.watch_config;
        self.resources = resources;
        self.revisions.send_replace(revision);
        tracing::info!(revision, config=%self.path.display(), "rule configuration committed");
        Ok(())
    }
}
#[tokio::main]
async fn main() -> Result<()> {
    astra_ipc::logging::init()?;
    let mut args = std::env::args().skip(1);
    let first = args.next();
    if first.as_deref() == Some("--stop") {
        let socket = args.next().context("expected management socket")?;
        astra_ipc::control::request(
            std::path::Path::new(&socket),
            astra_ipc::control::Command::Stop,
        )
        .await?;
        return Ok(());
    }
    if matches!(first.as_deref(), Some("--reload" | "--switch")) {
        let socket = args.next().context("expected management socket")?;
        let path = if first.as_deref() == Some("--switch") {
            Some(std::fs::canonicalize(
                args.next().context("expected config path")?,
            )?)
        } else {
            None
        };
        astra_ipc::control::request(
            std::path::Path::new(&socket),
            astra_ipc::control::Command::Reload { path },
        )
        .await?;
        return Ok(());
    }
    let check = first.as_deref() == Some("--check");
    let path =
        if check { args.next() } else { first }.unwrap_or_else(|| "configs/rule.toml".into());
    let path = std::fs::canonicalize(path)?;
    let (text, config, rules) = load(&path)?;
    if check {
        return Ok(());
    }
    let control = astra_ipc::control::Control::from_env()?;
    // Include process start time so caches cannot survive a rule process restart.
    let revision = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos() as u64;
    let generation = Arc::new(ArcSwap::from_pointee(Generation { rules, revision }));
    let listener = Arc::new(ArcSwap::from_pointee(bind(&config.socket)?));
    astra_ipc::control::applied(&path, &text);
    let (revisions, mut changes) = watch::channel(revision);
    let resources = ResourceWatch::new(&config, &path)?;
    let state = Arc::new(Mutex::new(State {
        path,
        text,
        socket: config.socket,
        watch_file: config.watch_config,
        resources,
        generation: generation.clone(),
        listener: listener.clone(),
        revisions,
    }));
    let shutdown = control.run_with_handlers(
        |path| {
            let state = state.clone();
            async move {
                state
                    .lock()
                    .await
                    .reload(path)
                    .await
                    .map_err(|e| format!("{e:#}"))
            }
        },
        || {
            let state = state.clone();
            async move {
                let state = state.lock().await;
                Ok(astra_ipc::control::ConfigSnapshot {
                    api_version: API_VERSION,
                    pid: std::process::id(),
                    path: state.path.clone(),
                    text: state.text.clone(),
                })
            }
        },
        |request| {
            let state = state.clone();
            async move {
                if request.api_version != API_VERSION
                    || request.target.host.is_empty()
                    || request.target.host.len() > 253
                    || request.target.port == 0
                {
                    return Err("Invalid route explanation request".to_owned());
                }
                let state = state.lock().await;
                let config: RuleConfig = toml::from_str(&state.text).map_err(|e| e.to_string())?;
                let generation = state.generation.load();
                let decision = generation.rules.decision(&request);
                Ok(astra_ipc::control::RouteExplanation {
                    api_version: API_VERSION,
                    pid: std::process::id(),
                    revision: generation.revision,
                    config: state.path.clone(),
                    request,
                    action: decision.action,
                    path: decision.path.to_string(),
                    default_action: config.default_action,
                    condition: generation.rules.matched_condition(&decision.path),
                })
            }
        },
    );
    tokio::pin!(shutdown);
    let watcher = {
        let state = state.clone();
        tokio::spawn(async move {
            let mut failure = String::new();
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                let mut state = state.lock().await;
                let resource_due = state.resources.due(state.watch_file);
                if !state.watch_file && !resource_due {
                    continue;
                }
                if let Ok(text) = tokio::fs::read_to_string(&state.path).await {
                    if resource_due || (text != state.text && text != failure) {
                        state.resources.attempted();
                        if let Err(error) = state.reload(None).await {
                            failure = text;
                            tracing::warn!(%error, "rule automatic reload rejected; active generation retained");
                        } else {
                            failure.clear();
                        }
                    }
                }
            }
        })
    };
    tracing::info!("Rule Core ready");
    loop {
        let bound = listener.load_full();
        tokio::select! {
            connection = bound.listener.accept() => {
                let (stream, _) = connection?;
                let (rules, revisions) = (generation.clone(), changes.clone());
                tokio::spawn(async move {
                    if let Err(error) = serve(stream, rules, revisions).await {tracing::debug!(%error, "IPC peer disconnected");}
                });
            }
            _ = changes.changed() => {},
            result = &mut shutdown => {result?; break;},
        }
    }
    watcher.abort();
    Ok(())
}
async fn serve(
    mut stream: UnixStream,
    current: Arc<ArcSwap<Generation>>,
    mut revisions: watch::Receiver<u64>,
) -> Result<()> {
    static NEXT_PEER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let peer_id = NEXT_PEER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    loop {
        let req: RouteRequest = match astra_ipc::receive(&mut stream).await {
            Ok(req) => req,
            Err(error) => {
                if error.kind() == std::io::ErrorKind::UnexpectedEof {
                    tracing::debug!(target: "astra_route", core="rule", phase="peer_closed", peer_id, "rule channel closed");
                } else {
                    tracing::debug!(target: "astra_route", core="rule", phase="decode_failed", peer_id, %error, "rule request could not be decoded");
                }
                return Err(error.into());
            }
        };
        let started = std::time::Instant::now();
        let subscription = req.id == 0 && req.target.host.is_empty() && req.target.port == 0;
        if !subscription || req.api_version != API_VERSION {
            tracing::debug!(target: "astra_route", core="rule", phase="received", peer_id, request_id=req.id, api_version=req.api_version,
                host=?req.target.host, port=req.target.port, destination_ip=?req.destination_ip, network=?req.network, inbound=?req.inbound, process=?req.process, "rule request received");
        }
        if req.api_version != API_VERSION {
            tracing::debug!(target: "astra_route", core="rule", phase="rejected", peer_id, request_id=req.id, reason="unsupported IPC version", "rule request rejected");
            anyhow::bail!("unsupported IPC version");
        }
        if req.id == 0 && req.target.host.is_empty() && req.target.port == 0 {
            loop {
                let revision = *revisions.borrow_and_update();
                astra_ipc::send(
                    &mut stream,
                    &RouteResponse {
                        api_version: API_VERSION,
                        id: 0,
                        revision,
                        action: RouteAction::Block,
                    },
                )
                .await?;
                let mut byte = [0];
                tokio::select! {
                    result = revisions.changed() => {result?;},
                    _ = stream.read(&mut byte) => return Ok(()),
                }
            }
        }
        if req.target.host.is_empty() || req.target.host.len() > 253 || req.target.port == 0 {
            tracing::debug!(target: "astra_route", core="rule", phase="rejected", peer_id, request_id=req.id, reason="invalid target", "rule request rejected");
            anyhow::bail!("invalid target");
        }
        let response = {
            let generation = current.load();
            let decision = generation.rules.decision(&req);
            tracing::debug!(target: "astra_route", core="rule", phase="decision", peer_id, request_id=req.id, revision=generation.revision,
                path=%decision.path, action=?decision.action, "rule decision selected");
            RouteResponse {
                api_version: API_VERSION,
                id: req.id,
                revision: generation.revision,
                action: decision.action,
            }
        };
        if let Err(error) = astra_ipc::send(&mut stream, &response).await {
            tracing::debug!(target: "astra_route", core="rule", phase="response_failed", peer_id, request_id=req.id, revision=response.revision,
                action=?response.action, elapsed_us=started.elapsed().as_micros() as u64, %error, "rule response failed");
            return Err(error.into());
        }
        tracing::debug!(target: "astra_route", core="rule", phase="responded", peer_id, request_id=req.id, revision=response.revision,
            action=?response.action, elapsed_us=started.elapsed().as_micros() as u64, "rule response sent");
    }
}
