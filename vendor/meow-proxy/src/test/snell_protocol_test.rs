use super::*;
use std::future::Future;
use std::time::Duration;
use tokio::io::{AsyncReadExt, DuplexStream};

/// Wrap an await that could hang so a failure is an assertion instead of
/// a test-runner timeout.
async fn within<T>(fut: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), fut)
        .await
        .expect("test future timed out")
}

/// Client `Snell` wrapper on one duplex half, mock-server `V4Conn` on the
/// other. The v4 codec is symmetric (each direction sends its own salt),
/// so a bare `V4Conn` works as the server side.
fn rig() -> (Snell<DuplexStream>, V4Conn<DuplexStream>) {
    let (a, b) = tokio::io::duplex(1 << 16);
    let psk: Arc<[u8]> = Arc::from(b"test-psk".as_slice());
    (Snell::new(a, Arc::clone(&psk)), V4Conn::new(b, psk))
}

/// Transport wrapper whose `poll_flush` parks while `blocked` — `duplex`'s
/// own flush always succeeds, so without this a test can never reach the
/// "codec `pending` drained but transport flush still pending" state.
/// `flushes` counts attempts so a resume can prove the flush was retried.
/// Manual-poll only: a blocked `poll_flush` never registers the waker, so
/// `.await`ing it would hang.
struct FlushGate<S> {
    inner: S,
    blocked: Arc<std::sync::atomic::AtomicBool>,
    flushes: Arc<std::sync::atomic::AtomicUsize>,
}

