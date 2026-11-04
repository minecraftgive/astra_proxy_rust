//! Separate, owner-only management socket. Routing IPC remains unchanged.
mod server;
use astra_platform::platform::LocalListener as UnixListener;
use serde::Serialize;
use std::{io, path::PathBuf, time::Instant};
use tokio::time::{timeout, Duration};

use astra_model::control::*;
// Publish immutable complete snapshots. A panicking publisher cannot poison
// status/identity requests or expose a path paired with a different digest.
static APPLIED: std::sync::OnceLock<arc_swap::ArcSwapOption<Applied>> = std::sync::OnceLock::new();
fn applied_state() -> &'static arc_swap::ArcSwapOption<Applied> {
    APPLIED.get_or_init(arc_swap::ArcSwapOption::empty)
}
fn applied_snapshot() -> Option<Applied> {
    applied_state().load_full().as_deref().cloned()
}
pub fn config_digest(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}
pub fn applied(path: &std::path::Path, text: &str) {
    publish_applied(applied_state(), path, text);
}
fn publish_applied(state: &arc_swap::ArcSwapOption<Applied>, path: &std::path::Path, text: &str) {
    state.store(Some(std::sync::Arc::new(Applied {
        path: path.to_owned(),
        digest: config_digest(text),
    })));
}

pub struct Control {
    candidate: astra_platform::lifecycle::Candidate,
    listener: Option<UnixListener>,
    started: Instant,
}

impl Control {
    /// Opt-in so existing standalone commands retain their behavior.
    pub fn from_env() -> io::Result<Self> {
        Self::bind(std::env::var_os("ASTRA_CONTROL_SOCKET").map(PathBuf::from))
    }

    pub fn bind(path: Option<PathBuf>) -> io::Result<Self> {
        let candidate = astra_platform::lifecycle::Candidate::from_env()?;
        let listener = match &path {
            Some(path) => {
                let listener = astra_platform::platform::bind_listener(path)?;
                astra_platform::platform::set_private(
                    astra_platform::platform::endpoint_path(path),
                    0o600,
                )?;
                Some(listener)
            }
            None => None,
        };
        Ok(Self {
            candidate,
            listener,
            started: Instant::now(),
        })
    }

    pub async fn shutdown(&self) -> io::Result<()> {
        self.run(|_| async { Err("Reload is not supported".to_owned()) })
            .await
    }

    /// Reload is acknowledged only after the candidate has been committed.
    pub async fn run<F, Fut>(&self, reload: F) -> io::Result<()>
    where
        F: FnMut(Option<PathBuf>) -> Fut,
        Fut: std::future::Future<Output = Result<(), String>> + Send,
    {
        self.run_with_snapshot(reload, || async {
            Err("Configuration snapshots are not supported".to_owned())
        })
        .await
    }

    pub async fn run_with_snapshot<F, Fut, G, SnapshotFuture>(
        &self,
        reload: F,
        snapshot: G,
    ) -> io::Result<()>
    where
        F: FnMut(Option<PathBuf>) -> Fut,
        Fut: std::future::Future<Output = Result<(), String>> + Send,
        G: FnMut() -> SnapshotFuture,
        SnapshotFuture: std::future::Future<Output = Result<ConfigSnapshot, String>> + Send,
    {
        self.run_with_handlers(reload, snapshot, |_| async {
            Err("Route explanation is not supported by this core".to_owned())
        })
        .await
    }

