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
    let (mut client, server) = astra_platform::platform::stream_pair().await.unwrap();
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
    let (mut client, server) = astra_platform::platform::stream_pair().await.unwrap();
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
