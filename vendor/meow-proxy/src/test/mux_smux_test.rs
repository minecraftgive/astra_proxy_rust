use super::*;
use std::sync::atomic::AtomicBool;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[test]
fn frame_wire_layout_matches_sagernet_smux() {
    // SYN on stream 1: [ver=1][cmd=SYN][len=0 u16 LE][sid=1 u32 LE].
    let frame = Frame {
        cmd: CMD_SYN,
        stream_id: 1,
        data: Bytes::new(),
    };
    let buf = frame.encode();
    assert_eq!(&buf[..], &[0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00]);

    // PSH with 3 bytes on stream 3.
    let frame = Frame {
        cmd: CMD_PSH,
        stream_id: 3,
        data: Bytes::from_static(b"abc"),
    };
    let buf = frame.encode();
    assert_eq!(
        &buf[..],
        &[0x01, 0x02, 0x03, 0x00, 0x03, 0x00, 0x00, 0x00, b'a', b'b', b'c']
    );

    let (cmd, length, sid) = Frame::decode_header(&buf[..FRAME_HEADER_LEN]).unwrap();
    assert_eq!(cmd, CMD_PSH);
    assert_eq!(length, 3);
    assert_eq!(sid, 3);
}

/// Minimal smux server over a duplex pipe (same sagernet v1 codec):
/// echoes PSH payloads back on the same stream and answers FIN with FIN.
async fn run_echo_server<S>(mut io: S) -> tokio::task::JoinHandle<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut header = [0u8; FRAME_HEADER_LEN];
        loop {
            if io.read_exact(&mut header).await.is_err() {
                return;
            }
            let Ok((cmd, length, stream_id)) = Frame::decode_header(&header) else {
                return;
            };
            let mut payload = vec![0u8; length];
            if !payload.is_empty() && io.read_exact(&mut payload).await.is_err() {
                return;
            }
            match cmd {
                CMD_PSH => {
                    let echoed = payload.clone();
                    let mut buf = BytesMut::with_capacity(FRAME_HEADER_LEN + echoed.len());
                    buf.put_u8(SMUX_VERSION);
                    buf.put_u8(CMD_PSH);
                    buf.put_u16_le(echoed.len() as u16);
                    buf.put_u32_le(stream_id);
                    buf.put_slice(&echoed);
                    if io.write_all(&buf).await.is_err() {
                        return;
                    }
                }
                CMD_FIN => {
                    let mut buf = BytesMut::with_capacity(FRAME_HEADER_LEN);
                    buf.put_u8(SMUX_VERSION);
                    buf.put_u8(CMD_FIN);
                    buf.put_u16_le(0);
                    buf.put_u32_le(stream_id);
                    if io.write_all(&buf).await.is_err() {
                        return;
                    }
                }
                _ => {}
            }
        }
    })
}

#[tokio::test]
async fn open_stream_echo_round_trip() {
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let _server = run_echo_server(server_io).await;
    let session = Arc::new(Session::client(client_io).unwrap());
    let mut stream = session.open_stream().await.unwrap();

    stream.write_all(b"hello smux").await.unwrap();
    let mut resp = [0u8; 10];
    stream.read_exact(&mut resp).await.unwrap();
    assert_eq!(&resp, b"hello smux");
}

#[tokio::test]
async fn two_streams_multiplex_independently() {
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let _server = run_echo_server(server_io).await;
    let session = Arc::new(Session::client(client_io).unwrap());

    let mut a = session.open_stream().await.unwrap();
    let mut b = session.open_stream().await.unwrap();

    a.write_all(b"AAA").await.unwrap();
    b.write_all(b"BBB").await.unwrap();
    let mut buf = [0u8; 3];
    a.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"AAA");
    b.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"BBB");
}

/// A 33-frame backlog is a normal burst, not a hog. Under a frame-count
/// trigger it retired the stream; the byte cap must leave it open, keep
/// the peer flowing, and let the consumer catch up on every byte.
#[tokio::test]
async fn small_backlog_neither_aborts_nor_blocks_peers() {
    const BACKLOG: usize = 33;
    let (client_io, mut server_io) = tokio::io::duplex(64 * 1024);
    let session = Arc::new(Session::client(client_io).unwrap());
    let mut slow = session.open_stream().await.unwrap();
    let mut peer = session.open_stream().await.unwrap();

    let mut wire = Vec::new();
    for _ in 0..BACKLOG {
        wire.extend_from_slice(&encode_frame(CMD_PSH, slow.id, b"x"));
    }
    wire.extend_from_slice(&encode_frame(CMD_PSH, peer.id, b"ready"));
    server_io.write_all(&wire).await.unwrap();

    let mut ready = [0u8; 5];
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        peer.read_exact(&mut ready),
    )
    .await
    .expect("a backed-up stream must not block its peers")
    .unwrap();
    assert_eq!(&ready, b"ready");

    let mut drained = vec![0u8; BACKLOG];
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        slow.read_exact(&mut drained),
    )
    .await
    .expect("the whole backlog must stay deliverable")
    .unwrap();
    assert!(drained.iter().all(|b| *b == b'x'));

    // Still open: the next read parks instead of erroring.
    let next = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        slow.read(&mut [0u8; 1]),
    )
    .await;
    assert!(next.is_err(), "a small backlog must not retire the stream");
    assert!(!session.is_dead());
}

