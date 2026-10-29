//! Bounded, resumable log transport on a dedicated management connection.
use crate::protocol::*;
use astra_ipc::platform::LocalStream as UnixStream;
use std::{io, path::PathBuf, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt},
    time::{interval, timeout},
};

pub async fn connect(
    socket: &std::path::Path,
    name: String,
    cursor: Option<LogCursor>,
) -> anyhow::Result<UnixStream> {
    timeout(Duration::from_secs(5), async {
        let mut stream = astra_ipc::platform::connect_trusted(socket).await?;
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
pub async fn serve(mut stream: UnixStream, path: PathBuf, cursor: Option<LogCursor>) {
    if let Err(error) = tail(&mut stream, path, cursor).await {
        let _ = send(
            &mut stream,
            &LogEvent::Error(format!("Read log stream: {error}")),
        )
        .await;
    }
}
async fn tail(
    stream: &mut UnixStream,
    path: PathBuf,
    mut cursor: Option<LogCursor>,
) -> io::Result<()> {
    let mut timer = interval(crate::UI_REFRESH_INTERVAL);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut waiting = false;
    loop {
        let mut file = match tokio::fs::File::open(&path).await {
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
        let (device, inode) = astra_ipc::platform::file_identity(&file).await?;
        let same_file = cursor.is_some_and(|c| c.device == device && c.inode == inode);
        let reset = !same_file || cursor.is_some_and(|c| c.offset > metadata.len());
        let offset = match cursor {
            Some(c) if same_file && c.offset <= metadata.len() => c.offset,
            Some(_) => 0,
            None => metadata.len().saturating_sub(64 * 1024),
        };
        let mut skip_prefix = false;
        if cursor.is_none() && offset > 0 {
            file.seek(std::io::SeekFrom::Start(offset - 1)).await?;
            skip_prefix = file.read_u8().await? != b'\n';
        }
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        let mut bytes = vec![0; 16 * 1024];
        let n = file.read(&mut bytes).await?;
        bytes.truncate(n);
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
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    async fn data(client: &mut UnixStream) -> LogEvent {
        timeout(Duration::from_secs(3), async {
            loop {
                let event: LogEvent = astra_ipc::receive_management(client).await.unwrap();
                if matches!(&event,LogEvent::Data{bytes,..} if !bytes.is_empty()) {
                    return event;
                }
            }
        })
        .await
        .unwrap()
    }
    #[tokio::test]
    async fn streams_new_bytes_resumes_and_detects_rotation_and_truncation() {
        let dir = std::env::temp_dir().join(format!(
            "astra-log-stream-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("core.log");
        let (mut client, server) = astra_ipc::platform::stream_pair().await.unwrap();
        let task = tokio::spawn(serve(server, path.clone(), None));
        assert!(matches!(
            astra_ipc::receive_management::<LogEvent>(&mut client)
                .await
                .unwrap(),
            LogEvent::Waiting
        ));
        tokio::fs::write(&path, b"INFO first\n").await.unwrap();
        let LogEvent::Data { bytes, cursor, .. } = data(&mut client).await else {
            unreachable!()
        };
        assert_eq!(bytes, b"INFO first\n");
        drop(client);
        timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .await
            .unwrap();
        file.write_all("DEBUG 中文\n".as_bytes()).await.unwrap();
        drop(file);
        let (mut client, server) = astra_ipc::platform::stream_pair().await.unwrap();
        let task = tokio::spawn(serve(server, path.clone(), Some(cursor)));
        let LogEvent::Data { bytes, reset, .. } = data(&mut client).await else {
            unreachable!()
        };
        assert_eq!(bytes, "DEBUG 中文\n".as_bytes());
        assert!(!reset);
        tokio::fs::rename(&path, dir.join("old.log")).await.unwrap();
        tokio::fs::write(&path, b"WARN rotated\n").await.unwrap();
        let LogEvent::Data { bytes, reset, .. } = data(&mut client).await else {
            unreachable!()
        };
        assert_eq!(bytes, b"WARN rotated\n");
        assert!(reset);
        tokio::fs::write(&path, b"E\n").await.unwrap();
        let LogEvent::Data { bytes, reset, .. } = data(&mut client).await else {
            unreachable!()
        };
        assert_eq!(bytes, b"E\n");
        assert!(reset);
        drop(client);
        timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        tokio::fs::remove_dir_all(dir).await.unwrap();
    }
}