impl<S> FlushGate<S> {
    fn new(
        inner: S,
    ) -> (
        Self,
        Arc<std::sync::atomic::AtomicBool>,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let blocked = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let flushes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        (
            Self {
                inner,
                blocked: Arc::clone(&blocked),
                flushes: Arc::clone(&flushes),
            },
            blocked,
            flushes,
        )
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for FlushGate<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for FlushGate<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.flushes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if self.blocked.load(std::sync::atomic::Ordering::Relaxed) {
            Poll::Pending
        } else {
            Pin::new(&mut self.inner).poll_flush(cx)
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[tokio::test]
async fn write_header_connect_layout() {
    let (mut client, mut peer) = rig();
    within(write_header(&mut client, "example.com", 443, false))
        .await
        .unwrap();
    within(client.flush()).await.unwrap();

    let mut got = [0u8; 17];
    within(peer.read_exact(&mut got)).await.unwrap();
    let mut expected = vec![HEADER_VERSION, COMMAND_CONNECT, 0, 11];
    expected.extend_from_slice(b"example.com");
    expected.extend_from_slice(&[0x01, 0xBB]); // 443 BE
    assert_eq!(&got[..], &expected[..]);
}

#[tokio::test]
async fn write_header_reuse_uses_connect_v2() {
    let (mut client, mut peer) = rig();
    within(write_header(&mut client, "example.com", 443, true))
        .await
        .unwrap();
    within(client.flush()).await.unwrap();

    let mut got = [0u8; 17];
    within(peer.read_exact(&mut got)).await.unwrap();
    assert_eq!(got[0], HEADER_VERSION);
    assert_eq!(got[1], COMMAND_CONNECT_V2);
}

#[tokio::test]
async fn write_header_rejects_long_host() {
    let host = "h".repeat(256);
    let mut sink = tokio::io::sink();
    let err = write_header(&mut sink, &host, 80, false).await.unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
}

/// Issue #625.16: `write_packet_frame` (the non-poll variant kept for
/// external/test callers) cancelled mid-write leaves a torn AEAD frame on the
/// wire. The tear is sticky — later writes must fail fast instead of
/// silently appending after the torn prefix (v3) or clobbering
/// undrained pending bytes (v4).
#[tokio::test]
async fn cancelled_packet_frame_write_tears_conn_v4() {
    let (a, _b) = tokio::io::duplex(1 << 10);
    let psk: Arc<[u8]> = Arc::from(b"test-psk".as_slice());
    let mut client = Snell::new(a, psk);

    // A 4 KiB frame exceeds the 1 KiB duplex buffer — the write pends
    // mid-frame and the timeout drops the future.
    let cancelled = tokio::time::timeout(
        Duration::from_millis(50),
        client.write_packet_frame(&[0xAA; 4096]),
    )
    .await;
    assert!(cancelled.is_err(), "write must have timed out mid-frame");

    // `within` so a regression that drops the gate hangs-bounded rather
    // than stalling the suite on the full pipe.
    let err = within(client.write_packet_frame(b"x")).await.unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    assert!(
        err.to_string().contains("desynced"),
        "write after a cancelled write must fail fast, got {err:?}"
    );
}

/// Same tear semantics on the v3 codec, where datagrams ride the plain
/// AEAD stream — a cancelled write leaves the peer mid-frame.
#[tokio::test]
async fn cancelled_packet_frame_write_tears_conn_v3() {
    let (a, _b) = tokio::io::duplex(1 << 10);
    let psk: Arc<[u8]> = Arc::from(b"test-psk".as_slice());
    let mut client = Snell::new_v3(a, psk);

    let cancelled = tokio::time::timeout(
        Duration::from_millis(50),
        client.write_packet_frame(&[0xAA; 4096]),
    )
    .await;
    assert!(cancelled.is_err(), "write must have timed out mid-frame");

    let err = within(client.write_packet_frame(b"x")).await.unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    assert!(
        err.to_string().contains("desynced"),
        "write after a cancelled write must fail fast, got {err:?}"
    );
}

/// The poll variant must refuse a fresh `PacketFrameProgress` on a torn
/// stream — the conn-level poison is the primary guard, this is the
/// in-codec backstop for any future direct caller.
#[test]
fn poll_packet_frame_refuses_fresh_progress_on_torn_stream() {
    for versioned in [false, true] {
        let (a, _b) = tokio::io::duplex(1 << 10);
        let psk: Arc<[u8]> = Arc::from(b"test-psk".as_slice());
        let mut client = if versioned {
            Snell::new_v3(a, psk)
        } else {
            Snell::new(a, psk)
        };
        let mut cx = Context::from_waker(std::task::Waker::noop());

        let frame = vec![0xAA; 4096];
        let mut progress = PacketFrameProgress::default();
        let poll = client.poll_write_packet_frame(&mut cx, &frame, &mut progress);
        assert!(
            matches!(poll, Poll::Pending),
            "v{}: write must park mid-frame on the tiny pipe, got {poll:?}",
            if versioned { 3 } else { 4 }
        );

        // A fresh progress — a NEW write while the first is still
        // in-flight — must fail fast instead of clobbering the codec's
        // undrained pending bytes.
        let mut fresh = PacketFrameProgress::default();
        match client.poll_write_packet_frame(&mut cx, &frame, &mut fresh) {
            Poll::Ready(Err(e)) => {
                assert_eq!(e.kind(), io::ErrorKind::BrokenPipe);
                assert!(
                    e.to_string().contains("desynced"),
                    "v{}: expected desync error, got {e}",
                    if versioned { 3 } else { 4 }
                );
            }
            other => panic!(
                "v{}: expected desynced error on torn stream, got {other:?}",
                if versioned { 3 } else { 4 }
            ),
        }

        // Resuming the in-flight write with the same progress is the
        // legitimate continuation path — it must not be refused.
        // Strictly `Pending`: the peer half is never read, so the
        // staged frame can never fully drain — `Ready(Ok)` here would
        // mean a pending-drop bug cleared the tear early.
        let poll = client.poll_write_packet_frame(&mut cx, &frame, &mut progress);
        assert!(
            matches!(poll, Poll::Pending),
            "v{}: resume on the undrained pipe must stay Pending, got {poll:?}",
            if versioned { 3 } else { 4 }
        );
    }
}

/// `frame_write_torn` must refuse on its own: park a write on the
/// *transport* flush after the codec `pending` buffer has fully drained
/// (a state `tokio::io::duplex` alone can never reach — its flush always
/// succeeds), abandon the write, and a fresh progress must still fail
/// fast even though `pending_leftover` is false.
///
/// The same rig pins the unconditional resume `poll_flush`: once the
/// gate opens, resuming the original progress must call `poll_flush`
/// again — with a `has_pending_write()` shortcut it would return
/// `Ready(Ok)` without retrying the pended transport flush.
#[test]
fn torn_flag_alone_refuses_fresh_write_and_resume_retries_flush() {
    use std::sync::atomic::Ordering;
    for versioned in [false, true] {
        let (a, _b) = tokio::io::duplex(1 << 16);
        let psk: Arc<[u8]> = Arc::from(b"test-psk".as_slice());
        let (gate, blocked, flushes) = FlushGate::new(a);
        let mut client = if versioned {
            Snell::new_v3(gate, psk)
        } else {
            Snell::new(gate, psk)
        };
        let mut cx = Context::from_waker(std::task::Waker::noop());

        // Pipe is big enough that the whole staged frame drains into the
        // transport; only the flush pends. Codec `pending` ends empty.
        let frame = vec![0xAA; 4096];
        let mut progress = PacketFrameProgress::default();
        let poll = client.poll_write_packet_frame(&mut cx, &frame, &mut progress);
        assert!(
            matches!(poll, Poll::Pending),
            "v{}: blocked transport flush must park the write, got {poll:?}",
            if versioned { 3 } else { 4 }
        );

        // Abandon the write. Codec pending is empty — only the torn flag
        // can refuse the fresh write that follows.
        let mut fresh = PacketFrameProgress::default();
        match client.poll_write_packet_frame(&mut cx, &frame, &mut fresh) {
                Poll::Ready(Err(e)) => {
                    assert_eq!(e.kind(), io::ErrorKind::BrokenPipe);
                    assert!(
                        e.to_string().contains("desynced"),
                        "v{}: expected desync error, got {e}",
                        if versioned { 3 } else { 4 }
                    );
                }
                other => panic!(
                    "v{}: torn flag must refuse fresh progress even with empty codec pending, got {other:?}",
                    if versioned { 3 } else { 4 }
                ),
            }

        // Unblock the transport flush and resume the original progress —
        // it must retry the flush (a `has_pending_write` shortcut would
        // return Ready without flushing).
        blocked.store(false, Ordering::Relaxed);
        let poll = client.poll_write_packet_frame(&mut cx, &frame, &mut progress);
        assert!(
            matches!(poll, Poll::Ready(Ok(()))),
            "v{}: resume after flush unblocked must complete, got {poll:?}",
            if versioned { 3 } else { 4 }
        );
        assert!(
            flushes.load(Ordering::Relaxed) >= 2,
            "v{}: resume must retry the transport flush",
            if versioned { 3 } else { 4 }
        );
    }
}

/// Codec `pending` bytes left by a write that bypassed the frame gate
/// (raw `AsyncWrite` passthrough) are treated as torn: a fresh
/// `poll_write_packet_frame` must refuse even though `frame_write_torn`
/// was never armed.
#[test]
fn leftover_codec_pending_refuses_fresh_frame_write() {
    for versioned in [false, true] {
        let (a, _b) = tokio::io::duplex(1 << 10);
        let psk: Arc<[u8]> = Arc::from(b"test-psk".as_slice());
        let mut client = if versioned {
            Snell::new_v3(a, psk)
        } else {
            Snell::new(a, psk)
        };
        let mut cx = Context::from_waker(std::task::Waker::noop());

        // Raw AsyncWrite: 4 KiB into a 1 KiB pipe leaves undrained codec
        // pending bytes — `frame_write_torn` stays clear.
        let raw = vec![0xAA; 4096];
        let _ = Pin::new(&mut client).poll_write(&mut cx, &raw);

        let mut fresh = PacketFrameProgress::default();
        match client.poll_write_packet_frame(&mut cx, b"x", &mut fresh) {
            Poll::Ready(Err(e)) => {
                assert_eq!(e.kind(), io::ErrorKind::BrokenPipe);
                assert!(
                    e.to_string().contains("desynced"),
                    "v{}: expected desync error, got {e}",
                    if versioned { 3 } else { 4 }
                );
            }
            other => panic!(
                "v{}: leftover codec pending must refuse fresh progress, got {other:?}",
                if versioned { 3 } else { 4 }
            ),
        }
    }
}

/// A pended frame write resumed with the same progress must deliver the
/// frame intact — the peer decodes exactly the original bytes, so
/// re-staging, mis-credited `written`, or a skipped drain would corrupt
/// the wire image and fail this assertion.
#[test]
fn resumed_packet_frame_write_completes_with_intact_frame() {
    for versioned in [false, true] {
        let (a, b) = tokio::io::duplex(1 << 10);
        let psk: Arc<[u8]> = Arc::from(b"test-psk".as_slice());
        let mut client = if versioned {
            Snell::new_v3(a, Arc::clone(&psk))
        } else {
            Snell::new(a, Arc::clone(&psk))
        };
        let mut cx = Context::from_waker(std::task::Waker::noop());
        let frame = vec![0xAB; 4096];
        let mut progress = PacketFrameProgress::default();
        assert!(
            client
                .poll_write_packet_frame(&mut cx, &frame, &mut progress)
                .is_pending(),
            "v{}: frame must park on the tiny pipe",
            if versioned { 3 } else { 4 }
        );

        // Interleave: drain the wire from the peer codec, re-poll the
        // same progress — repeat until the write completes. Both peer
        // codecs decrypt the packet frame back to the original payload.
        let mut plain = Vec::new();
        let mut scratch = [0u8; 1 << 14];
        let mut completed = false;
        macro_rules! pump {
            ($peer:expr) => {
                for _ in 0..512 {
                    if !completed {
                        match client.poll_write_packet_frame(&mut cx, &frame, &mut progress) {
                            Poll::Ready(Ok(())) => completed = true,
                            Poll::Ready(Err(e)) => panic!("resume: {e}"),
                            Poll::Pending => {}
                        }
                    }
                    // Drain everything the wire currently holds — the
                    // peer codec buffers partial ciphertext internally
                    // and yields plaintext once the frame is complete.
                    loop {
                        let mut rb = ReadBuf::new(&mut scratch);
                        match Pin::new(&mut $peer).poll_read(&mut cx, &mut rb) {
                            Poll::Ready(Ok(())) => {
                                if rb.filled().is_empty() {
                                    break;
                                }
                                plain.extend_from_slice(rb.filled());
                            }
                            Poll::Ready(Err(e)) => panic!("peer read: {e}"),
                            Poll::Pending => break,
                        }
                    }
                    if completed && plain.len() >= frame.len() {
                        break;
                    }
                }
            };
        }
        if versioned {
            let mut peer = V3Conn::new(b, psk);
            pump!(peer);
        } else {
            let mut peer = V4Conn::new(b, psk);
            pump!(peer);
        }
        assert!(
            completed,
            "v{}: resumed write never completed",
            if versioned { 3 } else { 4 }
        );
        assert_eq!(
            plain,
            frame,
            "v{}: peer decoded corrupted frame",
            if versioned { 3 } else { 4 }
        );
    }
}

/// Happy-path guard for the non-poll variant: a completed frame write
/// clears the tear flag so subsequent writes proceed.
#[tokio::test]
async fn completed_packet_frame_write_leaves_conn_usable() {
    let (mut client, mut peer) = rig();
    within(client.write_packet_frame(b"datagram"))
        .await
        .unwrap();
    within(client.write_packet_frame(b"second")).await.unwrap();

    // Both frames arrive intact and in order — no torn prefixes.
    let mut got = [0u8; 64];
    let n = within(peer.read(&mut got)).await.unwrap();
    assert!(n >= b"datagram".len(), "peer saw {n} bytes");
}

/// Same guard on the v3 codec: two consecutive completed frame writes —
/// pins that the v3 completion path clears `frame_write_torn` (a second
/// write must not trip the tear gate).
#[tokio::test]
async fn completed_packet_frame_write_leaves_conn_usable_v3() {
    let (a, b) = tokio::io::duplex(1 << 16);
    let psk: Arc<[u8]> = Arc::from(b"test-psk".as_slice());
    let mut client = Snell::new_v3(a, Arc::clone(&psk));
    let mut peer = V3Conn::new(b, psk);

    within(client.write_packet_frame(b"datagram"))
        .await
        .unwrap();
    within(client.write_packet_frame(b"second")).await.unwrap();

    let mut got = [0u8; 64];
    let n = within(peer.read(&mut got)).await.unwrap();
    assert!(n >= b"datagram".len(), "peer saw {n} bytes");
}

#[tokio::test]
async fn write_udp_header_layout() {
    let (mut client, mut peer) = rig();
    within(write_udp_header(&mut client)).await.unwrap();
    within(client.flush()).await.unwrap();

    let mut got = [0u8; 3];
    within(peer.read_exact(&mut got)).await.unwrap();
    assert_eq!(got, [HEADER_VERSION, COMMAND_UDP, 0x00]);
}

#[tokio::test]
async fn read_reply_accepts_tunnel_and_pong() {
    let (mut client, mut peer) = rig();
    within(peer.write_all(&[RESPONSE_TUNNEL])).await.unwrap();
    within(peer.flush()).await.unwrap();
    within(client.read_reply()).await.unwrap();

    let (mut client, mut peer) = rig();
    within(peer.write_all(&[RESPONSE_PONG])).await.unwrap();
    within(peer.flush()).await.unwrap();
    within(client.read_reply()).await.unwrap();
}

#[tokio::test]
async fn read_reply_parses_error_code_and_message() {
    let (mut client, mut peer) = rig();
    within(peer.write_all(&[RESPONSE_ERROR, 42, 5, b'o', b'o', b'p', b's', b'!']))
        .await
        .unwrap();
    within(peer.flush()).await.unwrap();

    let err = within(client.read_reply()).await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("42"), "missing error code in: {msg}");
    assert!(msg.contains("oops!"), "missing error message in: {msg}");
}

#[tokio::test]
async fn read_reply_rejects_unknown_code() {
    let (mut client, mut peer) = rig();
    within(peer.write_all(&[0x7F])).await.unwrap();
    within(peer.flush()).await.unwrap();

    let err = within(client.read_reply()).await.unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    assert!(
        err.to_string().contains("unknown response code"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn first_read_consumes_reply_then_yields_data() {
    let (mut client, mut peer) = rig();
    within(peer.write_all(&[RESPONSE_TUNNEL])).await.unwrap();
    within(peer.write_all(b"hello")).await.unwrap();
    within(peer.flush()).await.unwrap();

    let mut buf = [0u8; 16];
    let n = within(client.read(&mut buf)).await.unwrap();
    assert_eq!(&buf[..n], b"hello");

    // Reply already consumed by the first read — explicit call is a no-op.
    within(client.read_reply()).await.unwrap();
}

#[tokio::test]
async fn zero_chunk_maps_to_clean_eof() {
    let (mut client, mut peer) = rig();
    within(peer.write_all(&[RESPONSE_TUNNEL])).await.unwrap();
    within(peer.flush()).await.unwrap();
    within(write_zero_chunk(&mut peer)).await.unwrap();

    let mut buf = [0u8; 8];
    let n = within(client.read(&mut buf)).await.unwrap();
    assert_eq!(n, 0, "zero chunk should surface as clean EOF");
}

#[tokio::test]
async fn reset_reply_state_consumes_next_status_byte() {
    let (mut client, mut peer) = rig();
    within(peer.write_all(&[RESPONSE_TUNNEL])).await.unwrap();
    within(peer.write_all(b"hello")).await.unwrap();
    within(peer.flush()).await.unwrap();

    let mut buf = [0u8; 16];
    let n = within(client.read(&mut buf)).await.unwrap();
    assert_eq!(&buf[..n], b"hello");

    // Pool-reuse semantics: the next request's status byte is pending
    // again after a reset.
    client.reset_reply_state();
    within(peer.write_all(&[RESPONSE_TUNNEL])).await.unwrap();
    within(peer.write_all(b"again")).await.unwrap();
    within(peer.flush()).await.unwrap();

    let n = within(client.read(&mut buf)).await.unwrap();
    assert_eq!(&buf[..n], b"again");
}

#[tokio::test]
async fn write_packet_frame_rejects_oversize() {
    let (mut client, _peer) = rig();
    let oversize = vec![0u8; MAX_PAYLOAD_LENGTH + 1];
    let err = within(client.write_packet_frame(&oversize))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

    // The rejection happens before the tear flag arms — the conn stays
    // usable for a normal datagram.
    within(client.write_packet_frame(b"after")).await.unwrap();
}

#[tokio::test]
async fn write_packet_frame_roundtrips() {
    let (mut client, mut peer) = rig();
    let n = within(client.write_packet_frame(b"datagram"))
        .await
        .unwrap();
    assert_eq!(n, b"datagram".len());

    // Each datagram is exactly one AEAD frame, so a single read drains it.
    let mut buf = [0u8; 64];
    let m = within(peer.read(&mut buf)).await.unwrap();
    assert_eq!(&buf[..m], b"datagram");
}
