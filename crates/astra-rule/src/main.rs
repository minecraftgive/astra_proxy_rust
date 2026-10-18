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
    let listener = UnixListener::bind(path)
        .context("bind Rule Core UDS (existing sockets are never overwritten)")?;
    let bound = Bound {
        listener,
        path: path.into(),
    };
    astra_ipc::platform::set_private(astra_ipc::platform::endpoint_path(path), 0o600)?;
    Ok(bound)
}
struct State {
    path: PathBuf,
    text: String,
    socket: String,
    watch_file: bool,
    generation: Arc<ArcSwap<Generation>>,
    listener: Arc<ArcSwap<Bound>>,
    revisions: watch::Sender<u64>,
}
fn load(path: &std::path::Path) -> Result<(String, RuleConfig, RuleEngine)> {
    let text = std::fs::read_to_string(path)?;
    let config: RuleConfig = toml::from_str(&text)?;
    let rules = RuleEngine::compile(&config)?;
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
    let state = Arc::new(Mutex::new(State {
        path,
        text,
        socket: config.socket,
        watch_file: config.watch_config,
        generation: generation.clone(),
        listener: listener.clone(),
        revisions,
    }));
    let shutdown = control.run(|path| {
        let state = state.clone();
        async move {
            state
                .lock()
                .await
                .reload(path)
                .await
                .map_err(|e| format!("{e:#}"))
        }
    });
    tokio::pin!(shutdown);
    let watcher = {
        let state = state.clone();
        tokio::spawn(async move {
            let mut failure = String::new();
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                let mut state = state.lock().await;
                if !state.watch_file {
                    continue;
                }
                if let Ok(text) = tokio::fs::read_to_string(&state.path).await {
                    if text != state.text && text != failure {
                        if let Err(error) = state.reload(None).await {
                            failure = text;
                            tracing::warn!(%error, "rule automatic reload rejected");
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
    loop {
        let req: RouteRequest = astra_ipc::receive(&mut stream).await?;
        anyhow::ensure!(req.api_version == API_VERSION, "unsupported IPC version");
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
        anyhow::ensure!(
            !req.target.host.is_empty() && req.target.host.len() <= 253 && req.target.port != 0,
            "invalid target"
        );
        let response = {
            let generation = current.load();
            RouteResponse {
                api_version: API_VERSION,
                id: req.id,
                revision: generation.revision,
                action: generation.rules.decide(&req),
            }
        };
        astra_ipc::send(&mut stream, &response).await?;
    }
}
