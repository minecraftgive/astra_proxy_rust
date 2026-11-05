use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[test]
fn naming_reflects_drop_mode() {
    assert_eq!(RejectAdapter::new(false).name(), "REJECT");
    assert_eq!(RejectAdapter::new(true).name(), "REJECT-DROP");
    assert_eq!(
        RejectAdapter::new(false).adapter_type(),
        AdapterType::Reject
    );
    assert_eq!(
        RejectAdapter::new(true).adapter_type(),
        AdapterType::RejectDrop
    );
}

#[test]
fn empty_addr_and_supports_udp() {
    let a = RejectAdapter::new(false);
    assert_eq!(a.addr(), "");
    assert!(
        a.support_udp(),
        "reject claims UDP so rules don't bypass it"
    );
}

#[tokio::test]
async fn dial_tcp_reject_returns_eof_stream_immediately() {
    let a = RejectAdapter::new(false);
    let mut conn = a
        .dial_tcp(&Metadata::default())
        .await
        .expect("REJECT must produce a connection (not error)");
    // Read returns 0 (EOF) without blocking.
    let mut buf = [0u8; 8];
    let n = conn.read(&mut buf).await.unwrap();
    assert_eq!(n, 0, "REJECT stream is EOF on read");
}

#[tokio::test]
async fn dial_tcp_reject_silently_discards_writes() {
    let a = RejectAdapter::new(false);
    let mut conn = a.dial_tcp(&Metadata::default()).await.unwrap();
    // Writes report success but go nowhere.
    let n = conn.write(b"discarded").await.unwrap();
    assert_eq!(n, b"discarded".len());
    conn.flush().await.unwrap();
    conn.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn dial_tcp_reject_drop_hangs_then_yields_eof() {
    // RejectDrop simulates a black-hole by sleeping ~60 s before yielding
    // the same EOF stream. Under paused tokio time we can advance past
    // that without wall-clock waiting.
    let a = std::sync::Arc::new(RejectAdapter::new(true));
    let a2 = std::sync::Arc::clone(&a);
    let task = tokio::spawn(async move { a2.dial_tcp(&Metadata::default()).await });
    tokio::task::yield_now().await;
    assert!(!task.is_finished(), "dial must not return before sleep");
    tokio::time::advance(std::time::Duration::from_secs(61)).await;
    let mut conn = task.await.unwrap().expect("eventual EOF stream");
    let mut buf = [0u8; 4];
    let n = conn.read(&mut buf).await.unwrap();
    assert_eq!(n, 0);
}

/// PASS / PASS-RULE are Reject-shaped nops carrying their own type
/// tags (upstream nopConn): an immediate EOF at dial, writes
/// discarded. A rule that materializes one must never blackhole.
#[tokio::test]
async fn dial_tcp_pass_and_pass_rule_yield_eof_nop() {
    for (a, want) in [
        (RejectAdapter::pass(), AdapterType::Pass),
        (RejectAdapter::pass_rule(), AdapterType::PassRule),
    ] {
        assert_eq!(a.adapter_type(), want);
        let mut conn = a.dial_tcp(&Metadata::default()).await.expect("nop conn");
        let n = conn.write(b"discarded").await.unwrap();
        assert_eq!(n, b"discarded".len(), "writes report success");
        let mut buf = [0u8; 8];
        let n = conn.read(&mut buf).await.unwrap();
        assert_eq!(n, 0, "{want:?} stream is EOF on read");
    }
}

#[tokio::test]
async fn dial_udp_pass_and_pass_rule_yield_writeonly_nop() {
    for a in [RejectAdapter::pass(), RejectAdapter::pass_rule()] {
        let conn = a.dial_udp(&Metadata::default()).await.expect("nop conn");
        let dst: SocketAddr = "127.0.0.1:1".parse().unwrap();
        assert_eq!(conn.write_packet(b"x", &dst).await.unwrap(), 1);
        let mut buf = [0u8; 16];
        assert!(conn.read_packet(&mut buf).await.is_err());
    }
}

#[tokio::test]
async fn dial_udp_returns_writeonly_packet_conn() {
    let a = RejectAdapter::new(false);
    let conn = a.dial_udp(&Metadata::default()).await.expect("packet conn");
    let dst: SocketAddr = "127.0.0.1:1".parse().unwrap();
    let n = conn.write_packet(b"x", &dst).await.unwrap();
    assert_eq!(n, 1);
    let mut buf = [0u8; 16];
    let err = conn.read_packet(&mut buf).await.err();
    assert!(err.is_some(), "REJECT UDP read must error, not block");
    assert!(conn.local_addr().is_err());
    assert!(conn.close().is_ok());
}

#[tokio::test]
async fn connect_over_refuses_relay_insertion() {
    let a = RejectAdapter::new(false);
    let upstream = a.dial_tcp(&Metadata::default()).await.unwrap();
    let err = a
        .connect_over(upstream, &Metadata::default())
        .await
        .err()
        .expect("relay through REJECT must be a hard error");
    match err {
        MeowError::Proxy(msg) => assert!(
            msg.to_lowercase().contains("reject"),
            "error message should name 'reject': {msg}"
        ),
        other => panic!("expected MeowError::Proxy, got {other:?}"),
    }
}
