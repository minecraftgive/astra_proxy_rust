//! Separate, owner-only management socket. Routing IPC remains unchanged.
mod server;
use crate::platform::LocalListener as UnixListener;
use serde::{Deserialize, Serialize};
use std::{io, path::PathBuf, time::Instant};
use tokio::time::{timeout, Duration};

#[derive(Debug, Serialize, Deserialize)]
pub enum Command {
    Status,
    Stop,
    Reload { path: Option<PathBuf> },
    SetLogLevel(crate::logging::LogLevel),
    GetConfig,
    ExplainRoute(crate::RouteRequest),
    UpdateSubscriptions,
    Runtime(crate::runtime::Command),
    Session,
    WatchRuntime { interval_ms: u64 },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    Status(Status),
    Config(Result<ConfigSnapshot, String>),
    Explanation(Result<RouteExplanation, String>),
    Runtime(Result<crate::runtime::Snapshot, String>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteExplanation {
    pub api_version: u16,
    pub pid: u32,
    pub revision: u64,
    pub config: PathBuf,
    pub request: crate::RouteRequest,
    pub action: crate::RouteAction,
    pub path: String,
    pub default_action: String,
    pub condition: Option<(String, String)>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ConfigSnapshot {
    pub api_version: u16,
    pub pid: u32,
    pub path: PathBuf,
    pub text: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub api_version: u16,
    pub command: Command,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Status {
    pub api_version: u16,
    pub pid: u32,
    pub uptime_secs: u64,
    pub error: Option<String>,
    pub applied: Option<Applied>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Applied {
    pub path: PathBuf,
    pub digest: u64,
}
static APPLIED: std::sync::RwLock<Option<Applied>> = std::sync::RwLock::new(None);
pub fn config_digest(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}
pub fn applied(path: &std::path::Path, text: &str) {
    *APPLIED.write().unwrap() = Some(Applied {
        path: path.to_owned(),
        digest: config_digest(text),
    });
}

pub struct Control {
    listener: Option<UnixListener>,
    #[cfg_attr(windows, allow(dead_code))]
    path: Option<PathBuf>,
    started: Instant,
}

impl Control {
    /// Opt-in so existing standalone commands retain their behavior.
    pub fn from_env() -> io::Result<Self> {
        Self::bind(std::env::var_os("ASTRA_CONTROL_SOCKET").map(PathBuf::from))
    }

    pub fn bind(path: Option<PathBuf>) -> io::Result<Self> {
        let listener = match &path {
            Some(path) => {
                let listener = crate::platform::bind_listener(path)?;
                if let Err(e) =
                    crate::platform::set_private(crate::platform::endpoint_path(path), 0o600)
                {
                    crate::platform::remove_socket(path);
                    return Err(e);
                }
                Some(listener)
            }
            None => None,
        };
        Ok(Self {
            listener,
            path,
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
        R: FnMut(crate::runtime::Command) -> RuntimeFuture,
        RuntimeFuture:
            std::future::Future<Output = Result<crate::runtime::Snapshot, String>> + Send,
    {
        server::run(self, reload, snapshot, explain, update, runtime).await
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            crate::platform::remove_socket(path);
        }
    }
}

async fn rpc(path: &std::path::Path, command: Command, seconds: u64) -> io::Result<Response> {
    use std::{
        collections::HashMap,
        sync::{Mutex, OnceLock},
    };
    type Peer = crate::session::Peer<Request, Response>;
    static POOL: OnceLock<Mutex<HashMap<(PathBuf, u8), Peer>>> = OnceLock::new();
    let lane = match &command {
        Command::Status | Command::Runtime(crate::runtime::Command::Snapshot) => 0,
        Command::GetConfig | Command::ExplainRoute(_) => 2,
        _ => 1,
    };
    let peer = {
        let mut pool = POOL.get_or_init(Default::default).lock().unwrap();
        pool.retain(|_, peer| !peer.is_closed());
        if pool.len() >= 128 {
            if let Some(key) = pool.keys().next().cloned() {
                pool.remove(&key);
            }
        }
        pool.entry((path.to_owned(), lane))
            .or_insert_with(|| {
                Peer::new(path.to_owned(), || Request {
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
        POOL.get()
            .unwrap()
            .lock()
            .unwrap()
            .retain(|(endpoint, _), _| endpoint != path);
    }
    result
}
pub async fn request(path: &std::path::Path, command: Command) -> io::Result<Status> {
    if matches!(
        command,
        Command::GetConfig
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
    let seconds = if matches!(command, Command::Reload { .. }) {
        65
    } else {
        3
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
    command: crate::runtime::Command,
) -> io::Result<crate::runtime::Snapshot> {
    let seconds = if matches!(command, crate::runtime::Command::Snapshot) {
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
) -> io::Result<crate::platform::LocalStream> {
    let mut stream = timeout(
        Duration::from_secs(3),
        crate::platform::connect_trusted(path),
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
        let mut stream = crate::platform::LocalStream::connect(&path).await.unwrap();
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
                        Ok(crate::runtime::Snapshot {
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
                let snapshot: Result<crate::runtime::Snapshot, String> =
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