/// A stream that holds more than its receive share is retired — but
/// only after a full [`STREAM_STALL_GRACE`] window, never at frame
/// arrival. What it already buffered is still delivered, the consumer
/// then sees `ConnectionAborted`, and neither the session nor its
/// peers are affected.
#[tokio::test(start_paused = true)]
async fn stalled_hog_retires_after_grace_and_only_the_hog() {
    // Four 32 KiB frames land exactly at the share; the fifth crosses
    // it and arms the watchdog.
    let frames_over_cap = MAX_STREAM_BUFFER / MAX_FRAME_SIZE + 1;
    let hog_bytes = frames_over_cap * MAX_FRAME_SIZE;
    let (client_io, mut server_io) = tokio::io::duplex(2 * MAX_STREAM_BUFFER);
    let session = Arc::new(Session::client(client_io).unwrap());
    let mut hog = session.open_stream().await.unwrap();
    let mut peer = session.open_stream().await.unwrap();

    let big = vec![0x5au8; MAX_FRAME_SIZE];
    let mut wire = Vec::new();
    for _ in 0..frames_over_cap {
        wire.extend_from_slice(&encode_frame(CMD_PSH, hog.id, &big));
    }
    wire.extend_from_slice(&encode_frame(CMD_PSH, peer.id, b"ready"));
    server_io.write_all(&wire).await.unwrap();

    // While the hog sits over its share — before any grace has elapsed
    // — the peer still receives: the over-share condition alone must
    // not disturb anyone.
    let mut ready = [0u8; 5];
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        peer.read_exact(&mut ready),
    )
    .await
    .expect("a hog stream must not block its peers")
    .unwrap();
    assert_eq!(&ready, b"ready");

    // The consumer never drains a byte, so once the full grace window
    // has elapsed the watchdog retires the stream. Paused time
    // auto-advances to the watchdog timer once every task parks.
    tokio::time::sleep(STREAM_STALL_GRACE * 2).await;

    // Data queued before the retire must still be delivered, then the
    // abort surfaces.
    let mut buffered = vec![0u8; hog_bytes];
    hog.read_exact(&mut buffered)
        .await
        .expect("data queued before the retire must still be delivered");
    assert!(buffered.iter().all(|b| *b == 0x5a));
    let error = hog.read(&mut [0u8; 1]).await.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::ConnectionAborted);

    // The retire stayed scoped: the peer keeps its stream and the
    // session survives.
    let next = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        peer.read(&mut [0u8; 1]),
    )
    .await;
    assert!(next.is_err(), "the peer stream must still be open");
    assert!(!session.is_dead());
}

/// Streams pinned at the per-stream cap can exhaust the session receive
/// budget, and the consumers holding it are by definition not polling —
/// the relay stops polling a read half whose write half is blocked — so
/// nothing below can reclaim it. The reader must abandon the session
/// rather than leave it wedged forever.
#[tokio::test(start_paused = true)]
async fn wedged_receive_budget_abandons_the_session() {
    let frames_per_stream = MAX_STREAM_BUFFER / MAX_FRAME_SIZE;
    let hogs = MAX_RECEIVE_BUFFER / MAX_STREAM_BUFFER;
    // `hogs` is the session's concurrency headroom: this many streams
    // have to pin themselves at the cap before the reader can wedge.
    // `MuxClient::offer` places no bound on streams per session once
    // max-connections is reached, so a small divisor would make the
    // wedge reachable in ordinary traffic.
    const {
        assert!(
            MAX_RECEIVE_BUFFER / MAX_STREAM_BUFFER >= 32,
            "the receive budget must leave headroom for many streams per session"
        );
    };
    let (client_io, mut server_io) = tokio::io::duplex(MAX_RECEIVE_BUFFER + 64 * 1024);
    let session = Arc::new(Session::client(client_io).unwrap());
    let mut streams = Vec::new();
    for _ in 0..hogs + 1 {
        streams.push(session.open_stream().await.unwrap());
    }
    let ids: Vec<u32> = streams.iter().map(|s| s.id).collect();

    let big = vec![0x5au8; MAX_FRAME_SIZE];
    let writer = tokio::spawn(async move {
        for id in ids.iter().take(hogs) {
            for _ in 0..frames_per_stream {
                if server_io
                    .write_all(&encode_frame(CMD_PSH, *id, &big))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
        // Never acquired: the budget is already exhausted by the hogs.
        let _ = server_io
            .write_all(&encode_frame(CMD_PSH, ids[hogs], b"alive"))
            .await;
    });

    // Paused time auto-advances once every task is idle, so the stall
    // deadline resolves without a real wait. The victim's frame was never
    // dispatched, so it sees the abandoned session instead of its data.
    let error = streams[hogs].read(&mut [0u8; 1]).await.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    assert!(
        session.is_dead(),
        "a session that cannot make progress must be abandoned"
    );
    // The last hog was served in full, so the reader really did consume
    // the whole budget and only then stop — pinning the teardown to the
    // acquire rather than to some other session-death path.
    let mut buffered = vec![0u8; MAX_STREAM_BUFFER];
    streams[hogs - 1].read_exact(&mut buffered).await.unwrap();
    assert!(buffered.iter().all(|b| *b == 0x5a));
    writer.await.unwrap();
}

/// The inbox depth still matters, but through the spill and the
/// grace-gated watchdog rather than a synchronous cut: frames small
/// enough that [`STREAM_QUEUE`] of them cost less payload than their
/// queue slots fill the inbox, overflow into the spill, and retire the
/// stream only when the consumer has drained nothing for a whole
/// [`STREAM_STALL_GRACE`]. Retiring stays scoped, and everything
/// already queued — inbox and spill alike — is still delivered.
#[tokio::test(start_paused = true)]
async fn inbox_depth_overflow_retires_only_a_never_draining_consumer() {
    // Guards the premise: the depth bound has to be reached first, or
    // this test silently exercises the byte cap instead.
    const {
        assert!(
            STREAM_QUEUE + 1 < MAX_STREAM_BUFFER,
            "inbox depth must bind before the byte cap, or this test proves nothing"
        );
    };
    let frames = STREAM_QUEUE + 1;
    let (client_io, mut server_io) = tokio::io::duplex(64 * 1024);
    let session = Arc::new(Session::client(client_io).unwrap());
    let mut slow = session.open_stream().await.unwrap();
    let mut peer = session.open_stream().await.unwrap();

    let mut wire = Vec::new();
    for _ in 0..frames {
        wire.extend_from_slice(&encode_frame(CMD_PSH, slow.id, b"x"));
    }
    wire.extend_from_slice(&encode_frame(CMD_PSH, peer.id, b"ready"));
    server_io.write_all(&wire).await.unwrap();

    // The overflow is already parked while the peers keep flowing.
    let mut ready = [0u8; 5];
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        peer.read_exact(&mut ready),
    )
    .await
    .expect("an overflowing stream must not block its peers")
    .unwrap();
    assert_eq!(&ready, b"ready");

    // The consumer never drains, so the full grace window retires it.
    tokio::time::sleep(STREAM_STALL_GRACE * 2).await;

    // Everything queued before the retire — inbox AND spill — is still
    // delivered, then the abort surfaces.
    let mut drained = vec![0u8; frames];
    slow.read_exact(&mut drained)
        .await
        .expect("data queued before the retire must still be delivered");
    assert!(drained.iter().all(|b| *b == b'x'));

    let error = slow.read(&mut [0u8; 1]).await.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::ConnectionAborted);
    assert!(!session.is_dead());
}

