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
                let listener = UnixListener::bind(path)?;
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
    pub async fn run<F, Fut>(&self, mut reload: F) -> io::Result<()>
    where
        F: FnMut(Option<PathBuf>) -> Fut,
        Fut: std::future::Future<Output = Result<(), String>>,
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

#[cfg(test)]
mod tests {
    use super::*;
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
