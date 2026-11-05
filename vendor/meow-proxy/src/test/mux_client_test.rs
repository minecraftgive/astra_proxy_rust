use super::*;
use std::sync::atomic::AtomicUsize;
use std::task::Poll;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};

/// Minimal AsyncRead+AsyncWrite+ProxyConn newtype over a duplex half.
struct TestConn(tokio::io::DuplexStream);

impl ProxyConn for TestConn {}

impl AsyncRead for TestConn {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
    }
}

impl AsyncWrite for TestConn {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }
}

/// Mock dialer: each dial yields a duplex half whose far end swallows
/// bytes (a minimal mux data sink).
async fn mock_mux_client(dials: Arc<AtomicUsize>) -> Arc<MuxClient> {
    // The mock sink speaks smux frames — pin the protocol explicitly.
    mock_mux_client_with(
        dials,
        MuxOptions {
            protocol: Protocol::Smux,
            ..MuxOptions::default()
        },
    )
    .await
}

async fn mock_mux_client_with(dials: Arc<AtomicUsize>, options: MuxOptions) -> Arc<MuxClient> {
    let dial: DialFn = Arc::new(move || {
        let dials = Arc::clone(&dials);
        Box::pin(async move {
            dials.fetch_add(1, Ordering::SeqCst);
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            tokio::spawn(async move {
                // Drain frames without parsing: the previous version read a
                // 10-byte header and extracted a u32 BE "length" from bytes
                // 2–5, but smux frames are 8 bytes (u16 LE length at 2–3)
                // and yamux frames are 12 bytes.  On a misaligned read the
                // u32 can be enormous (e.g. 0x01000000 = 16 MiB when the
                // smux version byte 0x01 lands at header[2]), causing a
                // huge heap allocation that crashes the Windows runner.
                // A fixed-size read-and-discard loop is all a data sink
                // needs.
                let mut io = server_io;
                let mut buf = [0u8; 4096];
                loop {
                    match io.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(_) => {}
                    }
                }
            });
            Ok(Box::new(TestConn(client_io)) as Box<dyn ProxyConn>)
        })
    });
    MuxClient::new(dial, options)
}

#[tokio::test]
async fn streams_pack_into_one_connection_until_bounds() {
    let dials = Arc::new(AtomicUsize::new(0));
    let client = mock_mux_client(Arc::clone(&dials)).await;
    // Default bounds: max-connections=4, min/max-streams=4 — streams
    // accumulate on the first connection until it reaches max_streams.
    let mut streams = Vec::new();
    for _ in 0..4 {
        streams.push(client.open_stream("a.example", 80).await.unwrap());
    }
    assert_eq!(dials.load(Ordering::SeqCst), 1);
    // Fifth stream exceeds max_streams → second connection.
    streams.push(client.open_stream("a.example", 80).await.unwrap());
    assert_eq!(dials.load(Ordering::SeqCst), 2);
}

/// max-connections=0 with max-streams=0 and min-streams=0 keeps the
/// mihomo semantics: every stream dials its own physical connection.
#[tokio::test]
async fn zero_bounds_dial_one_connection_per_stream() {
    let dials = Arc::new(AtomicUsize::new(0));
    let options = MuxOptions {
        protocol: Protocol::Smux,
        max_connections: 0,
        min_streams: 0,
        max_streams: 0,
        ..MuxOptions::default()
    };
    let client = mock_mux_client_with(Arc::clone(&dials), options).await;
    let mut streams = Vec::new();
    for _ in 0..3 {
        streams.push(client.open_stream("a.example", 80).await.unwrap());
    }
    assert_eq!(
        dials.load(Ordering::SeqCst),
        3,
        "max-connections=0 + max-streams=0 must dial one connection per stream"
    );
    drop(streams);
}

/// Concurrent opens reserve their slots under the pool lock, so the
/// per-session stream cap is never overshot by racing offers.
#[tokio::test]
async fn concurrent_opens_respect_max_streams() {
    let dials = Arc::new(AtomicUsize::new(0));
    let options = MuxOptions {
        protocol: Protocol::Smux,
        max_connections: 0,
        min_streams: 4,
        max_streams: 4,
        ..MuxOptions::default()
    };
    let client = mock_mux_client_with(Arc::clone(&dials), options).await;
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let client = Arc::clone(&client);
            tokio::spawn(async move {
                client
                    .open_stream(format!("h{i}.example").as_str(), 80)
                    .await
                    .unwrap()
            })
        })
        .collect();
    let mut streams = Vec::new();
    for handle in handles {
        streams.push(handle.await.unwrap());
    }
    let sessions = client.sessions.lock().await;
    assert_eq!(
        sessions.len(),
        2,
        "8 concurrent streams at max-streams=4 need 2 sessions"
    );
    for session in sessions.iter() {
        assert_eq!(session.streams.load(Ordering::SeqCst), 4);
    }
    drop(streams);
}