/// Regression (PR #491 review): the receive cap must not abort a
/// healthy, actively reading stream. In one buffered burst the reader
/// task processes frames back-to-back without ever yielding to the
/// consumer, so on a current-thread runtime sixteen 32 KiB frames push
/// `unread` past the cap while a consumer that is continuously
/// reading has simply not been scheduled. The arrival-time retire
/// returned `ConnectionAborted` after exactly 131072 bytes; the same
/// probe must now deliver the whole burst with a clean EOF.
#[tokio::test(flavor = "current_thread")]
async fn buffered_burst_does_not_abort_a_reading_consumer() {
    const FRAMES: usize = 16;
    let total = FRAMES * MAX_FRAME_SIZE;
    let (client_io, mut server_io) = tokio::io::duplex(1024 * 1024);
    let session = Arc::new(Session::client(client_io).unwrap());
    let mut stream = session.open_stream().await.unwrap();

    // One buffered burst, fully written before the consumer task ever
    // gets to poll: 16 PSH frames + FIN. The 1 MiB duplex absorbs it
    // without yielding, which is exactly the harshest scheduling gap.
    let big = vec![0xa5u8; MAX_FRAME_SIZE];
    let mut wire = Vec::with_capacity((MAX_FRAME_SIZE + FRAME_HEADER_LEN) * FRAMES);
    for _ in 0..FRAMES {
        wire.extend_from_slice(&encode_frame(CMD_PSH, stream.id, &big));
    }
    wire.extend_from_slice(&encode_frame(CMD_FIN, stream.id, &[]));
    server_io.write_all(&wire).await.unwrap();

    // The consumer reads continuously; only the reader parking lets it
    // run, which is the exact interleaving that used to be misread as
    // a stall.
    let mut received = Vec::new();
    stream.read_to_end(&mut received).await.unwrap();
    assert_eq!(received.len(), total);
    assert!(received.iter().all(|b| *b == 0xa5));
    assert!(!session.is_dead());
}

/// A consumer that crosses its share but drains back under it inside
/// the grace window is a burst, not a stall: the watchdog must disarm
/// and leave the stream fully usable.
#[tokio::test(start_paused = true)]
async fn over_share_recovered_within_the_grace_window_survives() {
    let frames_over_cap = MAX_STREAM_BUFFER / MAX_FRAME_SIZE + 1;
    let hog_bytes = frames_over_cap * MAX_FRAME_SIZE;
    let (client_io, mut server_io) = tokio::io::duplex(2 * MAX_STREAM_BUFFER);
    let session = Arc::new(Session::client(client_io).unwrap());
    let mut stream = session.open_stream().await.unwrap();

    let big = vec![0x5au8; MAX_FRAME_SIZE];
    let mut wire = Vec::new();
    for _ in 0..frames_over_cap {
        wire.extend_from_slice(&encode_frame(CMD_PSH, stream.id, &big));
    }
    server_io.write_all(&wire).await.unwrap();

    // Halfway through the grace window the consumer drains everything.
    tokio::time::sleep(STREAM_STALL_GRACE / 2).await;
    let mut buffered = vec![0u8; hog_bytes];
    stream.read_exact(&mut buffered).await.unwrap();
    assert!(buffered.iter().all(|b| *b == 0x5a));

    // Past the original fire time the watchdog finds the stream back
    // under its share and disarms instead of retiring.
    tokio::time::sleep(STREAM_STALL_GRACE).await;

    // The stream must still be usable end-to-end.
    server_io
        .write_all(&encode_frame(CMD_PSH, stream.id, b"still alive"))
        .await
        .unwrap();
    let mut alive = [0u8; 11];
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        stream.read_exact(&mut alive),
    )
    .await
    .expect("a recovered stream must stay readable")
    .unwrap();
    assert_eq!(&alive, b"still alive");
    assert!(!session.is_dead());
}

/// The frame-count last resort: a flood of tiny frames that the byte
/// cap can never catch (payload too small to cross the share) parks
/// more than inbox+spill frames against a consumer that has drained
/// nothing, and the stream is retired synchronously so queue-overhead
/// memory stays bounded. Ordinary traffic crosses the grace-gated byte
/// cap long before this fires.
#[tokio::test(start_paused = true)]
async fn tiny_frame_flood_beyond_inbox_and_spill_retires_immediately() {
    // The 4097th frame makes the spill exceed SPILL_QUEUE.
    let frames = STREAM_QUEUE + SPILL_QUEUE + 1;
    let (client_io, mut server_io) = tokio::io::duplex(128 * 1024);
    let session = Arc::new(Session::client(client_io).unwrap());
    let mut flood = session.open_stream().await.unwrap();
    let mut peer = session.open_stream().await.unwrap();

    let mut wire = Vec::new();
    for _ in 0..frames {
        wire.extend_from_slice(&encode_frame(CMD_PSH, flood.id, b"f"));
    }
    wire.extend_from_slice(&encode_frame(CMD_PSH, peer.id, b"ready"));
    server_io.write_all(&wire).await.unwrap();

    // The flood never blocks its peers.
    let mut ready = [0u8; 5];
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        peer.read_exact(&mut ready),
    )
    .await
    .expect("a flood stream must not block its peers")
    .unwrap();
    assert_eq!(&ready, b"ready");

    // Every parked frame is delivered, then the abort surfaces — the
    // last resort fired during the flood, no grace wait involved.
    let mut drained = vec![0u8; frames];
    flood.read_exact(&mut drained).await.unwrap();
    assert!(drained.iter().all(|b| *b == b'f'));
    let error = flood.read(&mut [0u8; 1]).await.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::ConnectionAborted);
    assert!(!session.is_dead());
}