    pub async fn run_with_handlers<F, Fut, G, SnapshotFuture, H, ExplainFuture>(
        &self,
        reload: F,
        snapshot: G,
        explain: H,
    ) -> io::Result<()>
    where
        F: FnMut(Option<PathBuf>) -> Fut,
        Fut: std::future::Future<Output = Result<(), String>> + Send,
        G: FnMut() -> SnapshotFuture,
        SnapshotFuture: std::future::Future<Output = Result<ConfigSnapshot, String>> + Send,
        H: FnMut(crate::RouteRequest) -> ExplainFuture,
        ExplainFuture: std::future::Future<Output = Result<RouteExplanation, String>> + Send,
    {
        self.run_with_updates(reload, snapshot, explain, || async {
            Err("Subscription updates are not supported by this core".into())
        })
        .await
    }
    pub async fn run_with_updates<F, Fut, G, SnapshotFuture, H, ExplainFuture, U, UpdateFuture>(
        &self,
        reload: F,
        snapshot: G,
        explain: H,
        update: U,
    ) -> io::Result<()>
    where
        F: FnMut(Option<PathBuf>) -> Fut,
        Fut: std::future::Future<Output = Result<(), String>> + Send,
        G: FnMut() -> SnapshotFuture,
        SnapshotFuture: std::future::Future<Output = Result<ConfigSnapshot, String>> + Send,
        H: FnMut(crate::RouteRequest) -> ExplainFuture,
        ExplainFuture: std::future::Future<Output = Result<RouteExplanation, String>> + Send,
        U: FnMut() -> UpdateFuture,
        UpdateFuture: std::future::Future<Output = Result<(), String>> + Send,
    {
        self.run_with_runtime(reload, snapshot, explain, update, |_| async {
            Err("Runtime controls are not supported by this core".into())
        })
        .await
    }
    pub async fn run_with_runtime<
        F,
        Fut,
        G,
        SnapshotFuture,
        H,
        ExplainFuture,
        U,
        UpdateFuture,
        R,
        RuntimeFuture,
    >(
        &self,
        reload: F,
        snapshot: G,
        explain: H,
        update: U,
        runtime: R,
    ) -> io::Result<()>
    where
        F: FnMut(Option<PathBuf>) -> Fut,
        Fut: std::future::Future<Output = Result<(), String>> + Send,
        G: FnMut() -> SnapshotFuture,
        SnapshotFuture: std::future::Future<Output = Result<ConfigSnapshot, String>> + Send,
        H: FnMut(crate::RouteRequest) -> ExplainFuture,
        ExplainFuture: std::future::Future<Output = Result<RouteExplanation, String>> + Send,
        U: FnMut() -> UpdateFuture,
        UpdateFuture: std::future::Future<Output = Result<(), String>> + Send,
        R: FnMut(astra_model::runtime::Command) -> RuntimeFuture,
        RuntimeFuture:
            std::future::Future<Output = Result<astra_model::runtime::Snapshot, String>> + Send,
    {
        server::run(self, reload, snapshot, explain, update, runtime).await
    }
}

type ControlPeer = crate::session::Peer<Request, Response>;
type ControlPeers = std::collections::HashMap<(PathBuf, u8), ControlPeer>;
static POOL: std::sync::OnceLock<std::sync::Mutex<ControlPeers>> = std::sync::OnceLock::new();
fn peer_pool(pool: &std::sync::Mutex<ControlPeers>) -> std::sync::MutexGuard<'_, ControlPeers> {
    match pool.lock() {
        Ok(peers) => peers,
        Err(poisoned) => {
            // This is only a connection cache. Existing calls hold their own
            // peer; clearing the cache neither cancels nor replays a mutation.
            let mut peers = poisoned.into_inner();
            peers.clear();
            pool.clear_poison();
            tracing::error!("Control connection cache was poisoned; discarded cached peers without replaying calls");
            peers
        }
    }
}
async fn rpc(path: &std::path::Path, command: Command, seconds: u64) -> io::Result<Response> {
    let lane = match &command {
        Command::Monitor
        | Command::Status
        | Command::Runtime(astra_model::runtime::Command::Snapshot) => 0,
        Command::GetConfig | Command::ExplainRoute(_) => 2,
        _ => 1,
    };
    let peer = {
        let mut pool = peer_pool(POOL.get_or_init(Default::default));
        pool.retain(|_, peer| !peer.is_closed());
        if pool.len() >= 128 {
            if let Some(key) = pool.keys().next().cloned() {
                pool.remove(&key);
            }
        }
        pool.entry((path.to_owned(), lane))
            .or_insert_with(|| {
                ControlPeer::new(path.to_owned(), || Request {
                    api_version: crate::API_VERSION,
                    command: Command::Session,
                })
            })
            .clone()
    };
    let stopping = matches!(&command, Command::Stop);
    let result = peer
        .request(
            Request {
                api_version: crate::API_VERSION,
                command,
            },
            Duration::from_secs(seconds),
        )
        .await;
    if stopping && result.is_ok() {
        peer_pool(POOL.get_or_init(Default::default)).retain(|(endpoint, _), _| endpoint != path);
    }
    result
}
const MANAGEMENT_RELOAD_TIMEOUT_SECS: u64 = 65;
const MANAGEMENT_UPDATE_TIMEOUT_SECS: u64 = 65;
const MANAGEMENT_DEFAULT_TIMEOUT_SECS: u64 = 3;