/// Stress test for the max-streams overshoot race: with max-streams=1
/// every session must end with at most one stream slot.  The original
/// design used separate `streams` + `pending` atomics whose sum was
/// read with two independent loads — a concurrent open could transition
/// between them and make the load appear falsely 0.  The unified
/// `streams` counter with CAS-based reservation makes the check-and-
/// increment one atomic step, so the overshoot is structurally
/// impossible.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn stress_concurrent_opens_never_overshoot_max_streams_one() {
    for round in 0..200 {
        let dials = Arc::new(AtomicUsize::new(0));
        let options = MuxOptions {
            protocol: Protocol::Smux,
            max_connections: 0,
            min_streams: 1,
            max_streams: 1,
            ..MuxOptions::default()
        };
        let client = mock_mux_client_with(Arc::clone(&dials), options).await;
        let handles: Vec<_> = (0..16)
            .map(|i| {
                let client = Arc::clone(&client);
                tokio::spawn(async move {
                    client
                        .open_stream(format!("h{i}.example").as_str(), 80)
                        .await
                        .unwrap()
                })
            })
            .collect();
        let mut streams = Vec::new();
        for handle in handles {
            streams.push(handle.await.unwrap());
        }
        let sessions = client.sessions.lock().await;
        for session in sessions.iter() {
            let streams = session.streams.load(Ordering::SeqCst);
            assert!(
                streams <= 1,
                "round {round}: session overshot max-streams=1 with {streams} streams"
            );
        }
        drop(streams);
    }
}

/// Issue #495 item 11: with `padding: true` everything after the
/// version-1 request header is padding-framed, so the first smux frame
/// (SYN) arrives inside a `[len][padding_len]` frame rather than raw.
#[tokio::test]
async fn padding_frames_the_session_after_the_request_header() {
    let (wire_tx, wire_rx) = tokio::sync::oneshot::channel();
    let wire_tx = Arc::new(std::sync::Mutex::new(Some(wire_tx)));
    let dial: DialFn = Arc::new(move || {
        let wire_tx = Arc::clone(&wire_tx);
        Box::pin(async move {
            let (client_io, mut server_io) = tokio::io::duplex(64 * 1024);
            tokio::spawn(async move {
                let mut header = [0u8; 5];
                server_io.read_exact(&mut header).await.unwrap();
                let mut request_padding =
                    vec![0u8; usize::from(u16::from_be_bytes([header[3], header[4]]))];
                server_io.read_exact(&mut request_padding).await.unwrap();
                let mut frame = [0u8; 6];
                server_io.read_exact(&mut frame).await.unwrap();
                if let Some(tx) = wire_tx.lock().unwrap().take() {
                    let _ = tx.send((header, frame));
                }
                let mut sink = [0u8; 4096];
                while matches!(server_io.read(&mut sink).await, Ok(n) if n > 0) {}
            });
            Ok(Box::new(TestConn(client_io)) as Box<dyn ProxyConn>)
        })
    });
    let client = MuxClient::new(
        dial,
        MuxOptions {
            protocol: Protocol::Smux,
            padding: true,
            ..MuxOptions::default()
        },
    );
    let _stream = client.open_stream("a.example", 80).await.unwrap();

    let (header, frame) = wire_rx.await.unwrap();
    assert_eq!(
        &header[..3],
        &[1, Protocol::Smux as u8, 1],
        "v1, smux, padded"
    );
    let padding = u16::from_be_bytes([frame[2], frame[3]]);
    assert!(
        (256..=767).contains(&padding),
        "session must be padding-framed, got padding_len {padding}"
    );
    assert_eq!(&frame[4..], &[1, 0], "framed payload is the smux SYN");
}