/// Consuming data must return those bytes to the per-stream counter. A
/// counter that only ever grows would retire every long-lived stream once
/// it had transferred one cap's worth in total — the exact defect class
/// the byte cap replaced the frame count to avoid.
#[tokio::test]
async fn draining_a_stream_frees_its_receive_cap() {
    let frames_per_batch = MAX_STREAM_BUFFER / MAX_FRAME_SIZE;
    let (client_io, mut server_io) = tokio::io::duplex(2 * MAX_STREAM_BUFFER);
    let session = Arc::new(Session::client(client_io).unwrap());
    let mut stream = session.open_stream().await.unwrap();

    let big = vec![0x5au8; MAX_FRAME_SIZE];
    // Two consecutive full-cap batches. The second is only deliverable if
    // draining the first gave the bytes back.
    for batch in 0..2 {
        let mut wire = Vec::new();
        for _ in 0..frames_per_batch {
            wire.extend_from_slice(&encode_frame(CMD_PSH, stream.id, &big));
        }
        server_io.write_all(&wire).await.unwrap();

        let mut buf = vec![0u8; MAX_STREAM_BUFFER];
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            stream.read_exact(&mut buf),
        )
        .await
        .unwrap_or_else(|_| panic!("batch {batch} must be delivered in full"))
        .unwrap();
        assert!(buf.iter().all(|b| *b == 0x5a), "batch {batch} corrupted");
    }

    // Still open: the next read parks instead of erroring.
    let next = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        stream.read(&mut [0u8; 1]),
    )
    .await;
    assert!(
        next.is_err(),
        "a fully drained stream must not have been retired"
    );
    assert!(!session.is_dead());
}

/// Upstream `ErrInvalidProtocol`: a control frame (FIN/NOP) carrying a
/// payload kills the session — silently draining it would desync the
/// frame boundary and let a corrupt peer keep the session alive while
/// delivering garbage.
#[tokio::test]
async fn malformed_control_frame_kills_the_session() {
    async fn assert_killed(cmd: u8) {
        let (client_io, mut server_io) = tokio::io::duplex(64 * 1024);
        let session = Arc::new(Session::client(client_io).unwrap());
        let mut stream = session.open_stream().await.unwrap();

        let mut wire = Vec::new();
        wire.extend_from_slice(&encode_frame(cmd, stream.id, b"junk"));
        wire.extend_from_slice(&encode_frame(CMD_PSH, stream.id, b"alive"));
        server_io.write_all(&wire).await.unwrap();

        let mut buf = [0u8; 5];
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            stream.read_exact(&mut buf),
        )
        .await;
        match result {
            // The payload-bearing control frame desyncs the session
            // before the queued data can dispatch — upstream behavior.
            Err(_) | Ok(Err(_)) => {}
            Ok(Ok(n)) => panic!("malformed control frame must kill the session, read {n}"),
        }
        // Give the reader task a moment to land mark_dead.
        for _ in 0..50 {
            if session.is_dead() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(session.is_dead());
    }

    assert_killed(CMD_NOP).await;
    // FIN carrying a payload hits the same upstream arm.
    assert_killed(CMD_FIN).await;
}

/// Regression: when both outbound queues are full, `queue_fin` drops one
/// FIN. It used to call `mark_dead` + `cancel`, and because this path is
/// reachable from `Drop`, merely closing a stream during backpressure
/// killed every healthy stream on the session.
#[tokio::test]
async fn dropped_fin_does_not_kill_the_session() {
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let open = Arc::new(AtomicBool::new(false));
    let waker = Arc::new(std::sync::Mutex::new(None));
    let polled = Arc::new(AtomicBool::new(false));
    let session = Arc::new(
        Session::client(GatedIo {
            inner: client_io,
            open: Arc::clone(&open),
            waker: Arc::clone(&waker),
            polled: Arc::clone(&polled),
        })
        .unwrap(),
    );
    // Open both streams before wedging the writer: opening queues a SYN.
    let victim = session.open_stream().await.unwrap();
    let mut peer = session.open_stream().await.unwrap();
    let victim_id = victim.id;

    // Park the writer on the gate, then fill BOTH outbound queues so
    // `queue_fin` has nowhere left to put the frame.
    let filler = OutMsg::Frame(encode_frame(CMD_PSH, 0xFFFF, &[1]));
    loop {
        while session.writer_tx.capacity() > 0 {
            session.writer_tx.try_send(filler.clone()).unwrap();
        }
        if polled.load(Ordering::SeqCst) {
            break;
        }
        tokio::task::yield_now().await;
    }
    let deferred_filler = encode_frame(CMD_PSH, 0xFFFF, &[1]);
    while session.deferred_fin_tx.capacity() > 0 {
        session
            .deferred_fin_tx
            .try_send(deferred_filler.clone())
            .unwrap();
    }

    drop(victim);
    assert!(
        !session.is_dead(),
        "an unqueueable FIN must cost one stream, not the session"
    );

    open.store(true, Ordering::SeqCst);
    if let Some(w) = waker.lock().unwrap().take() {
        w.wake();
    }
    tokio::time::timeout(std::time::Duration::from_secs(1), peer.write_all(b"ping"))
        .await
        .expect("the session must still accept writes")
        .unwrap();
    assert!(!session.is_dead());

    // The FIN really was dropped rather than slipping into a queue.
    let mut io = server_io;
    let mut hdr = [0u8; FRAME_HEADER_LEN];
    let mut payload = [0u8; 64];
    let mut saw_victim_fin = false;
    let drain = async {
        loop {
            if io.read_exact(&mut hdr).await.is_err() {
                break;
            }
            let Ok((cmd, length, sid)) = Frame::decode_header(&hdr) else {
                break;
            };
            if length > 0 && io.read_exact(&mut payload[..length]).await.is_err() {
                break;
            }
            if cmd == CMD_FIN && sid == victim_id {
                saw_victim_fin = true;
            }
        }
    };
    let _ = tokio::time::timeout(std::time::Duration::from_millis(500), drain).await;
    assert!(
        !saw_victim_fin,
        "both queues were full, so the FIN must have been dropped"
    );
}

