use astra_platform::platform::{LocalListener as UnixListener, LocalStream as UnixStream};
pub mod engine;
use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use astra_ipc::API_VERSION;
use astra_model::RouteAction;
use astra_model::RouteRequest;
use astra_model::RouteResponse;
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
}
fn bind(path: &str) -> Result<Bound> {
    let listener = astra_platform::platform::bind_listener(std::path::Path::new(path))
        .with_context(|| format!("bind Rule Core routing endpoint {path} (active endpoints and non-marker files are preserved)"))?;
    let bound = Bound { listener };
    astra_platform::platform::set_private(astra_platform::platform::endpoint_path(path), 0o600)?;
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
    load_text(path, text)
}
fn load_text(path: &std::path::Path, text: String) -> Result<(String, RuleConfig, RuleEngine)> {
    load_text_with(path, text, &astra_resource::ResourceLoader::default())
}
fn load_text_with(
    path: &std::path::Path,
    text: String,
    loader: &astra_resource::ResourceLoader,
) -> Result<(String, RuleConfig, RuleEngine)> {
    let config: RuleConfig = toml::from_str(&text)?;
    ResourceWatch::new(&config, path)?;
    let rules = RuleEngine::compile_with_loader(
        &config,
        path.parent().context("configuration has no parent")?,
        loader,
    )?;
    Ok((text, config, rules))
}
impl State {
    async fn reload(&mut self, path: Option<PathBuf>) -> Result<()> {
        let operation = astra_observe::operation!("rule.reload", "规则重载");
        let result = self.reload_inner(path).await;
        operation.finish(result.is_ok());
        result
    }
    async fn reload_inner(&mut self, path: Option<PathBuf>) -> Result<()> {
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
        astra_observe::gauge!("rule.revision", "当前规则版本", "count").set(revision);
        tracing::info!(revision, config=%self.path.display(), "rule configuration committed");
        Ok(())
    }
}
pub async fn run() -> Result<()> {
    let _log = astra_platform::logging::init()?;
    let mut args = std::env::args().skip(1);
    let first = args.next();
    if first.as_deref() == Some("--stop") {
        let socket = args.next().context("expected management socket")?;
        astra_ipc::control::request(
            std::path::Path::new(&socket),
            astra_model::control::Command::Stop,
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
            astra_model::control::Command::Reload { path },
        )
        .await?;
        return Ok(());
    }
    if first.as_deref() == Some("--check-stdin") {
        use std::io::Read;
        let path = args
            .next()
            .context("--check-stdin requires the configuration path")?;
        let mut text = String::new();
        std::io::stdin()
            .take(480 * 1024 + 1)
            .read_to_string(&mut text)?;
        anyhow::ensure!(text.len() <= 480 * 1024, "Configuration exceeds 480 KiB");
        // Validation must not wait on downloads when a valid cache exists.
        let loader = astra_resource::ResourceLoader::new(astra_resource::LoadPolicy::PreferCache);
        load_text_with(std::path::Path::new(&path), text, &loader)?;
        return Ok(());
    }
    let check = first.as_deref() == Some("--check");
    let hosted_worker = first.as_deref() == Some("--host-worker");
    let path = if check || hosted_worker {
        args.next()
    } else {
        first
    }
    .unwrap_or_else(|| "configs/rule.toml".into());
    let path = std::fs::canonicalize(path)?;
    // Cold start serves expired caches immediately; the watcher refreshes
    // them in the background right after the core is listening.
    let loader = astra_resource::ResourceLoader::new(if hosted_worker {
        astra_resource::LoadPolicy::ReadOnly
    } else {
        astra_resource::LoadPolicy::PreferCache
    });
    let (text, config, rules) = if hosted_worker {
        use std::io::Read;
        let mut text = String::new();
        std::io::stdin()
            .take(480 * 1024 + 1)
            .read_to_string(&mut text)?;
        anyhow::ensure!(text.len() <= 480 * 1024, "Configuration exceeds 480 KiB");
        load_text_with(&path, text, &loader)?
    } else {
        load_text_with(&path, std::fs::read_to_string(&path)?, &loader)?
    };
    if check {
        return Ok(());
    }
    let control = astra_ipc::control::Control::from_env()?;
    // Include process start time so caches cannot survive a rule process restart.
    let revision = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos() as u64;
    let generation = Arc::new(ArcSwap::from_pointee(Generation { rules, revision }));
    astra_observe::gauge!("rule.revision", "当前规则版本", "count").set(revision);
    let listener = Arc::new(ArcSwap::from_pointee(bind(&config.socket)?));
    astra_ipc::control::applied(&path, &text);
    let (revisions, mut changes) = watch::channel(revision);
    let mut resources = ResourceWatch::new(&config, &path)?;
    if loader.served_stale() {
        resources.next = Some(std::time::Instant::now());
    }
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
                if hosted_worker {
                    return Err("generation configuration is immutable".into());
                }
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
                Ok(astra_model::control::ConfigSnapshot {
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
                Ok(astra_model::control::RouteExplanation {
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
    let mut watcher = {
        let state = state.clone();
        astra_platform::supervision::Task::spawn(
            "rule configuration/resource watcher",
            async move {
                let mut failure = String::new();
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    if hosted_worker {
                        continue;
                    }
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
            },
        )
    };
    tracing::info!("Rule Core ready");
    let outcome = loop {
        let bound = listener.load_full();
        tokio::select! {
            connection = bound.listener.accept() => {
                let (stream, _) = match connection {
                    Ok(connection) => connection,
                    Err(error) if astra_platform::supervision::transient_io(&error) => {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        continue;
                    }
                    Err(error) => break Err(error.into()),
                };
                let (rules, revisions) = (generation.clone(), changes.clone());
                tokio::spawn(async move {
                    if let Err(error) = serve(stream, rules, revisions).await {tracing::debug!(%error, "IPC peer disconnected");}
                });
            }
            _ = changes.changed() => {},
            result = &mut shutdown => break result.map_err(Into::into),
            error = watcher.failure() => break Err(anyhow::anyhow!(error)),
        }
    };
    watcher.abort();
    outcome
}
async fn serve(
    mut stream: UnixStream,
    current: Arc<ArcSwap<Generation>>,
    mut revisions: watch::Receiver<u64>,
) -> Result<()> {
    let _channel =
        astra_observe::gauge!("rule.ipc_channels", "规则 IPC 连接（含版本订阅）", "count").enter();
    static NEXT_PEER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let peer_id = NEXT_PEER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    loop {
        let req: RouteRequest = match astra_ipc::receive(&mut stream).await {
            Ok(req) => req,
            Err(error) => {
                if error.kind() == std::io::ErrorKind::UnexpectedEof {
                    tracing::debug!(target: "astra_route", core="rule", phase="peer_closed", peer_id, "rule channel closed");
                } else {
                    astra_observe::counter!("rule.decode_errors", "规则请求解码错误").add(1);
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
            astra_observe::counter!("rule.invalid_requests", "无效规则请求").add(1);
            anyhow::bail!("unsupported IPC version");
        }
        if req.id == u64::MAX
            && req.target.host == astra_ipc::CORE_INFO_HOST
            && req.target.port == 0
        {
            let process = req.process.as_ref().context("missing core metadata")?;
            let peer: astra_ipc::CoreInfo = serde_json::from_str(&process.path)?;
            anyhow::ensure!(
                peer.wire_version == API_VERSION,
                "incompatible core wire protocol"
            );
            let local = astra_ipc::CoreInfo::local(env!("CARGO_PKG_VERSION"));
            tracing::info!(?peer, features = ?local.common_features(&peer), "core capabilities exchanged");
            astra_ipc::send(
                &mut stream,
                &RouteResponse {
                    revision: 0,
                    api_version: API_VERSION,
                    id: req.id,
                    action: RouteAction::Proxy(format!(
                        "astra-core-info:{}",
                        serde_json::to_string(&local)?
                    )),
                },
            )
            .await?;
            continue;
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
            astra_observe::counter!("rule.invalid_requests", "无效规则请求").add(1);
            anyhow::bail!("invalid target");
        }
        let operation = astra_observe::operation!("rule.decision", "规则匹配与回复");
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
        match &response.action {
            RouteAction::Direct => astra_observe::counter!("rule.direct", "规则结果：直连").add(1),
            RouteAction::Block => astra_observe::counter!("rule.block", "规则结果：拦截").add(1),
            RouteAction::Proxy(_) => astra_observe::counter!("rule.proxy", "规则结果：代理").add(1),
        }
        if let Err(error) = astra_ipc::send(&mut stream, &response).await {
            operation.finish(false);
            tracing::debug!(target: "astra_route", core="rule", phase="response_failed", peer_id, request_id=req.id, revision=response.revision,
                action=?response.action, elapsed_us=started.elapsed().as_micros() as u64, %error, "rule response failed");
            return Err(error.into());
        }
        operation.finish(true);
        tracing::debug!(target: "astra_route", core="rule", phase="responded", peer_id, request_id=req.id, revision=response.revision,
            action=?response.action, elapsed_us=started.elapsed().as_micros() as u64, "rule response sent");
    }
}
