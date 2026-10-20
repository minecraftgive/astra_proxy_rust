//! Separate, owner-only management socket. Routing IPC remains unchanged.
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
        Fut: std::future::Future<Output = Result<(), String>>,
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
        Fut: std::future::Future<Output = Result<(), String>>,
        G: FnMut() -> SnapshotFuture,
        SnapshotFuture: std::future::Future<Output = Result<ConfigSnapshot, String>>,
    {
        self.run_with_handlers(reload, snapshot, |_| async {
            Err("Route explanation is not supported by this core".to_owned())
        })
        .await
    }

    pub async fn run_with_handlers<F, Fut, G, SnapshotFuture, H, ExplainFuture>(
        &self,
        mut reload: F,
        mut snapshot: G,
        mut explain: H,
    ) -> io::Result<()>
    where
        F: FnMut(Option<PathBuf>) -> Fut,
        Fut: std::future::Future<Output = Result<(), String>>,
        G: FnMut() -> SnapshotFuture,
        SnapshotFuture: std::future::Future<Output = Result<ConfigSnapshot, String>>,
        H: FnMut(crate::RouteRequest) -> ExplainFuture,
        ExplainFuture: std::future::Future<Output = Result<RouteExplanation, String>>,
    {
        let signal = crate::shutdown_signal();
        tokio::pin!(signal);
        let mut hup = crate::platform::ReloadSignal::new()?;
        loop {
            let connection = async {
                match &self.listener {
                    Some(listener) => listener.accept().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                result = &mut signal => return result,
                _ = hup.recv() => {
                    if let Err(error) = reload(None).await {
                        eprintln!("Configuration reload rejected: {error}");
                    }
                }
                result = connection => {
                    let (mut stream, _) = result?;
                    let request = async {
                        let request: Request = crate::receive(&mut stream).await?;
                        if request.api_version != crate::API_VERSION {
                            return Err(io::Error::new(io::ErrorKind::InvalidData, "unsupported management version"));
                        }
                        if matches!(request.command, Command::GetConfig) {
                            crate::send_management(&mut stream, &snapshot().await).await?;
                            return Ok(false);
                        }
                        if let Command::ExplainRoute(route) = request.command {
                            crate::send_management(&mut stream, &explain(route).await).await?;
                            return Ok(false);
                        }
                        let stop = matches!(request.command, Command::Stop);
                        let error = match request.command {
                            Command::Reload { path } => reload(path).await.err(),
                            Command::SetLogLevel(level) => crate::logging::set_level(level).err().map(|e|e.to_string()),
                            _ => None,
                        };
                        let applied = APPLIED.read().unwrap().clone();
                        crate::send(&mut stream, &Status {
                            api_version: crate::API_VERSION,
                            pid: std::process::id(),
                            uptime_secs: self.started.elapsed().as_secs(),
                            error,
                            applied,
                        }).await?;
                        Ok::<_, io::Error>(stop)
                    };
                    tokio::select! {
                        result = &mut signal => return result,
                        result = timeout(Duration::from_secs(60), request) => {
                            if matches!(result, Ok(Ok(true))) { return Ok(()); }
                        }
                    }
                }
            }
        }
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            crate::platform::remove_socket(path);
        }
    }
}

pub async fn request(path: &std::path::Path, command: Command) -> io::Result<Status> {
    if matches!(command, Command::GetConfig | Command::ExplainRoute(_)) {
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
    timeout(Duration::from_secs(seconds), async {
        let mut stream = crate::platform::LocalStream::connect(path).await?;
        crate::send(
            &mut stream,
            &Request {
                api_version: crate::API_VERSION,
                command,
            },
        )
        .await?;
        let status: Status = crate::receive(&mut stream).await?;
        if status.api_version != crate::API_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported management version",
            ));
        }
        if let Some(error) = &status.error {
            return Err(io::Error::other(error.clone()));
        }
        Ok(status)
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "core management timeout"))?
}

pub async fn config_snapshot(path: &std::path::Path) -> io::Result<ConfigSnapshot> {
    timeout(Duration::from_secs(3), async {
        let mut stream = crate::platform::LocalStream::connect(path).await?;
        crate::send(
            &mut stream,
            &Request {
                api_version: crate::API_VERSION,
                command: Command::GetConfig,
            },
        )
        .await?;
        let snapshot: Result<ConfigSnapshot, String> =
            crate::receive_management(&mut stream).await?;
        let snapshot = snapshot.map_err(io::Error::other)?;
        if snapshot.api_version != crate::API_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported snapshot version",
            ));
        }
        Ok(snapshot)
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "configuration snapshot timeout"))?
}

pub async fn explain_route(
    path: &std::path::Path,
    request: crate::RouteRequest,
) -> io::Result<RouteExplanation> {
    timeout(Duration::from_secs(3), async {
        let mut stream = crate::platform::LocalStream::connect(path).await?;
        crate::send(
            &mut stream,
            &Request {
                api_version: crate::API_VERSION,
                command: Command::ExplainRoute(request),
            },
        )
        .await?;
        let reply: Result<RouteExplanation, String> =
            crate::receive_management(&mut stream).await?;
        let reply = reply.map_err(io::Error::other)?;
        if reply.api_version != crate::API_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported explanation version",
            ));
        }
        Ok(reply)
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "route explanation timeout"))?
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