#[derive(Clone)]
struct CaptureSink(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for CaptureSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureSink {
    type Writer = CaptureSink;
    fn make_writer(&'a self) -> CaptureSink {
        self.clone()
    }
}

/// Thread-local on purpose: a global subscriber races across parallel test binaries.
fn capture_logs(f: impl FnOnce()) -> String {
    let sink = CaptureSink(Arc::new(std::sync::Mutex::new(Vec::new())));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(sink.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .finish();
    tracing::subscriber::with_default(subscriber, f);
    let captured = sink.0.lock().unwrap();
    String::from_utf8_lossy(&captured).into_owned()
}

/// Blaming a wedge for a dead session points whoever reads the log at a live socket.
#[test]
fn dropped_fin_distinguishes_a_dead_session_from_a_wedge() {
    const WEDGE: &str = "outbound queues full";

    let (writer_tx, writer_rx) = mpsc::channel::<OutMsg>(OUTBOUND_QUEUE);
    let (deferred_tx, deferred_rx) = mpsc::channel::<Bytes>(DEFERRED_FIN_QUEUE);
    drop(writer_rx);
    drop(deferred_rx);
    let dead = capture_logs(|| queue_fin(&writer_tx, &deferred_tx, 7));
    assert!(
        !dead.contains(WEDGE),
        "a dead session must not be reported as backpressure: {dead:?}"
    );

    let (writer_tx, _writer_rx) = mpsc::channel::<OutMsg>(OUTBOUND_QUEUE);
    let (deferred_tx, _deferred_rx) = mpsc::channel::<Bytes>(DEFERRED_FIN_QUEUE);
    for _ in 0..OUTBOUND_QUEUE {
        writer_tx
            .try_send(OutMsg::Frame(Bytes::from_static(b"x")))
            .expect("the writer queue has room for this filler");
    }
    for _ in 0..DEFERRED_FIN_QUEUE {
        deferred_tx
            .try_send(Bytes::from_static(b"x"))
            .expect("the deferred queue has room for this filler");
    }
    let wedged = capture_logs(|| queue_fin(&writer_tx, &deferred_tx, 7));
    assert!(
        wedged.contains(WEDGE),
        "a genuinely wedged writer must still be reported: {wedged:?}"
    );
}

#[tokio::test]
async fn large_write_splits_into_max_frame_chunks() {
    let (client_io, server_io) = tokio::io::duplex(1 << 20);
    let _server = run_echo_server(server_io).await;
    let session = Arc::new(Session::client(client_io).unwrap());
    let mut stream = session.open_stream().await.unwrap();

    // 2.5 × MaxFrameSize — must arrive as 3 PSH frames and echo back
    // byte-identical.
    let payload = vec![0x5au8; MAX_FRAME_SIZE * 2 + MAX_FRAME_SIZE / 2];
    stream.write_all(&payload).await.unwrap();
    let mut resp = vec![0u8; payload.len()];
    stream.read_exact(&mut resp).await.unwrap();
    assert_eq!(resp, payload);
}

/// A zero-length PSH frame carries no payload (flush / ack signal):
/// it must be skipped, not delivered as an empty read, which consumers
/// (the tunnel relay) treat as EOF.
#[tokio::test]
async fn zero_length_psh_frame_does_not_read_as_eof() {
    let (client_io, mut server_io) = tokio::io::duplex(64 * 1024);
    let session = Arc::new(Session::client(client_io).unwrap());
    let mut stream = session.open_stream().await.unwrap();
    let server = tokio::spawn(async move {
        let mut hdr = [0u8; FRAME_HEADER_LEN];
        server_io.read_exact(&mut hdr).await.unwrap(); // client SYN
        let stream_id = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
        server_io
            .write_all(&encode_frame(CMD_PSH, stream_id, &[]))
            .await
            .unwrap();
        server_io
            .write_all(&encode_frame(CMD_PSH, stream_id, b"ok"))
            .await
            .unwrap();
    });
    stream.write_all(b"x").await.unwrap();
    let mut buf = [0u8; 4];
    let n = tokio::time::timeout(std::time::Duration::from_secs(1), stream.read(&mut buf))
        .await
        .expect("read must not hang")
        .expect("zero-length PSH must not read as EOF");
    assert_eq!(n, 2);
    assert_eq!(&buf[..2], b"ok");
    drop(stream);
    drop(session);
    server.await.unwrap();
}

#[tokio::test]
async fn half_close_sends_fin_and_eofs_peer() {
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let _server = run_echo_server(server_io).await;
    let session = Arc::new(Session::client(client_io).unwrap());
    let mut stream = session.open_stream().await.unwrap();

    stream.write_all(b"ping").await.unwrap();
    stream.shutdown().await.unwrap();
    let mut resp = Vec::new();
    stream.read_to_end(&mut resp).await.unwrap();
    assert_eq!(&resp[..], b"ping");
}

#[tokio::test]
async fn physical_eof_is_reported_as_an_error() {
    let (client_io, mut server_io) = tokio::io::duplex(64 * 1024);
    let session = Arc::new(Session::client(client_io).unwrap());
    let mut stream = session.open_stream().await.unwrap();
    let server = tokio::spawn(async move {
        let mut syn = [0u8; FRAME_HEADER_LEN];
        server_io.read_exact(&mut syn).await.unwrap();
    });
    server.await.unwrap();

    let mut buf = [0u8; 1];
    let error = tokio::time::timeout(std::time::Duration::from_secs(1), stream.read(&mut buf))
        .await
        .expect("physical EOF must wake the stream")
        .expect_err("physical EOF must not look like a clean FIN");
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
}

#[tokio::test]
async fn poll_flush_round_trips_through_writer() {
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(async move {
        let mut io = server_io;
        let mut buf = [0u8; 64];
        let _ = io.read(&mut buf).await;
    });
    let session = Arc::new(Session::client(client_io).unwrap());
    let mut stream = session.open_stream().await.unwrap();
    stream.write_all(b"abc").await.unwrap();
    // The flush request travels the writer channel behind the data
    // frames and is acknowledged only after the transport flushed.
    tokio::time::timeout(std::time::Duration::from_secs(1), stream.flush())
        .await
        .expect("flush must complete")
        .unwrap();
    drop(stream);
    drop(session);
    server.await.unwrap();
}

/// IO with independent write and flush gates: the writer can be
/// parked inside `poll_flush` while later writes are queued, which is
/// the exact interleaving that exposes a flush-contract bug.
struct FlushGatedIo {
    inner: tokio::io::DuplexStream,
    write_open: Arc<AtomicBool>,
    flush_open: Arc<AtomicBool>,
    write_waker: Arc<std::sync::Mutex<Option<std::task::Waker>>>,
    flush_waker: Arc<std::sync::Mutex<Option<std::task::Waker>>>,
    write_polled: Arc<AtomicBool>,
    flush_polled: Arc<AtomicBool>,
}

impl AsyncRead for FlushGatedIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for FlushGatedIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.write_open.load(Ordering::SeqCst) {
            return Pin::new(&mut this.inner).poll_write(cx, buf);
        }
        this.write_polled.store(true, Ordering::SeqCst);
        *this.write_waker.lock().unwrap() = Some(cx.waker().clone());
        Poll::Pending
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.flush_open.load(Ordering::SeqCst) {
            return Pin::new(&mut this.inner).poll_flush(cx);
        }
        this.flush_polled.store(true, Ordering::SeqCst);
        *this.flush_waker.lock().unwrap() = Some(cx.waker().clone());
        Poll::Pending
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// Regression: a second flush issued while a first flush is still
/// pending must cover data written after the first flush request.
/// Current code returns the first flush's ack and silently leaves the
/// later data unflushed.
#[tokio::test]
async fn second_flush_must_cover_data_written_while_first_flush_pending() {
    let (client_io, mut server_io) = tokio::io::duplex(64 * 1024);
    let write_open = Arc::new(AtomicBool::new(true));
    let flush_open = Arc::new(AtomicBool::new(false));
    let write_waker = Arc::new(std::sync::Mutex::new(None));
    let flush_waker = Arc::new(std::sync::Mutex::new(None));
    let write_polled = Arc::new(AtomicBool::new(false));
    let flush_polled = Arc::new(AtomicBool::new(false));
    let session = Arc::new(
        Session::client(FlushGatedIo {
            inner: client_io,
            write_open: Arc::clone(&write_open),
            flush_open: Arc::clone(&flush_open),
            write_waker: Arc::clone(&write_waker),
            flush_waker: Arc::clone(&flush_waker),
            write_polled: Arc::clone(&write_polled),
            flush_polled: Arc::clone(&flush_polled),
        })
        .unwrap(),
    );
    let mut stream = session.open_stream().await.unwrap();

    // D1 is written and the first flush request is queued behind it.
    stream.write_all(b"abc").await.unwrap();
    let waker = futures::task::noop_waker();
    let mut cx = std::task::Context::from_waker(&waker);
    let first_poll = Pin::new(&mut stream).poll_flush(&mut cx);
    assert!(first_poll.is_pending(), "first flush must be pending");

    // Wait until the writer is parked inside the transport flush.
    for _ in 0..100 {
        if flush_polled.load(Ordering::SeqCst) {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        flush_polled.load(Ordering::SeqCst),
        "writer must be parked in poll_flush"
    );

    // D2 is queued AFTER the first flush request.
    stream.write_all(b"def").await.unwrap();

    // Before releasing the first flush, close the write gate so the
    // writer will block when it later tries to write D2.
    write_open.store(false, Ordering::SeqCst);

    // Release the first flush.
    flush_open.store(true, Ordering::SeqCst);
    if let Some(w) = flush_waker.lock().unwrap().take() {
        w.wake();
    }

    // The second flush must NOT complete while D2 is still unflushed:
    // the first flush's ack only covers D1.
    let waker = futures::task::noop_waker();
    let mut cx = std::task::Context::from_waker(&waker);
    let second_poll = Pin::new(&mut stream).poll_flush(&mut cx);
    assert!(
        second_poll.is_pending(),
        "second flush must stay pending until D2 is flushed"
    );

    // Release the write gate: the writer drains D2, and only then may
    // the second flush complete.
    write_open.store(true, Ordering::SeqCst);
    if let Some(w) = write_waker.lock().unwrap().take() {
        w.wake();
    }
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        std::future::poll_fn(|cx| Pin::new(&mut stream).poll_flush(cx)),
    )
    .await
    .expect("second flush must not hang")
    .expect("second flush must succeed");

    // The transport must have received both D1 and D2.
    let mut hdr = [0u8; FRAME_HEADER_LEN];
    server_io.read_exact(&mut hdr).await.unwrap();
    let (cmd, length, _sid) = Frame::decode_header(&hdr).unwrap();
    assert_eq!(cmd, CMD_SYN, "first frame must be the stream SYN");
    assert_eq!(length, 0);
    server_io.read_exact(&mut hdr).await.unwrap();
    let (cmd, length, _sid) = Frame::decode_header(&hdr).unwrap();
    assert_eq!(cmd, CMD_PSH, "second frame must be D1");
    let mut payload = vec![0u8; length];
    server_io.read_exact(&mut payload).await.unwrap();
    assert_eq!(&payload, b"abc");
    server_io.read_exact(&mut hdr).await.unwrap();
    let (cmd, length, _sid) = Frame::decode_header(&hdr).unwrap();
    assert_eq!(cmd, CMD_PSH, "third frame must be D2");
    let mut payload = vec![0u8; length];
    server_io.read_exact(&mut payload).await.unwrap();
    assert_eq!(&payload, b"def");

    // Cleanup: release the write gate and drain.
    write_open.store(true, Ordering::SeqCst);
    if let Some(w) = write_waker.lock().unwrap().take() {
        w.wake();
    }
    drop(stream);
    drop(session);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        let mut buf = [0u8; 64];
        loop {
            match server_io.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
        }
    })
    .await;
}

