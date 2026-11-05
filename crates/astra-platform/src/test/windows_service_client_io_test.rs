use super::*;
use std::time::{Duration, Instant};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::windows::named_pipe::ServerOptions,
};

fn pipe_name() -> String {
    format!(r"\\.\pipe\astra-service-io-test-{}", uuid::Uuid::new_v4())
}

#[tokio::test]
async fn native_overlapped_pipe_round_trip_handles_fragmented_frames_and_handoff_bytes() {
    let name = pipe_name();
    let mut server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(&name)
        .unwrap();
    let client = tokio::task::spawn_blocking(move || {
        let pipe = Pipe::open(OsStr::new(&name)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        pipe.send(&serde_json::json!({"args":["中文 路径","value"]}), deadline)
            .unwrap();
        let response: serde_json::Value = pipe.receive(deadline).unwrap();
        assert_eq!(response["status"], "ready");
        pipe.write_all(&[1], deadline).unwrap();
        let mut ready = [0];
        pipe.read_exact(&mut ready, deadline).unwrap();
        assert_eq!(ready, [1]);
    });
    server.connect().await.unwrap();
    let mut header = [0; 4];
    server.read_exact(&mut header).await.unwrap();
    let mut body = vec![0; u32::from_le_bytes(header) as usize];
    server.read_exact(&mut body).await.unwrap();
    let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(request["args"][0], "中文 路径");
    let response = br#"{"status":"ready"}"#;
    let mut frame = (response.len() as u32).to_le_bytes().to_vec();
    frame.extend(response);
    for fragment in frame.chunks(3) {
        server.write_all(fragment).await.unwrap();
        tokio::task::yield_now().await;
    }
    let mut ack = [0];
    server.read_exact(&mut ack).await.unwrap();
    assert_eq!(ack, [1]);
    server.write_all(&[1]).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), client)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn stalled_native_pipe_read_is_cancelled_before_buffers_are_released() {
    let name = pipe_name();
    let mut server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(&name)
        .unwrap();
    let client = tokio::task::spawn_blocking(move || {
        let pipe = Pipe::open(OsStr::new(&name)).unwrap();
        let start = Instant::now();
        let mut bytes = [0; 16];
        let error = pipe
            .read_exact(&mut bytes, start + Duration::from_millis(100))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(2));
        // Closing the pipe after cancellation is safe; there is no detached
        // read thread or pending operation still referencing this stack buffer.
    });
    server.connect().await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), client)
        .await
        .unwrap()
        .unwrap();
    let mut probe = [0];
    match tokio::time::timeout(Duration::from_secs(1), server.read(&mut probe))
        .await
        .unwrap()
    {
        Ok(0) => {}
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {}
        result => panic!("client pipe remained open after cancellation: {result:?}"),
    }
}

#[tokio::test]
async fn blocked_native_pipe_write_obeys_the_same_deadline() {
    let name = pipe_name();
    let server = ServerOptions::new()
        .first_pipe_instance(true)
        .in_buffer_size(16)
        .create(&name)
        .unwrap();
    let client = tokio::task::spawn_blocking(move || {
        let pipe = Pipe::open(OsStr::new(&name)).unwrap();
        let bytes = vec![42; MAX_FRAME];
        let error = pipe
            .write_all(&bytes, Instant::now() + Duration::from_millis(100))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    });
    server.connect().await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), client)
        .await
        .unwrap()
        .unwrap();
}