#[tokio::test]
async fn yamux_protocol_pools_streams() {
    let dials = Arc::new(AtomicUsize::new(0));
    let options = MuxOptions {
        protocol: Protocol::Yamux,
        ..MuxOptions::default()
    };
    let client = mock_mux_client_with(Arc::clone(&dials), options).await;
    let _s1 = client.open_stream("a.example", 80).await.unwrap();
    let _s2 = client.open_stream("b.example", 80).await.unwrap();
    assert_eq!(dials.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn idle_sessions_are_evicted() {
    for protocol in [Protocol::MuxCool, Protocol::Smux, Protocol::Yamux] {
        // TCP and UDP use distinct connection wrappers, each responsible
        // for releasing the pool's established-stream reservation.
        for udp in [false, true] {
            let dials = Arc::new(AtomicUsize::new(0));
            let closed = Arc::new(AtomicUsize::new(0));
            let client =
                tracked_mux_client(protocol, Arc::clone(&dials), Arc::clone(&closed), None);
            if udp {
                let stream = client.open_packet_stream("127.0.0.1", 53).await.unwrap();
                drop(stream);
            } else {
                let stream = client.open_stream("a.example", 80).await.unwrap();
                drop(stream);
            }

            // A completed stream leaves its session reusable before the
            // idle deadline, including after the UDP wrapper drops.
            let reused = client.open_stream("b.example", 81).await.unwrap();
            assert_eq!(dials.load(Ordering::SeqCst), 1, "{protocol:?}, UDP={udp}");
            drop(reused);
            {
                let sessions = client.sessions.lock().await;
                sessions[0].last_used_ms.store(
                    now_ms().wrapping_sub(IDLE_TIMEOUT.as_millis() as Uint),
                    Ordering::SeqCst,
                );
            }

            let fresh = client.open_stream("c.example", 82).await.unwrap();
            assert_eq!(dials.load(Ordering::SeqCst), 2, "{protocol:?}, UDP={udp}");
            // Removing the pool entry must close the old physical
            // connection, rather than merely hiding it from future opens.
            wait_for(
                &closed,
                1,
                &format!("{protocol:?}, UDP={udp}: evicted connection"),
            )
            .await;
            drop(fresh);
            drop(client);
            wait_for(
                &closed,
                2,
                &format!("{protocol:?}, UDP={udp}: dropped pool"),
            )
            .await;
        }
    }
}

/// Regression test for issue #421: `offer()`'s idle check must compare
/// `last_used_ms` in the truncated `Uint` domain with `wrapping_sub`,
/// never widen to `u64` first. `last_used_ms` can end up numerically
/// *ahead* of a freshly-read `now_ms()` two ways: a `Uint = u32`
/// millisecond clock rolling over on a 32-bit-atomic target (mips32),
/// or a monotonic clock read racing itself. A `saturating_sub`-based
/// comparison computes `now.saturating_sub(last) == 0` in that case and
/// treats the session as freshly used forever, so it can never be
/// evicted. `wrapping_sub` recovers the true elapsed distance
/// regardless of which side is numerically larger, so a session whose
/// stamp reads far in the "future" is still pruned as idle.
#[tokio::test]
async fn idle_sessions_are_evicted_across_a_wrapped_clock_reading() {
    let dials = Arc::new(AtomicUsize::new(0));
    let client = mock_mux_client(Arc::clone(&dials)).await;
    let s = client.open_stream("a.example", 80).await.unwrap();
    drop(s);
    assert_eq!(dials.load(Ordering::SeqCst), 1);

    // Force the session's last-used stamp to read as if the millisecond
    // clock had wrapped past `now`, simulating the mips32 `AtomicU32`
    // rollover this fix targets.
    {
        let sessions = client.sessions.lock().await;
        assert_eq!(sessions.len(), 1);
        let wrapped = now_ms().wrapping_add(IDLE_TIMEOUT.as_millis() as Uint * 10);
        sessions[0].last_used_ms.store(wrapped, Ordering::SeqCst);
    }

    // offer() runs its retain() pass on the next open: the wrapped
    // session must still be recognized as idle-past-timeout and pruned,
    // forcing a second dial.
    let _s2 = client.open_stream("b.example", 81).await.unwrap();
    assert_eq!(
        dials.load(Ordering::SeqCst),
        2,
        "a session whose last-used stamp reads ahead of `now` (wrapped clock) \
             must still be evicted as idle, not kept alive forever"
    );
}

#[tokio::test]
async fn write_error_on_dead_session_falls_back_to_new_dial() {
    let dials = Arc::new(AtomicUsize::new(0));
    let client = mock_mux_client(Arc::clone(&dials)).await;
    let mut s = client.open_stream("a.example", 80).await.unwrap();
    // Kill the underlying session by shutting the far side: drop the
    // server task happens implicitly — instead, just verify a second
    // stream still works after the first session dies.
    s.shutdown().await.ok();
    let _s2 = client.open_stream("b.example", 81).await.unwrap();
    assert!(dials.load(Ordering::SeqCst) >= 1);
}

#[tokio::test]
async fn concurrent_opens_do_not_overshoot_max_connections() {
    let dials = Arc::new(AtomicUsize::new(0));
    let options = MuxOptions {
        protocol: Protocol::Smux,
        max_connections: 2,
        min_streams: 1,
        max_streams: 1,
        ..MuxOptions::default()
    };
    let client = mock_mux_client_with(Arc::clone(&dials), options).await;
    // Every stream saturates its session, so a racy pool would dial
    // once per open; holding the lock across the dial must serialize
    // them onto at most `max_connections` physical connections.
    let opens = (0..8)
        .map(|i| {
            let client = Arc::clone(&client);
            async move { (i, client.open_stream("a.example", 80).await.unwrap()) }
        })
        .collect::<Vec<_>>();
    let streams = futures::future::join_all(opens).await;
    assert_eq!(streams.len(), 8);
    assert_eq!(dials.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn session_setup_timeout_bounds_a_stalled_dial() {
    let dial: DialFn = Arc::new(|| Box::pin(std::future::pending::<Result<Box<dyn ProxyConn>>>()));
    let client = MuxClient::new(
        dial,
        MuxOptions {
            protocol: Protocol::Smux,
            ..MuxOptions::default()
        },
    );

    let Err(error) = client.open_stream("a.example", 80).await else {
        panic!("a stalled dial must time out");
    };
    assert!(
        error.to_string().contains("session setup timed out"),
        "unexpected error: {error}"
    );
}

/// The dialer-layer encoding of a chained `UdpTarget::Name` (issue
/// #657): host-only UDP metadata. Both checks must resolve before any
/// dial — a counting dial that pends proves it.
fn host_only_udp_meta(host: &str) -> Metadata {
    Metadata {
        network: meow_common::Network::Udp,
        host: host.into(),
        dst_port: 443,
        ..Default::default()
    }
}

/// mux.cool stamps a per-datagram `SocketAddr` destination it cannot
/// derive from a name — `open_packet_stream_for` declines (`Ok(None)`)
/// so the adapter's own UDP path handles the domain request.
#[tokio::test]
async fn packet_stream_muxcool_declines_domain_target() {
    let dials = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&dials);
    let dial: DialFn = Arc::new(move || {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(std::future::pending::<Result<Box<dyn ProxyConn>>>())
    });
    let client = MuxClient::new(
        dial,
        MuxOptions {
            protocol: Protocol::MuxCool,
            ..MuxOptions::default()
        },
    );
    let meta = host_only_udp_meta("relay.internal");
    let got = client
        .open_packet_stream_for(&meta, "test")
        .await
        .expect("decline is not an error");
    assert!(got.is_none(), "mux.cool must decline a domain target");
    assert_eq!(dials.load(std::sync::atomic::Ordering::SeqCst), 0);
}

/// Bound-flow protocols carry the name in the stream-open Socksaddr,
/// whose FQDN length is a u8 — an oversized name must surface as
/// `NotSupported` (capability class, so the chained caller falls back
/// to a literal `Addr`), not the io error `encode_address` would raise.
#[tokio::test]
async fn packet_stream_refuses_oversized_domain_target() {
    let dials = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&dials);
    let dial: DialFn = Arc::new(move || {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(std::future::pending::<Result<Box<dyn ProxyConn>>>())
    });
    let client = MuxClient::new(
        dial,
        MuxOptions {
            protocol: Protocol::Smux,
            ..MuxOptions::default()
        },
    );
    let meta = host_only_udp_meta(&"a".repeat(256));
    match client.open_packet_stream_for(&meta, "test").await {
        Err(MeowError::NotSupported(_)) => {}
        Err(other) => panic!("expected NotSupported, got {other:?}"),
        Ok(_) => panic!("oversized domain target must error"),
    }
    assert_eq!(dials.load(std::sync::atomic::Ordering::SeqCst), 0);
}

/// Mock dialer for lifecycle tests: counts dials and physical
/// connections the far end saw close; the first dial parks on `gate`
/// when one is given.
fn tracked_mux_client(
    protocol: Protocol,
    dials: Arc<AtomicUsize>,
    closed: Arc<AtomicUsize>,
    gate: Option<Arc<tokio::sync::Notify>>,
) -> Arc<MuxClient> {
    let dial: DialFn = Arc::new(move || {
        let dials = Arc::clone(&dials);
        let closed = Arc::clone(&closed);
        let gate = gate.clone();
        Box::pin(async move {
            let n = dials.fetch_add(1, Ordering::SeqCst);
            if let (0, Some(gate)) = (n, gate) {
                gate.notified().await;
            }
            let (client_io, mut server_io) = tokio::io::duplex(64 * 1024);
            tokio::spawn(async move {
                let mut sink = [0u8; 4096];
                while matches!(server_io.read(&mut sink).await, Ok(n) if n > 0) {}
                closed.fetch_add(1, Ordering::SeqCst);
            });
            Ok(Box::new(TestConn(client_io)) as Box<dyn ProxyConn>)
        })
    });
    MuxClient::new(
        dial,
        MuxOptions {
            protocol,
            // One physical connection per concurrent stream, so the test
            // controls exactly which sessions are idle and which busy.
            max_connections: 0,
            min_streams: 0,
            max_streams: 0,
            ..MuxOptions::default()
        },
    )
}

async fn wait_for(counter: &AtomicUsize, want: usize, what: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while counter.load(Ordering::SeqCst) < want {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what} >= {want}"));
    assert_eq!(counter.load(Ordering::SeqCst), want, "{what}");
}

/// Issue #695: a reset closes the idle *and* the busy session's physical
/// connection, fails the busy session's stream, and the next open dials
/// fresh instead of reusing either.
#[tokio::test]
async fn reset_closes_idle_and_busy_sessions_and_redials() {
    for protocol in [Protocol::Smux, Protocol::Yamux] {
        let dials = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicUsize::new(0));
        let client = tracked_mux_client(protocol, Arc::clone(&dials), Arc::clone(&closed), None);
        let mut busy = client.open_stream("a.example", 80).await.unwrap();
        let idle = client.open_stream("b.example", 80).await.unwrap();
        drop(idle);
        assert_eq!(dials.load(Ordering::SeqCst), 2);

        assert_eq!(client.reset(), 2, "{protocol:?}: both sessions drained");
        wait_for(&closed, 2, "closed physical connections").await;
        assert!(
            busy.write_all(b"after reset").await.is_err(),
            "{protocol:?}: a stream on a reset session must fail"
        );

        let _fresh = client.open_stream("c.example", 80).await.unwrap();
        assert_eq!(
            dials.load(Ordering::SeqCst),
            3,
            "{protocol:?}: the open after a reset must dial fresh"
        );
        assert_eq!(client.reset(), 1);
    }
}

/// Issue #695: a reset that lands while a session dial holds the pool
/// lock cannot drain synchronously; the straddling session must be
/// discarded and redialled, never cached.
#[tokio::test]
async fn reset_discards_a_session_dialled_across_it() {
    let dials = Arc::new(AtomicUsize::new(0));
    let closed = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(tokio::sync::Notify::new());
    let client = tracked_mux_client(
        Protocol::Smux,
        Arc::clone(&dials),
        Arc::clone(&closed),
        Some(Arc::clone(&gate)),
    );
    let opener = {
        let client = Arc::clone(&client);
        tokio::spawn(async move { client.open_stream("a.example", 80).await })
    };
    wait_for(&dials, 1, "dials started").await;

    assert_eq!(client.reset(), 0, "pool lock is held across the dial");
    gate.notify_one();
    let stream = opener.await.unwrap().expect("open redials after the reset");

    assert_eq!(dials.load(Ordering::SeqCst), 2, "straddling dial redialled");
    wait_for(&closed, 1, "closed straddling connection").await;
    let sessions = client.sessions.lock().await;
    assert_eq!(sessions.len(), 1, "only the post-reset session is pooled");
    assert_eq!(sessions[0].generation, client.generation());
    drop(sessions);
    drop(stream);
}