/// IO whose write half stays Pending until the test releases it —
/// parks the session writer deterministically so the bounded
/// outbound channel can be filled (a plain small duplex cannot: the
/// writer drains it too fast, and duplex(0) reads EOF instantly).
/// `polled` records that the write half parked, so tests can top
/// the channel up AFTER the writer stopped consuming; the stored
/// waker lets the test wake the parked write on release.
struct GatedIo {
    inner: tokio::io::DuplexStream,
    open: Arc<AtomicBool>,
    waker: Arc<std::sync::Mutex<Option<std::task::Waker>>>,
    polled: Arc<AtomicBool>,
}

impl AsyncRead for GatedIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for GatedIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.open.load(Ordering::SeqCst) {
            return Pin::new(&mut this.inner).poll_write(cx, buf);
        }
        this.polled.store(true, Ordering::SeqCst);
        *this.waker.lock().unwrap() = Some(cx.waker().clone());
        Poll::Pending
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// Regression: an open cancelled while the outbound channel is full
/// must not leave its stream-map entry behind.
#[tokio::test]
async fn cancelled_open_removes_stream_map_entry() {
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let open = Arc::new(AtomicBool::new(false));
    let waker = Arc::new(std::sync::Mutex::new(None));
    let polled = Arc::new(AtomicBool::new(false));
    let session = Arc::new(
        Session::client(GatedIo {
            inner: client_io,
            open: Arc::clone(&open),
            waker: Arc::clone(&waker),
            polled: Arc::clone(&polled),
        })
        .unwrap(),
    );
    // Park the writer on a first frame (gate closed), then keep the
    // queue topped up: the parked write consumed one slot that must
    // be refilled for the SYN send to stay Pending.
    let filler = OutMsg::Frame(encode_frame(CMD_PSH, 0xFFFF, &[1]));
    session.writer_tx.try_send(filler.clone()).unwrap();
    while session.writer_tx.capacity() > 0 {
        session.writer_tx.try_send(filler.clone()).unwrap();
    }
    loop {
        while session.writer_tx.capacity() > 0 {
            session.writer_tx.try_send(filler.clone()).unwrap();
        }
        if polled.load(Ordering::SeqCst) {
            break;
        }
        tokio::task::yield_now().await;
    }
    let opened =
        tokio::time::timeout(std::time::Duration::from_millis(50), session.open_stream()).await;
    assert!(opened.is_err(), "open must wait for outbound capacity");
    // The cancel guard removes the entry on drop (synchronous for
    // smux, but poll anyway — cheap and robust if that changes).
    for _ in 0..50 {
        if session.state.streams.lock().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        session.state.streams.lock().is_empty(),
        "the cancelled open must not leave its stream-map entry"
    );
    // Release the parked write and drain so the session can shut
    // down cleanly.
    open.store(true, Ordering::SeqCst);
    if let Some(w) = waker.lock().unwrap().take() {
        w.wake();
    }
    let drain = tokio::spawn(async move {
        let mut io = server_io;
        let mut buf = [0u8; 64];
        loop {
            match io.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
        }
    });
    drop(session);
    drain.await.unwrap();
}

