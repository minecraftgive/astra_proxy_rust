//! Bounded, resumable log transport on a dedicated management connection.
use crate::protocol::*;
use astra_platform::platform::LocalStream as UnixStream;
use std::{io, path::PathBuf, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt},
    time::{interval, timeout},
};

#[cfg(test)]
pub async fn connect(
    socket: &std::path::Path,
    name: String,
    cursor: Option<LogCursor>,
) -> anyhow::Result<UnixStream> {
    timeout(Duration::from_secs(5), async {
        let mut stream = astra_platform::platform::connect_trusted(socket).await?;
        astra_ipc::send_management(
            &mut stream,
            &Request {
                version: astra_ipc::API_VERSION,
                action: Action::FollowLogs { name, cursor },
            },
        )
        .await?;
        let reply: Reply = astra_ipc::receive_management(&mut stream).await?;
        anyhow::ensure!(
            reply.version == astra_ipc::API_VERSION,
            "Unsupported log stream version"
        );
        if let Some(error) = reply.error {
            anyhow::bail!("{error}");
        }
        Ok(stream)
    })
    .await?
}
async fn send(stream: &mut UnixStream, event: &LogEvent) -> io::Result<()> {
    timeout(
        Duration::from_secs(3),
        astra_ipc::send_management(stream, event),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Log subscriber is too slow"))?
}
async fn wait(stream: &mut UnixStream, timer: &mut tokio::time::Interval) -> io::Result<bool> {
    tokio::select! { _ = timer.tick() => Ok(true), _ = stream.read_u8() => Ok(false) }
}
/// First open reads at most this much of the current session.
pub const INITIAL_WINDOW: u64 = 1024 * 1024;
/// Bytes per Data event; a full chunk means more is already available.
pub const CHUNK: usize = 128 * 1024;

/// What a subscriber follows: one file, or a source's current session, which
/// is re-resolved on every poll so a restarted process's new session replaces
/// the old one (the cursor's file identity then reports `reset`).
#[derive(Clone, Debug)]
pub enum LogPath {
    Fixed(PathBuf),
    Current { home: PathBuf, source: String },
}
impl From<PathBuf> for LogPath {
    fn from(path: PathBuf) -> Self {
        Self::Fixed(path)
    }
}
impl LogPath {
    pub fn current(home: &std::path::Path, source: impl Into<String>) -> Self {
        Self::Current {
            home: home.to_owned(),
            source: source.into(),
        }
    }
    fn resolve(&self) -> Option<PathBuf> {
        match self {
            Self::Fixed(path) => Some(path.clone()),
            Self::Current { home, source } => astra_platform::log_sessions::current(home, source),
        }
    }
}

pub async fn serve(mut stream: UnixStream, path: impl Into<LogPath>, cursor: Option<LogCursor>) {
    if let Err(error) = tail(&mut stream, path.into(), cursor).await {
        let _ = send(
            &mut stream,
            &LogEvent::Error(format!("Read log stream: {error}")),
        )
        .await;
    }
}
async fn tail(
    stream: &mut UnixStream,
    path: LogPath,
    mut cursor: Option<LogCursor>,
) -> io::Result<()> {
    let mut timer = interval(crate::UI_REFRESH_INTERVAL);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut waiting = false;
    loop {
        let opened = match path.resolve() {
            Some(path) => tokio::fs::File::open(&path).await,
            None => Err(io::ErrorKind::NotFound.into()),
        };
        let mut file = match opened {
            Ok(file) => {
                waiting = false;
                file
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if !waiting {
                    send(stream, &LogEvent::Waiting).await?;
                    waiting = true;
                }
                if !wait(stream, &mut timer).await? {
                    return Ok(());
                }
                continue;
            }
            Err(error) => return Err(error),
        };
        let metadata = file.metadata().await?;
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Log path is not a regular file",
            ));
        }
        let (device, inode) = astra_platform::platform::file_identity(&file).await?;
        let same_file = cursor.is_some_and(|c| c.device == device && c.inode == inode);
        let reset = !same_file || cursor.is_some_and(|c| c.offset > metadata.len());
        let offset = match cursor {
            Some(c) if same_file && c.offset <= metadata.len() => c.offset,
            Some(_) => 0,
            None => metadata.len().saturating_sub(INITIAL_WINDOW),
        };
        let mut skip_prefix = false;
        if cursor.is_none() && offset > 0 {
            file.seek(std::io::SeekFrom::Start(offset - 1)).await?;
            skip_prefix = file.read_u8().await? != b'\n';
        }
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        // Fill whole chunks so a full one reliably signals a backlog.
        let mut bytes = Vec::with_capacity(CHUNK);
        let n = (&mut file)
            .take(CHUNK as u64)
            .read_to_end(&mut bytes)
            .await?;
        let next = LogCursor {
            device,
            inode,
            offset: offset + n as u64,
        };
        if n > 0 || reset {
            send(
                stream,
                &LogEvent::Data {
                    bytes,
                    cursor: next,
                    reset,
                    skip_prefix,
                },
            )
            .await?;
        }
        cursor = Some(next);
        if n == 0 && !wait(stream, &mut timer).await? {
            return Ok(());
        }
    }
}

#[cfg(test)]
#[path = "test/log_stream_test.rs"]
mod tests;
