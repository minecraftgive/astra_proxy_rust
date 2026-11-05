use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Mirrors the real relay shape: one task pumping each direction
/// between the memory duplex and the transport.
fn spawn_pair(transport: DuplexStream) -> (TaskedDuplex, JoinHandle<bool>) {
    let (client, proxy) = tokio::io::duplex(4096);
    let (mut rd, mut wr) = tokio::io::split(transport);
    let (mut proxy_rd, mut proxy_wr) = tokio::io::split(proxy);
    let read_task = tokio::spawn(async move {
        // Clean EOF at the end, like the protocol read loops.
        tokio::io::copy(&mut rd, &mut proxy_wr).await.is_ok()
    });
    let write_task = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut proxy_rd, &mut wr).await;
        let _ = wr.shutdown().await;
    });
    let read_abort = read_task.abort_handle();
    (TaskedDuplex::new(client, read_abort, write_task), read_task)
}

/// Regression for the #514 review follow-up: `shutdown()` must wait for
/// the write-side task to flush bytes already accepted into the duplex
/// buffer onto the transport. Before the fix, write→shutdown→drop
/// aborted the writer mid-drain and the peer saw zero bytes.
#[tokio::test]
async fn shutdown_drains_accepted_bytes_before_completing() {
    let (transport, mut peer) = tokio::io::duplex(4096);
    let (client, proxy) = tokio::io::duplex(4096);
    let (_rd, mut wr) = tokio::io::split(transport);
    let (mut proxy_rd, _proxy_wr) = tokio::io::split(proxy);
    let write_task = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut proxy_rd, &mut wr).await;
        let _ = wr.shutdown().await;
    });
    // No read pump in this scenario — a parked task stands in for it.
    let read_stub = tokio::spawn(std::future::pending::<()>());
    let mut conn = TaskedDuplex::new(client, read_stub.abort_handle(), write_task);

    conn.write_all(b"accepted payload").await.unwrap();
    // Must not return until the writer pushed the bytes to the wire.
    conn.shutdown().await.unwrap();
    drop(conn);

    let mut received = Vec::new();
    peer.read_to_end(&mut received).await.unwrap();
    assert_eq!(received, b"accepted payload");
}

/// A write task that never drains (wedged transport) must not hang
/// `shutdown()` forever — after [`SHUTDOWN_DRAIN_TIMEOUT`] the writer
/// is aborted and shutdown reports failure: accepted bytes were
/// dropped, so `Ok(())` would lie.
#[tokio::test(start_paused = true)]
async fn shutdown_times_out_and_reports_error_when_writer_wedged() {
    let (client, _proxy) = tokio::io::duplex(4096);
    let wedged_writer = tokio::spawn(std::future::pending::<()>());
    let read_stub = tokio::spawn(std::future::pending::<()>());
    let mut conn = TaskedDuplex::new(client, read_stub.abort_handle(), wedged_writer);

    conn.write_all(b"never delivered").await.unwrap();
    let err = conn
        .shutdown()
        .await
        .expect_err("a wedged drain must not report a clean shutdown");
    assert_eq!(err.kind(), io::ErrorKind::TimedOut);

    // Idempotent: the write side IS shut down (aborted) — a second
    // call must not re-poll the spent JoinHandle or re-report.
    conn.shutdown().await.unwrap();
}

/// If the write task is already gone when shutdown is polled (aborted
/// by the supervisor after a fatal read-side failure), the drain can
/// never complete — report failure rather than Ok(()).
#[tokio::test]
async fn shutdown_reports_error_when_writer_already_aborted() {
    let (client, _proxy) = tokio::io::duplex(4096);
    let write_task = tokio::spawn(std::future::pending::<()>());
    let write_abort = write_task.abort_handle();
    let read_stub = tokio::spawn(std::future::pending::<()>());
    let mut conn = TaskedDuplex::new(client, read_stub.abort_handle(), write_task);
    write_abort.abort();

    let err = conn
        .shutdown()
        .await
        .expect_err("a dead writer cannot complete the drain");
    assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
}

/// After a graceful shutdown the read direction still delivers data
/// the peer sends afterwards — half-close download stays intact.
#[tokio::test]
async fn read_side_still_delivers_after_shutdown() {
    let (transport, mut peer) = tokio::io::duplex(4096);
    let (mut conn, read_task) = spawn_pair(transport);

    conn.write_all(b"upload").await.unwrap();
    conn.shutdown().await.unwrap();

    // Peer observes the upload then FIN.
    let mut got = [0u8; 6];
    peer.read_exact(&mut got).await.unwrap();
    assert_eq!(&got, b"upload");
    assert_eq!(peer.read(&mut [0u8; 1]).await.unwrap(), 0);

    // A late answer still flows through the read direction.
    peer.write_all(b"late reply").await.unwrap();
    let mut buf = [0u8; 10];
    conn.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"late reply");
    drop(conn);
    let _ = read_task.await;
}