/// Regression: dropping a stream while the outbound channel is full
/// must not silently lose the FIN frame.  `best_effort_fin` uses
/// `try_send`; when the channel is full the FIN is dropped and the
/// server never learns the stream closed.
#[tokio::test]
async fn dropped_stream_under_backpressure_still_sends_fin() {
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let open = Arc::new(AtomicBool::new(false));
    let waker = Arc::new(std::sync::Mutex::new(None));
    let polled = Arc::new(AtomicBool::new(false));
    let session = Arc::new(
        Session::client(GatedIo {
            inner: client_io,
            open: Arc::clone(&open),
            waker: Arc::clone(&waker),
            polled: Arc::clone(&polled),
        })
        .unwrap(),
    );
    // Open a stream first: its SYN is queued and the writer parks on
    // it (gate closed).
    let stream = session.open_stream().await.unwrap();
    let sid = stream.id;
    // Keep the channel topped up so the FIN try_send must fail.
    let filler = OutMsg::Frame(encode_frame(CMD_PSH, 0xFFFF, &[1]));
    while session.writer_tx.capacity() > 0 {
        session.writer_tx.try_send(filler.clone()).unwrap();
    }
    loop {
        while session.writer_tx.capacity() > 0 {
            session.writer_tx.try_send(filler.clone()).unwrap();
        }
        if polled.load(Ordering::SeqCst) {
            break;
        }
        tokio::task::yield_now().await;
    }
    // Drop the stream while the channel is full: try_send(FIN) fails.
    drop(stream);
    // Release the writer and collect every frame the server sees.
    open.store(true, Ordering::SeqCst);
    if let Some(w) = waker.lock().unwrap().take() {
        w.wake();
    }
    let mut seen_fin_for_sid = false;
    let mut io = server_io;
    let mut hdr = [0u8; FRAME_HEADER_LEN];
    let mut payload = [0u8; 64];
    let drain = async {
        loop {
            if io.read_exact(&mut hdr).await.is_err() {
                break;
            }
            let Ok((cmd, length, frame_sid)) = Frame::decode_header(&hdr) else {
                break;
            };
            if length > 0 && io.read_exact(&mut payload[..length]).await.is_err() {
                break;
            }
            if cmd == CMD_FIN && frame_sid == sid {
                seen_fin_for_sid = true;
            }
        }
    };
    // The writer drains the channel and then idles; EOF never comes
    // while the session is alive, so bound the observation window.
    let _ = tokio::time::timeout(std::time::Duration::from_millis(500), drain).await;
    assert!(
        seen_fin_for_sid,
        "server must receive FIN for the dropped stream even under backpressure"
    );
    drop(session);
}