pub async fn identity(path: &std::path::Path) -> io::Result<Identity> {
    timeout(Duration::from_secs(3), async {
        let mut stream = astra_platform::platform::connect_trusted(path).await?;
        let peer = astra_platform::platform::peer_process_id(&stream)?;
        crate::send(
            &mut stream,
            &Request {
                api_version: crate::API_VERSION,
                command: Command::Identity,
            },
        )
        .await?;
        let identity: Identity = crate::receive_management(&mut stream).await.map_err(|error|io::Error::new(error.kind(),format!("Invalid lifecycle response; incompatible core protocol (expected {}): {error}",crate::API_VERSION)))?;
        if identity.version != 1 || identity.process.pid != peer {
            return Err(io::Error::other(
                "lifecycle identity does not match IPC peer",
            ));
        }
        Ok(identity)
    })
    .await
    .map_err(io::Error::other)?
}

pub async fn request(path: &std::path::Path, command: Command) -> io::Result<Status> {
    if matches!(
        command,
        Command::Identity
            | Command::GetConfig
            | Command::ExplainRoute(_)
            | Command::Runtime(_)
            | Command::Session
            | Command::WatchRuntime { .. }
    ) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Use the matching management helper for this command",
        ));
    }
    // Reload and subscription refreshes may involve configuration/resource work
    // bounded by the management server's 60-second operation deadline. The
    // refresh command currently acknowledges scheduling rather than waiting for
    // every download, but use the same deadline so a slow connection/handshake
    // cannot report a false timeout for an accepted mutation.
    let seconds = match command {
        Command::Reload { .. } => MANAGEMENT_RELOAD_TIMEOUT_SECS,
        Command::UpdateSubscriptions => MANAGEMENT_UPDATE_TIMEOUT_SECS,
        _ => MANAGEMENT_DEFAULT_TIMEOUT_SECS,
    };
    match rpc(path, command, seconds).await? {
        Response::Status(status) => {
            if status.api_version != crate::API_VERSION {
                return Err(io::Error::other("unsupported management version"));
            }
            if let Some(error) = &status.error {
                return Err(io::Error::other(error.clone()));
            }
            Ok(status)
        }
        _ => Err(io::Error::other("unexpected core response")),
    }
}
/// Read process-local instrumentation without changing core state.
pub async fn monitor(path: &std::path::Path) -> io::Result<astra_model::monitor::InternalSnapshot> {
    match rpc(path, Command::Monitor, 4).await? {
        Response::Monitor(result) => result.map_err(io::Error::other),
        _ => Err(io::Error::other(
            "internal monitoring is unavailable; update the core",
        )),
    }
}
pub async fn config_snapshot(path: &std::path::Path) -> io::Result<ConfigSnapshot> {
    match rpc(path, Command::GetConfig, 4).await? {
        Response::Config(reply) => reply.map_err(io::Error::other),
        _ => Err(io::Error::other("unexpected snapshot response")),
    }
}
pub async fn explain_route(
    path: &std::path::Path,
    request: crate::RouteRequest,
) -> io::Result<RouteExplanation> {
    match rpc(path, Command::ExplainRoute(request), 4).await? {
        Response::Explanation(reply) => reply.map_err(io::Error::other),
        _ => Err(io::Error::other("unexpected explanation response")),
    }
}
pub async fn runtime(
    path: &std::path::Path,
    command: astra_model::runtime::Command,
) -> io::Result<astra_model::runtime::Snapshot> {
    let seconds = if matches!(command, astra_model::runtime::Command::Snapshot) {
        4
    } else {
        65
    };
    match rpc(path, Command::Runtime(command), seconds).await? {
        Response::Runtime(reply) => reply.map_err(io::Error::other),
        _ => Err(io::Error::other("unexpected runtime response")),
    }
}
pub async fn watch_runtime(
    path: &std::path::Path,
    interval_ms: u64,
) -> io::Result<astra_platform::platform::LocalStream> {
    let mut stream = timeout(
        Duration::from_secs(3),
        astra_platform::platform::connect_trusted(path),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "watch connection timeout"))??;
    crate::send(
        &mut stream,
        &Request {
            api_version: crate::API_VERSION,
            command: Command::WatchRuntime { interval_ms },
        },
    )
    .await?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn publisher_panic_cannot_poison_applied_snapshots() {
        let state = std::sync::Arc::new(arc_swap::ArcSwapOption::empty());
        let publisher = state.clone();
        assert!(std::thread::spawn(move || {
            publish_applied(&publisher, std::path::Path::new("first.toml"), "first");
            panic!("injected publisher failure");
        })
        .join()
        .is_err());
        let retained = state.load_full().unwrap();
        assert_eq!(retained.path, PathBuf::from("first.toml"));
        assert_eq!(retained.digest, config_digest("first"));
        publish_applied(&state, std::path::Path::new("next.toml"), "next");
        let next = state.load_full().unwrap();
        assert_eq!(next.path, PathBuf::from("next.toml"));
        assert_eq!(next.digest, config_digest("next"));
        assert_eq!(retained.digest, config_digest("first"));
    }
    #[test]
    fn poisoned_peer_cache_is_reset_before_reuse() {
        let pool = std::sync::Arc::new(std::sync::Mutex::new(ControlPeers::new()));
        let writer = pool.clone();
        assert!(std::thread::spawn(move || {
            let _guard = writer.lock().unwrap();
            panic!("injected cache writer failure");
        })
        .join()
        .is_err());
        assert!(peer_pool(&pool).is_empty());
        assert!(!pool.is_poisoned());
        assert!(peer_pool(&pool).is_empty());
    }
    #[tokio::test]
    async fn monitor_reads_internal_counters_without_changing_status() {
        let path =
            std::env::temp_dir().join(format!("astra-monitor-test-{}.sock", std::process::id()));
        let control = Control::bind(Some(path.clone())).unwrap();
        let task = tokio::spawn(async move {
            control.shutdown().await.unwrap();
        });
        astra_observe::counter!("test.monitor_requests", "测试请求").add(1);
        let snapshot = monitor(&path).await.unwrap();
        assert_eq!(snapshot.epoch, astra_platform::lifecycle::boot_id());
        assert!(snapshot
            .metrics
            .iter()
            .any(|m| m.name == "test.monitor_requests" && m.value == "1"));
        assert!(snapshot
            .metrics
            .iter()
            .any(|m| m.name == "runtime.control_queue"));
        assert_eq!(
            request(&path, Command::Status).await.unwrap().pid,
            std::process::id()
        );
        request(&path, Command::Stop).await.unwrap();
        task.await.unwrap();
    }
    #[tokio::test]
    async fn snapshot_uses_bounded_management_frames_without_changing_status_replies() {
        let path =
            std::env::temp_dir().join(format!("astra-snapshot-test-{}.sock", std::process::id()));
        let control = Control::bind(Some(path.clone())).unwrap();
        let task = tokio::spawn(async move {
            control
                .run_with_snapshot(
                    |_| async { Ok(()) },
                    || async {
                        Ok(ConfigSnapshot {
                            api_version: crate::API_VERSION,
                            pid: std::process::id(),
                            path: PathBuf::from("loaded.toml"),
                            text: "x".repeat(32 * 1024),
                        })
                    },
                )
                .await
                .unwrap();
        });
        let snapshot = config_snapshot(&path).await.unwrap();
        assert_eq!(snapshot.text.len(), 32 * 1024);
        assert_eq!(snapshot.path, PathBuf::from("loaded.toml"));
        assert_eq!(snapshot.pid, std::process::id());
        assert!(request(&path, Command::GetConfig).await.is_err());
        assert!(request(&path, Command::Status).await.is_ok());
        request(&path, Command::Stop).await.unwrap();
        task.await.unwrap();
    }
    #[tokio::test]
    async fn status_version_rejection_stop_and_cleanup() {
        let path = std::env::temp_dir().join(format!("astra-ctl-test-{}.sock", std::process::id()));
        let control = Control::bind(Some(path.clone())).unwrap();
        let task = tokio::spawn(async move {
            control.shutdown().await.unwrap();
        });
        assert_eq!(
            request(&path, Command::Status).await.unwrap().pid,
            std::process::id()
        );
        let mut stream = astra_platform::platform::LocalStream::connect(&path)
            .await
            .unwrap();
        crate::send(
            &mut stream,
            &Request {
                api_version: 999,
                command: Command::Stop,
            },
        )
        .await
        .unwrap();
        assert!(crate::receive::<Status>(&mut stream).await.is_err());
        assert!(request(&path, Command::Status).await.is_ok());
        request(&path, Command::Stop).await.unwrap();
        task.await.unwrap();
        assert!(!path.exists());
    }
    #[tokio::test]
    async fn slow_reload_streams_progress_without_blocking_status_or_runtime_push() {
        let path =
            std::env::temp_dir().join(format!("astra-concurrent-{}.sock", std::process::id()));
        let control = Control::bind(Some(path.clone())).unwrap();
        let entered = std::sync::Arc::new(tokio::sync::Notify::new());
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        let (started, finish) = (entered.clone(), release.clone());
        let server = tokio::spawn(async move {
            control
                .run_with_runtime(
                    move |_| {
                        let (started, finish) = (started.clone(), finish.clone());
                        async move {
                            crate::progress::report("Reload waiting", "test barrier");
                            started.notify_one();
                            finish.notified().await;
                            Ok(())
                        }
                    },
                    || async { Err("unused".into()) },
                    |_| async { Err("unused".into()) },
                    || async { Ok(()) },
                    |_| async {
                        Ok(astra_model::runtime::Snapshot {
                            pid: std::process::id(),
                            ..Default::default()
                        })
                    },
                )
                .await
                .unwrap();
        });
        let (progress, mut updates) = tokio::sync::watch::channel(None);
        let endpoint = path.clone();
        let reload = tokio::spawn(crate::progress::scope(progress, async move {
            request(&endpoint, Command::Reload { path: None }).await
        }));
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        assert!(!reload.is_finished());
        tokio::time::timeout(Duration::from_secs(1), async {
            assert_eq!(
                request(&path, Command::Status).await.unwrap().pid,
                std::process::id()
            );
            let mut push = watch_runtime(&path, 100).await.unwrap();
            for _ in 0..2 {
                let snapshot: Result<astra_model::runtime::Snapshot, String> =
                    crate::receive_management(&mut push).await.unwrap();
                assert_eq!(snapshot.unwrap().pid, std::process::id());
            }
            loop {
                if updates
                    .borrow_and_update()
                    .as_ref()
                    .is_some_and(|p| p.phase == "Reload waiting")
                {
                    break;
                }
                updates.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        release.notify_one();
        reload.await.unwrap().unwrap();
        request(&path, Command::Stop).await.unwrap();
        server.await.unwrap();
    }
    #[test]
    fn existing_socket_is_never_overwritten() {
        let path =
            std::env::temp_dir().join(format!("astra-ctl-existing-{}.sock", std::process::id()));
        std::fs::write(&path, "keep").unwrap();
        // Bind rejects existing regular files before creating a Tokio listener.
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            assert!(Control::bind(Some(path.clone())).is_err());
        });
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "keep");
        std::fs::remove_file(path).unwrap();
    }
}