#[tokio::test]
async fn outbound_queue_applies_backpressure() {
    let (client_io, _server_io) = tokio::io::duplex(1);
    let session = Arc::new(Session::client(client_io).unwrap());
    let mut stream = session.open_stream().await.unwrap();
    let payload = vec![0x5a; MAX_FRAME_SIZE * (OUTBOUND_QUEUE + 2)];

    let result = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        stream.write_all(&payload),
    )
    .await;
    assert!(
        result.is_err(),
        "a blocked physical writer must backpressure streams"
    );
}

struct DropTrackedIo {
    inner: tokio::io::DuplexStream,
    dropped: Arc<AtomicBool>,
}

impl Drop for DropTrackedIo {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

impl AsyncRead for DropTrackedIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for DropTrackedIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[tokio::test]
async fn dropping_idle_session_releases_physical_io() {
    let (client_io, _server_io) = tokio::io::duplex(64 * 1024);
    let dropped = Arc::new(AtomicBool::new(false));
    let session = Session::client(DropTrackedIo {
        inner: client_io,
        dropped: Arc::clone(&dropped),
    })
    .unwrap();
    drop(session);

    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while !dropped.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("dropping an idle session must stop its tasks and release the fd");
}

/// `client_with_keepalive` must emit a NOP (cmd=3, sid=0) every
/// interval so a kcptun session keeps its NAT mapping warm. Real time,
/// not `start_paused` — the silence check compares `std::time`
/// timestamps, which mocked time does not advance.
#[tokio::test]
async fn keepalive_emits_nop_frames() {
    let (client_io, mut server_io) = tokio::io::duplex(64 * 1024);
    let _session = Session::client_with_keepalive(
        client_io,
        MAX_STREAM_BUFFER,
        std::time::Duration::from_millis(20),
        std::time::Duration::from_millis(60),
    )
    .unwrap();

    // Two NOPs prove the ticker repeats; anything else on the wire is
    // a regression (no streams were ever opened).
    let mut header = [0u8; 8];
    for _ in 0..2 {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            server_io.read_exact(&mut header),
        )
        .await
        .expect("a NOP must arrive within the keepalive interval")
        .unwrap();
        assert_eq!(header[0], 1, "smux v1 version byte");
        assert_eq!(header[1], CMD_NOP, "keepalive frame must be a NOP");
        assert_eq!(&header[4..8], &0u32.to_le_bytes(), "NOPs are sid 0");
    }
}

/// A write side that buffers everything until `poll_flush` — models
/// `tokio_snappy::SnappyIO`'s frame accumulator so this test proves the
/// writer task flushes queued frames (keepalives, FINs) out without
/// waiting for an unrelated write to trigger it.
struct BufferedIo {
    inner: tokio::io::DuplexStream,
    pending: Vec<u8>,
}

impl AsyncRead for BufferedIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for BufferedIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.pending.extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.as_mut().get_mut();
        while !this.pending.is_empty() {
            let n = match Pin::new(&mut this.inner).poll_write(cx, &this.pending) {
                Poll::Ready(Ok(0)) => {
                    return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
                }
                Poll::Ready(Ok(n)) => n,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            };
            this.pending.drain(..n);
        }
        Pin::new(&mut this.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Regression for the kcptun keepalive stall: NOPs queued while the
/// write queue is idle must flush through a buffering transport —
/// upstream's compStream flushes per write for exactly this reason.
#[tokio::test]
async fn keepalive_nops_flush_through_buffered_transport() {
    let (client_io, mut server_io) = tokio::io::duplex(64 * 1024);
    let buffered = BufferedIo {
        inner: client_io,
        pending: Vec::new(),
    };
    let _session = Session::client_with_keepalive(
        buffered,
        MAX_STREAM_BUFFER,
        std::time::Duration::from_millis(20),
        std::time::Duration::from_millis(5000),
    )
    .unwrap();

    let mut header = [0u8; 8];
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        server_io.read_exact(&mut header),
    )
    .await
    .expect("a NOP must flush out of a buffered transport")
    .unwrap();
    assert_eq!(header[1], CMD_NOP, "keepalive frame must be a NOP");
}

/// The kcptun dead-link detector: with no inbound frame for
/// `timeout` the keepalive task must mark the session dead and
/// release the physical io.
#[tokio::test]
async fn silent_peer_declares_session_dead() {
    let (client_io, _server_io) = tokio::io::duplex(64 * 1024);
    let session = Session::client_with_keepalive(
        client_io,
        MAX_STREAM_BUFFER,
        std::time::Duration::from_millis(20),
        std::time::Duration::from_millis(60),
    )
    .unwrap();
    assert!(!session.is_dead());

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !session.is_dead() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("a silent peer past the keepalive timeout must kill the session");
}
