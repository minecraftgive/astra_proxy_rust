use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::Mutex as StdMutex;
use std::task::Poll as TaskPoll;
use tokio::io::DuplexStream;

/// Minimal AsyncRead+AsyncWrite+ProxyConn newtype over a duplex half.
struct TestConn(DuplexStream);

impl ProxyConn for TestConn {}

impl AsyncRead for TestConn {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> TaskPoll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
    }
}

impl AsyncWrite for TestConn {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> TaskPoll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> TaskPoll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> TaskPoll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }
}

/// (session id, network, destination) of an observed New frame.
type NewFrameLog = (u16, u8, Option<(Dest, u16)>);

/// What the mock server observed, for assertions.
#[derive(Default)]
struct ServerLog {
    new_frames: Vec<NewFrameLog>,
    end_frames: Vec<u16>,
    data_frames: usize,
    keepalives: usize,
}

/// Minimal Mux.Cool server: parses frames with the production decoder,
/// echoes every data frame back on the same session id (UDP frames keep
/// their destination), and records New/End frames.  prefix bytes are
/// written first (e.g. a VLESS response header for integration tests).
/// With `echo_end` false the server stays silent on End frames, matching
/// sing-box's server (sing-vmess/mux.go), which never answers them.
async fn run_mock_server(
    mut io: DuplexStream,
    log: Arc<StdMutex<ServerLog>>,
    prefix: Vec<u8>,
    echo_end: bool,
) {
    if !prefix.is_empty() {
        io.write_all(&prefix).await.unwrap();
    }
    loop {
        let Ok(meta_len) = read_u16(&mut io).await else {
            return;
        };
        let mut meta = vec![0u8; meta_len as usize];
        if io.read_exact(&mut meta).await.is_err() {
            return;
        }
        let frame = decode_meta(&meta).unwrap();
        match frame.status {
            STATUS_NEW => {
                log.lock()
                    .unwrap()
                    .new_frames
                    .push((frame.session_id, meta[4], frame.dest));
            }
            STATUS_END => {
                log.lock().unwrap().end_frames.push(frame.session_id);
                if echo_end {
                    // Mirror xray-core's server: it answers a client End
                    // with an End on the same session id.
                    if io
                        .write_all(&encode_end_frame(frame.session_id, false))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
            STATUS_KEEPALIVE => {
                log.lock().unwrap().keepalives += 1;
            }
            STATUS_KEEP => {
                if frame.option & OPTION_DATA == 0 {
                    continue;
                }
                let data_len = read_u16(&mut io).await.unwrap();
                let mut data = vec![0u8; data_len as usize];
                io.read_exact(&mut data).await.unwrap();
                log.lock().unwrap().data_frames += 1;
                let dest = frame.dest.as_ref().map(|(d, p)| (d.host_string(), *p));
                let echo = encode_data_frame(
                    frame.session_id,
                    &data,
                    dest.as_ref().map(|(h, p)| (h.as_str(), *p)),
                )
                .unwrap();
                if io.write_all(&echo).await.is_err() {
                    return;
                }
            }
            _ => {}
        }
    }
}

/// Run a client session + mock server over a duplex pair.
async fn session_pair(prefix: Vec<u8>) -> (Arc<MuxCoolSession>, Arc<StdMutex<ServerLog>>) {
    session_pair_with(prefix, true).await
}

/// Like [`session_pair`] but the server can stay silent on End frames.
async fn session_pair_with(
    prefix: Vec<u8>,
    echo_end: bool,
) -> (Arc<MuxCoolSession>, Arc<StdMutex<ServerLog>>) {
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let log = Arc::new(StdMutex::new(ServerLog::default()));
    tokio::spawn(run_mock_server(
        server_io,
        Arc::clone(&log),
        prefix,
        echo_end,
    ));
    let session = MuxCoolSession::client(Box::new(TestConn(client_io)))
        .await
        .unwrap();
    (session, log)
}

/// Poll a predicate against the server log until it holds or the
/// timeout expires.
async fn wait_for(log: &Arc<StdMutex<ServerLog>>, what: &str, pred: impl Fn(&ServerLog) -> bool) {
    for _ in 0..100 {
        if pred(&log.lock().unwrap()) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("mock server never observed: {what}");
}

// --- Codec byte-level assertions -----------------------------------------

#[test]
fn new_frame_ipv4_matches_xray_layout() {
    let frame = encode_new_frame(1, false, "10.0.2.2", 18081).unwrap();
    // meta_len = sid(2)+status(1)+option(1)+network(1)+port(2)+atype(1)+ipv4(4)
    assert_eq!(
        &frame[..],
        &[
            0x00, 0x0C, // meta_len = 12
            0x00, 0x01, // sid = 1
            0x01, // status New
            0x00, // option none
            0x01, // network TCP
            0x46, 0xA1, // port 18081
            0x01, 10, 0, 2, 2, // atype IPv4 + bytes
        ][..]
    );
}

#[test]
fn new_frame_domain_layout() {
    let frame = encode_new_frame(5, false, "example.com", 80).unwrap();
    // meta_len = 4 + 1 + 2 + 1 + 1 + 11 = 20
    assert_eq!(u16::from_be_bytes([frame[0], frame[1]]), 20);
    assert_eq!(u16::from_be_bytes([frame[2], frame[3]]), 5);
    assert_eq!(frame[4], STATUS_NEW);
    assert_eq!(frame[6], NETWORK_TCP);
    assert_eq!(u16::from_be_bytes([frame[7], frame[8]]), 80);
    assert_eq!(frame[9], ATYPE_DOMAIN);
    assert_eq!(frame[10] as usize, "example.com".len());
    assert_eq!(&frame[11..], b"example.com");
}

#[test]
fn new_frame_udp_marks_network_udp() {
    let frame = encode_new_frame(3, true, "8.8.8.8", 53).unwrap();
    assert_eq!(frame[4], STATUS_NEW);
    assert_eq!(frame[6], NETWORK_UDP);
}

#[test]
fn new_frame_ipv6_layout_and_decode() {
    let frame = encode_new_frame(2, false, "2001:db8::1", 443).unwrap();
    // meta_len = 4 + network(1) + port(2) + atype(1) + ipv6(16) = 24
    assert_eq!(u16::from_be_bytes([frame[0], frame[1]]), 24);
    assert_eq!(frame[4], STATUS_NEW);
    assert_eq!(frame[6], NETWORK_TCP);
    assert_eq!(u16::from_be_bytes([frame[7], frame[8]]), 443);
    assert_eq!(frame[9], ATYPE_IPV6);
    let ip: std::net::Ipv6Addr = "2001:db8::1".parse().unwrap();
    assert_eq!(&frame[10..26], &ip.octets());
    let decoded = decode_meta(&frame[2..]).unwrap();
    let (dest, port) = decoded.dest.expect("New frame must carry a destination");
    assert_eq!(dest, Dest::Ip("2001:db8::1".parse().unwrap()));
    assert_eq!(port, 443);
}

#[test]
fn data_frame_tcp_meta_len_4() {
    let frame = encode_data_frame(7, b"hello", None).unwrap();
    assert_eq!(
        &frame[..],
        &[
            0x00, 0x04, // meta_len = 4
            0x00, 0x07, // sid = 7
            0x02, // status Keep
            0x01, // option Data
            0x00, 0x05, b'h', b'e', b'l', b'l', b'o', // dataLen + payload
        ][..]
    );
}

#[test]
fn data_frame_udp_carries_destination() {
    let frame = encode_data_frame(9, b"hi", Some(("10.0.2.2", 18082))).unwrap();
    // meta_len = 4 + 1 + 2 + 1 + 4 = 12
    assert_eq!(u16::from_be_bytes([frame[0], frame[1]]), 12);
    assert_eq!(frame[6], NETWORK_UDP);
    assert_eq!(u16::from_be_bytes([frame[7], frame[8]]), 18082);
    assert_eq!(frame[9], ATYPE_IPV4);
    assert_eq!(&frame[10..14], &[10, 0, 2, 2]);
    assert_eq!(u16::from_be_bytes([frame[14], frame[15]]), 2);
    assert_eq!(&frame[16..], b"hi");
}

#[test]
fn end_frame_has_no_payload_length() {
    let frame = encode_end_frame(11, false);
    assert_eq!(&frame[..], &[0x00, 0x04, 0x00, 0x0B, 0x03, 0x00][..]);
}

#[test]
fn keepalive_frame_uses_session_id_zero() {
    let frame = encode_keepalive_frame();
    assert_eq!(&frame[..], &[0x00, 0x04, 0x00, 0x00, 0x04, 0x00][..]);
}

#[test]
fn decode_meta_round_trips() {
    for (meta, sid, status, has_dest) in [
        (
            encode_new_frame(1, false, "10.0.2.2", 18081).unwrap(),
            1u16,
            STATUS_NEW,
            true,
        ),
        (
            encode_data_frame(7, b"x", None).unwrap(),
            7u16,
            STATUS_KEEP,
            false,
        ),
        (encode_end_frame(3, false), 3u16, STATUS_END, false),
        (encode_keepalive_frame(), 0u16, STATUS_KEEPALIVE, false),
    ] {
        let decoded = decode_meta(&meta[2..]).unwrap();
        assert_eq!(decoded.session_id, sid);
        assert_eq!(decoded.status, status);
        assert_eq!(decoded.dest.is_some(), has_dest, "meta={meta:?}");
    }
}

#[test]
fn decode_meta_rejects_short_meta() {
    assert!(decode_meta(&[0x00, 0x01, 0x01]).is_err());
}

#[test]
fn decode_meta_rejects_new_without_destination() {
    // New frame with meta_len 4 (no network/address) — must error, not
    // panic the reader task on rest[0].
    assert!(decode_meta(&[0x00, 0x01, 0x01, 0x00]).is_err());
}

/// Deterministic pseudo-random garbage through the decoder: it is the
/// untrusted-input surface, and must error — never panic — on any input.
#[test]
fn decode_meta_never_panics_on_garbage() {
    let mut state = 0x9E37_79B9u32;
    for len in 0..64 {
        for _ in 0..256 {
            let mut meta = Vec::with_capacity(len);
            for _ in 0..len {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                meta.push((state >> 24) as u8);
            }
            let _ = decode_meta(&meta);
        }
    }
}

#[test]
fn decode_meta_rejects_truncated_domain_address() {
    // New frame whose address block stops after the atype byte
    // (port + atype, no length byte) — must error, not panic on the
    // missing b[3] in the domain arm.
    let meta = [
        0x00,
        0x01, // sid 1
        STATUS_NEW,
        0x00, // option none
        NETWORK_TCP,
        0x00,
        0x50,         // port 80
        ATYPE_DOMAIN, // ...and nothing else
    ];
    assert!(decode_meta(&meta).is_err());
}

#[test]
fn decode_meta_rejects_end_frame_with_data_option() {
    // End frames never carry payloads: a Data bit would leave an
    // unconsumed length field desyncing the frame stream.
    let meta = [0x00, 0x01, STATUS_END, OPTION_DATA];
    assert!(decode_meta(&meta).is_err());
}

/// The hot UDP path encodes destinations from raw IP octets; it must
/// produce exactly the put_address (string) layout.
#[test]
fn udp_frame_ip_layouts_match_put_address() {
    let ip = "10.0.2.2".parse().unwrap();
    assert_eq!(
        encode_udp_data_frame(9, b"hi", ip, 18082),
        encode_data_frame(9, b"hi", Some(("10.0.2.2", 18082))).unwrap()
    );
    let ip6 = "2001:db8::1".parse().unwrap();
    assert_eq!(
        encode_udp_data_frame(9, b"hi", ip6, 53),
        encode_data_frame(9, b"hi", Some(("2001:db8::1", 53))).unwrap()
    );
}

#[test]
fn decode_meta_ignores_trailing_meta_bytes() {
    // xray may append optional fullcone/GlobalID metadata after the
    // address - trailing bytes must be ignored, not misparsed.
    let frame = encode_new_frame(1, false, "10.0.2.2", 80).unwrap();
    let mut extended = BytesMut::new();
    extended.extend_from_slice(&frame);
    let meta_len = extended.len() - 2;
    extended[0..2].copy_from_slice(&((meta_len + 8) as u16).to_be_bytes());
    extended.extend_from_slice(&[0xAA; 8]);
    let decoded = decode_meta(&extended[2..]).unwrap();
    assert_eq!(decoded.session_id, 1);
    assert_eq!(decoded.status, STATUS_NEW);
    assert_eq!(decoded.dest.as_ref().unwrap().1, 80);
}

/// Pins the UDP read-path allocation fix (issue #427): `parse_address`
/// must hand back `Dest::Ip` directly (no `to_string()`/reparse round
/// trip), and `dest_to_socket` must build the SocketAddr straight from
/// it. Domain destinations still fall back to the stream's bound addr.
#[test]
fn dest_to_socket_uses_ip_directly_and_falls_back_for_domains() {
    let (dest, port) = parse_address(&[0x1F, 0x90, ATYPE_IPV4, 10, 0, 2, 2]).unwrap();
    assert_eq!(dest, Dest::Ip("10.0.2.2".parse().unwrap()));
    let fallback: SocketAddr = "192.0.2.1:9".parse().unwrap();
    assert_eq!(
        dest_to_socket(&dest, port, fallback),
        "10.0.2.2:8080".parse::<SocketAddr>().unwrap()
    );

    let mut domain_bytes = vec![0x00, 0x50, ATYPE_DOMAIN, 4];
    domain_bytes.extend_from_slice(b"host");
    let (dest, _) = parse_address(&domain_bytes).unwrap();
    assert!(matches!(dest, Dest::Domain(ref b) if b == b"host"));
    assert_eq!(dest_to_socket(&dest, 80, fallback), fallback);
}

// --- Session round trips -------------------------------------------------

/// AsyncRead contract: polling with a zero-capacity buffer yields
/// Ready(Ok(())) per the tokio docs (a Pending here would park
/// `read(&mut [])` forever), and the pending bytes survive intact.
#[tokio::test]
async fn poll_read_with_full_buffer_returns_ready_empty() {
    let (session, _log) = session_pair(vec![]).await;
    let mut stream = session.open_stream("a.example", 80, false).await.unwrap();
    stream.write_all(b"hello").await.unwrap();
    // Consume one byte so the demux channel holds pending data.
    let mut one = [0u8; 1];
    let n = stream.read(&mut one).await.unwrap();
    assert_eq!(n, 1);
    assert_eq!(one[0], b'h');

    let poll = std::future::poll_fn(|cx| {
        let mut empty = [0u8; 0];
        let mut rb = ReadBuf::new(&mut empty);
        Pin::new(&mut stream).poll_read(cx, &mut rb)
    });
    let res = tokio::time::timeout(Duration::from_millis(100), poll).await;
    assert!(
        res.is_ok(),
        "poll_read with a zero-capacity buffer must return Ready(Ok(()))"
    );

    // The pending bytes survive intact.
    let mut rest = [0u8; 8];
    let n = stream.read(&mut rest).await.unwrap();
    assert_eq!(&rest[..n], b"ello");
}

/// IO whose write half stays Pending until the test releases it —
/// parks the driver deterministically so the bounded outbound
/// channel can be filled (a plain small duplex cannot: the driver
/// drains it too fast, and duplex(0) reads EOF instantly).
/// `polled` records that the write half parked, so tests can top
/// the channel up AFTER the driver stopped consuming; the stored
/// waker lets the test wake the parked write on release (a fresh
/// `Notify::notified()` future would drop its registration between
/// polls and miss `notify_waiters`).
struct GatedConn {
    inner: DuplexStream,
    open: Arc<AtomicBool>,
    waker: Arc<StdMutex<Option<std::task::Waker>>>,
    polled: Arc<AtomicBool>,
}

impl ProxyConn for GatedConn {}

impl AsyncRead for GatedConn {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> TaskPoll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for GatedConn {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> TaskPoll<io::Result<usize>> {
        let this = self.get_mut();
        if this.open.load(Ordering::SeqCst) {
            return Pin::new(&mut this.inner).poll_write(cx, buf);
        }
        this.polled.store(true, Ordering::SeqCst);
        *this.waker.lock().unwrap() = Some(cx.waker().clone());
        TaskPoll::Pending
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> TaskPoll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> TaskPoll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// Regression: a shutdown cancelled while the outbound channel is
/// full must not claim the End — the EndGuard re-sends it on drop.
#[tokio::test]
async fn cancelled_shutdown_still_sends_end_on_drop() {
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let log = Arc::new(StdMutex::new(ServerLog::default()));
    tokio::spawn(run_mock_server(server_io, Arc::clone(&log), vec![], true));
    let open = Arc::new(AtomicBool::new(false));
    let waker = Arc::new(StdMutex::new(None));
    let polled = Arc::new(AtomicBool::new(false));
    let session = MuxCoolSession::client(Box::new(GatedConn {
        inner: client_io,
        open: Arc::clone(&open),
        waker: Arc::clone(&waker),
        polled: Arc::clone(&polled),
    }))
    .await
    .unwrap();
    let mut stream = session.open_stream("a.example", 80, false).await.unwrap();
    // The driver drains the New frame and parks writing it (gate
    // closed): keep the channel topped up so the shutdown's reserve
    // must stay Pending.
    // Fillers must be well-formed frames: zero-length frames would
    // desynchronise the mock server's decoder.
    while session.driver_tx.capacity() > 0 {
        session
            .driver_tx
            .try_send(encode_keepalive_frame())
            .unwrap();
    }
    loop {
        while session.driver_tx.capacity() > 0 {
            session
                .driver_tx
                .try_send(encode_keepalive_frame())
                .unwrap();
        }
        if polled.load(Ordering::SeqCst) {
            break;
        }
        tokio::task::yield_now().await;
    }
    // One poll of shutdown stashes the End frame without claiming it
    // (the reserve is Pending); the timeout then cancels the future.
    let poll = std::future::poll_fn(|cx| -> TaskPoll<io::Result<()>> {
        let _ = Pin::new(&mut stream).poll_shutdown(cx);
        TaskPoll::Pending
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(50), poll)
            .await
            .is_err(),
        "shutdown must stay Pending on a full channel"
    );
    drop(stream); // cancelled shutdown: EndGuard must resend the End
                  // Open the gate: the driver writes the queued frames, then the
                  // re-sent End, which the mock server logs.
    open.store(true, Ordering::SeqCst);
    if let Some(w) = waker.lock().unwrap().take() {
        w.wake();
    }
    wait_for(&log, "End frame after cancelled shutdown", |l| {
        !l.end_frames.is_empty()
    })
    .await;
    assert_eq!(log.lock().unwrap().end_frames[0], 1);
}

/// Regression: an open cancelled while the outbound channel is full
/// must not leave its stream-map entry (and consumed sid) behind.
#[tokio::test]
async fn cancelled_open_removes_stream_map_entry() {
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let open = Arc::new(AtomicBool::new(false));
    let waker = Arc::new(StdMutex::new(None));
    let polled = Arc::new(AtomicBool::new(false));
    let session = MuxCoolSession::client(Box::new(GatedConn {
        inner: client_io,
        open: Arc::clone(&open),
        waker: Arc::clone(&waker),
        polled: Arc::clone(&polled),
    }))
    .await
    .unwrap();
    // Park the driver writing a keepalive frame, then keep the queue
    // topped up so the open's send must stay Pending.
    session
        .driver_tx
        .try_send(encode_keepalive_frame())
        .unwrap();
    // Fillers must be well-formed frames: zero-length frames would
    // desynchronise the mock server's decoder.
    while session.driver_tx.capacity() > 0 {
        session
            .driver_tx
            .try_send(encode_keepalive_frame())
            .unwrap();
    }
    loop {
        while session.driver_tx.capacity() > 0 {
            session
                .driver_tx
                .try_send(encode_keepalive_frame())
                .unwrap();
        }
        if polled.load(Ordering::SeqCst) {
            break;
        }
        tokio::task::yield_now().await;
    }
    let opened = tokio::time::timeout(
        Duration::from_millis(50),
        session.open_stream("a.example", 80, false),
    )
    .await;
    assert!(opened.is_err(), "open must wait for outbound capacity");
    // The cancellation guard removes the entry asynchronously — poll
    // for it instead of racing a fixed sleep on a loaded runner.
    for _ in 0..100 {
        if session.streams.lock().await.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        session.streams.lock().await.is_empty(),
        "the cancelled open must not leave its stream-map entry"
    );
    assert_eq!(session.next_sid.load(Ordering::SeqCst), 2);
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

#[tokio::test]
async fn tcp_stream_echo_round_trip() {
    let (session, log) = session_pair(vec![]).await;
    let mut stream = session.open_stream("10.0.2.2", 18081, false).await.unwrap();
    stream.write_all(b"hello-mux").await.unwrap();
    let mut buf = [0u8; 16];
    let n = stream.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"hello-mux");
    stream.shutdown().await.unwrap();
    wait_for(&log, "New frame", |l| !l.new_frames.is_empty()).await;
    wait_for(&log, "End frame", |l| !l.end_frames.is_empty()).await;
    assert_eq!(
        log.lock().unwrap().new_frames[0],
        (
            1,
            NETWORK_TCP,
            Some((Dest::Ip("10.0.2.2".parse().unwrap()), 18081))
        )
    );
    assert_eq!(log.lock().unwrap().end_frames[0], 1);
}

#[tokio::test]
async fn two_streams_demux_by_session_id() {
    let (session, log) = session_pair(vec![]).await;
    let mut a = session.open_stream("a.example", 80, false).await.unwrap();
    let mut b = session.open_stream("b.example", 81, false).await.unwrap();
    a.write_all(b"stream-a").await.unwrap();
    b.write_all(b"stream-b").await.unwrap();
    let mut bufa = [0u8; 16];
    let mut bufb = [0u8; 16];
    let na = a.read(&mut bufa).await.unwrap();
    let nb = b.read(&mut bufb).await.unwrap();
    assert_eq!(&bufa[..na], b"stream-a");
    assert_eq!(&bufb[..nb], b"stream-b");
    assert_eq!(log.lock().unwrap().new_frames.len(), 2);
    // Distinct session ids, assigned in open order from 1.
    assert_eq!(log.lock().unwrap().new_frames[0].0, 1);
    assert_eq!(log.lock().unwrap().new_frames[1].0, 2);
}

#[tokio::test]
async fn end_frame_sent_on_drop() {
    let (session, log) = session_pair(vec![]).await;
    let stream = session.open_stream("a.example", 80, false).await.unwrap();
    drop(stream);
    wait_for(&log, "End frame after drop", |l| !l.end_frames.is_empty()).await;
}

/// The client must emit KeepAlive frames (sid 0) while a session sits
/// idle so NAT bindings stay alive — verified with paused virtual time.
#[tokio::test(start_paused = true)]
async fn client_sends_keepalive_frames_while_idle() {
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let log = Arc::new(StdMutex::new(ServerLog::default()));
    tokio::spawn(run_mock_server(server_io, Arc::clone(&log), vec![], true));
    let session = MuxCoolSession::client(Box::new(TestConn(client_io)))
        .await
        .unwrap();
    let stream = session.open_stream("a.example", 80, false).await.unwrap();
    drop(stream); // zero-stream idle session: keepalive must still run
    for _ in 0..200 {
        if log.lock().unwrap().keepalives > 0 {
            break;
        }
        tokio::time::advance(Duration::from_millis(200)).await;
    }
    assert!(
        log.lock().unwrap().keepalives > 0,
        "expected at least one keepalive frame after {}s of idle",
        KEEPALIVE_INTERVAL.as_secs()
    );
}

#[tokio::test]
async fn server_keepalive_is_ignored() {
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let mut io = server_io;
        // KeepAlive frame first, then wait for the client's New frame
        // before answering.
        io.write_all(&encode_keepalive_frame()).await.unwrap();
        let meta_len = read_u16(&mut io).await.unwrap();
        let mut meta = vec![0u8; meta_len as usize];
        io.read_exact(&mut meta).await.unwrap();
        let sid = u16::from_be_bytes([meta[0], meta[1]]);
        let echo = encode_data_frame(sid, b"still-here", None).unwrap();
        io.write_all(&echo).await.unwrap();
    });
    let session = MuxCoolSession::client(Box::new(TestConn(client_io)))
        .await
        .unwrap();
    let mut stream = session.open_stream("a.example", 80, false).await.unwrap();
    stream.write_all(b"ping").await.unwrap();
    let mut buf = [0u8; 16];
    let n = stream.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"still-here");
}

#[tokio::test]
async fn large_write_is_chunked_at_8k() {
    let (session, log) = session_pair(vec![]).await;
    let mut stream = session.open_stream("a.example", 80, false).await.unwrap();
    let payload = vec![0x5A; 20 * 1024];
    stream.write_all(&payload).await.unwrap();
    let mut received = vec![0u8; payload.len()];
    stream.read_exact(&mut received).await.unwrap();
    assert_eq!(received, payload);
    // 20 KiB at 8 KiB per frame = 3 data frames.
    assert_eq!(log.lock().unwrap().data_frames, 3);
}

#[tokio::test]
async fn keep_error_frame_drains_payload_and_keeps_framing() {
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let mut io = server_io;
        // Consume the two New frames.
        let mut sids = Vec::new();
        for _ in 0..2 {
            let meta_len = read_u16(&mut io).await.unwrap();
            let mut meta = vec![0u8; meta_len as usize];
            io.read_exact(&mut meta).await.unwrap();
            sids.push(u16::from_be_bytes([meta[0], meta[1]]));
        }
        // Error|Data keep frame for stream 1: the payload must be
        // drained (framing), and the stream must see an error.
        let mut err = BytesMut::new();
        err.put_u16(4);
        err.put_u16(sids[0]);
        err.put_u8(STATUS_KEEP);
        err.put_u8(OPTION_DATA | OPTION_ERROR);
        err.put_u16(1);
        err.put_u8(0xAA);
        io.write_all(&err).await.unwrap();
        // ...followed by a normal data frame for stream 2.
        let echo = encode_data_frame(sids[1], b"ok", None).unwrap();
        io.write_all(&echo).await.unwrap();
    });
    let session = MuxCoolSession::client(Box::new(TestConn(client_io)))
        .await
        .unwrap();
    let mut s1 = session.open_stream("a.example", 80, false).await.unwrap();
    let mut s2 = session.open_stream("b.example", 81, false).await.unwrap();
    let mut buf = [0u8; 8];
    let n = s2.read(&mut buf).await.unwrap();
    assert_eq!(
        &buf[..n],
        b"ok",
        "stream 2 framing must survive the error frame"
    );
    let result = s1.read(&mut buf).await;
    assert!(result.is_err(), "stream 1 must surface the remote error");
}

#[tokio::test]
async fn oversized_meta_aborts_session_with_error() {
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let mut io = server_io;
        // Consume the New frame, then send an oversized meta length.
        let meta_len = read_u16(&mut io).await.unwrap();
        let mut meta = vec![0u8; meta_len as usize];
        io.read_exact(&mut meta).await.unwrap();
        let mut bad = BytesMut::with_capacity(2);
        bad.put_u16(MAX_META as u16 + 1);
        io.write_all(&bad).await.unwrap();
    });
    let session = MuxCoolSession::client(Box::new(TestConn(client_io)))
        .await
        .unwrap();
    let mut stream = session.open_stream("a.example", 80, false).await.unwrap();
    stream.write_all(b"x").await.unwrap();
    let mut buf = [0u8; 8];
    let result = stream.read(&mut buf).await;
    assert!(
        result.is_err(),
        "oversized meta must abort the session with an error"
    );
}

#[tokio::test]
async fn session_abort_surfaces_error_on_streams() {
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    // Server sends nothing and dies: the reader errors, streams must
    // surface an error (not a clean EOF) once the session aborts.
    tokio::spawn(async move {
        drop(server_io);
    });
    let session = MuxCoolSession::client(Box::new(TestConn(client_io)))
        .await
        .unwrap();
    let mut stream = session.open_stream("a.example", 80, false).await.unwrap();
    stream.write_all(b"x").await.unwrap();
    let mut buf = [0u8; 8];
    let result = stream.read(&mut buf).await;
    assert!(
        result.is_err(),
        "aborted session must error, got {result:?}"
    );
}

// --- Delivery flow control (no driver blocking) ---------------------------

/// Unit-level: a frame that hits a full queue parks, and the retry
/// delivers it (in order) once the consumer drains.
#[tokio::test]
async fn parked_frame_retries_after_consumer_drains() {
    let (tx, mut rx) = mpsc::channel(2);
    let data = |b: u8| Event::Data {
        bytes: Bytes::copy_from_slice(&[b; 8]),
        dest: None,
    };
    assert!(deliver_now(Some(tx.clone()), data(1)).is_none());
    assert!(deliver_now(Some(tx.clone()), data(2)).is_none());
    let mut parked = Some(deliver_now(Some(tx.clone()), data(3)).expect("third frame parks"));
    retry_parked(&mut parked);
    assert!(
        parked.is_some(),
        "a still-full queue keeps the frame parked"
    );
    let first = rx.recv().await.unwrap();
    assert!(
        matches!(first, Event::Data { bytes, .. } if bytes[0] == 1),
        "FIFO order must survive parking"
    );
    retry_parked(&mut parked);
    assert!(
        parked.is_none(),
        "the retry succeeds once the consumer drains"
    );
    assert!(matches!(rx.recv().await.unwrap(), Event::Data { bytes, .. } if bytes[0] == 2));
    assert!(matches!(rx.recv().await.unwrap(), Event::Data { bytes, .. } if bytes[0] == 3));
}

/// Unit-level: a consumer that drops its stream discards the parked
/// frame (closed channel) instead of wedging the driver.
#[tokio::test]
async fn parked_frame_discarded_when_consumer_dropped() {
    let (tx, rx) = mpsc::channel(1);
    tx.try_send(Event::Eof).unwrap();
    let mut parked = Some(
        deliver_now(Some(tx), Event::Error(io::Error::other("x")))
            .expect("frame on a full queue parks"),
    );
    drop(rx);
    retry_parked(&mut parked);
    assert!(
        parked.is_none(),
        "a closed channel discards the parked frame"
    );
}

#[tokio::test]
async fn data_delivery_survives_cancellation_while_stream_map_is_locked() {
    let (mut client_io, mut server_io) = tokio::io::duplex(64 * 1024);
    server_io
        .write_all(&encode_data_frame(1, b"payload", None).unwrap())
        .await
        .unwrap();

    let streams = Arc::new(Mutex::new(HashMap::new()));
    let (tx, mut rx) = mpsc::channel(1);
    streams.lock().await.insert(1, tx);
    let mut state = ReadState::fresh();
    read_step(&mut client_io, &mut state, &streams)
        .await
        .unwrap();
    read_step(&mut client_io, &mut state, &streams)
        .await
        .unwrap();
    read_step(&mut client_io, &mut state, &streams)
        .await
        .unwrap();
    assert!(matches!(state.phase, ReadPhase::Data));

    let guard = streams.lock().await;
    let cancelled = tokio::time::timeout(
        Duration::from_millis(100),
        read_step(&mut client_io, &mut state, &streams),
    )
    .await;
    assert!(
        cancelled.is_err(),
        "delivery must wait for the stream map lock"
    );
    assert!(matches!(state.phase, ReadPhase::Data));
    assert_eq!(&state.buf[..], b"payload");
    drop(guard);

    assert!(read_step(&mut client_io, &mut state, &streams)
        .await
        .unwrap()
        .is_none());
    let event = rx.recv().await.unwrap();
    assert!(matches!(event, Event::Data { bytes, .. } if bytes.as_ref() == b"payload"));
}

#[tokio::test]
async fn length_prefix_read_survives_cancellation() {
    let (mut client_io, mut server_io) = tokio::io::duplex(64 * 1024);
    server_io.write_all(&[0]).await.unwrap();

    let streams = Arc::new(Mutex::new(HashMap::new()));
    let (tx, mut rx) = mpsc::channel(1);
    streams.lock().await.insert(1, tx);
    let mut state = ReadState::fresh();
    let cancelled = tokio::time::timeout(
        Duration::from_millis(100),
        read_step(&mut client_io, &mut state, &streams),
    )
    .await;
    assert!(
        cancelled.is_err(),
        "the read must wait for the second length byte"
    );
    assert_eq!(state.pos, 1);
    assert!(matches!(state.phase, ReadPhase::MetaLen));

    let frame = encode_data_frame(1, b"x", None).unwrap();
    server_io.write_all(&frame[1..]).await.unwrap();
    for _ in 0..4 {
        read_step(&mut client_io, &mut state, &streams)
            .await
            .unwrap();
    }
    let event = rx.recv().await.unwrap();
    assert!(matches!(event, Event::Data { bytes, .. } if bytes.as_ref() == b"x"));
}

#[tokio::test]
async fn blocked_physical_write_does_not_starve_reads() {
    let (client_io, mut server_io) = tokio::io::duplex(1);
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        server_io
            .write_all(&encode_data_frame(1, b"reply", None).unwrap())
            .await
            .unwrap();
        let _ = done_rx.await;
    });
    let session = MuxCoolSession::client(Box::new(TestConn(client_io)))
        .await
        .unwrap();
    let mut stream = session.open_stream("a.example", 80, false).await.unwrap();
    let mut reply = [0u8; 5];
    tokio::time::timeout(Duration::from_secs(1), stream.read_exact(&mut reply))
        .await
        .expect("read progress must continue while the New frame is blocked")
        .unwrap();
    assert_eq!(&reply, b"reply");
    done_tx.send(()).unwrap();
    server.await.unwrap();
}

/// Regression: a full-duplex bulk transfer must not deadlock.  The
/// consumer writes 1 MiB (128 x 8 KiB frames) before reading while the
/// server echoes every frame: the 32-frame inbound queue fills
/// mid-write.  The driver must PARK the overflow and keep draining
/// outbound (flow control), not block on the stream queue - blocking
/// wedged the writer (waiting for outbound capacity) against the
/// driver (waiting for inbound capacity) forever.
#[tokio::test]
async fn full_duplex_bulk_transfer_does_not_deadlock() {
    // 512 KiB duplex: large enough that the duplex itself never
    // backpressures (both directions absorb the 256 KiB transfer), so
    // the only backpressure under test is the mux queues.  Kept well
    // below 1 MiB to avoid the BytesMut capacity-doubling spikes that
    // can trigger a 16 MiB heap allocation on the Windows debug runner.
    let (client_io, server_io) = tokio::io::duplex(512 * 1024);
    let log = Arc::new(StdMutex::new(ServerLog::default()));
    tokio::spawn(run_mock_server(server_io, Arc::clone(&log), vec![], true));
    let session = MuxCoolSession::client(Box::new(TestConn(client_io)))
        .await
        .unwrap();
    let mut stream = session.open_stream("a.example", 80, false).await.unwrap();
    // 256 KiB = 32 × 8 KiB frames: fills the 32-frame inbound queue
    // exactly once, exercising the driver's park-and-retry path.
    let payload = vec![0x5A; 256 * 1024];
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        stream.write_all(&payload).await.unwrap();
        let mut received = vec![0u8; payload.len()];
        stream.read_exact(&mut received).await.unwrap();
        assert_eq!(received, payload);
    })
    .await;
    assert!(result.is_ok(), "full-duplex bulk transfer deadlocked");
}

/// The End handler removes the map entry before the terminal event
/// parks: a full queue at End time still surfaces EOF and leaves no
/// stale sender behind.
#[tokio::test]
async fn end_after_full_queue_delivers_eof_and_cleans_entry() {
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let mut io = server_io;
        let meta_len = read_u16(&mut io).await.unwrap();
        let mut meta = vec![0u8; meta_len as usize];
        io.read_exact(&mut meta).await.unwrap();
        let sid = u16::from_be_bytes([meta[0], meta[1]]);
        // 32 frames fill the inbound queue; the 33rd parks.
        for _ in 0..(STREAM_QUEUE as u16 + 1) {
            if io
                .write_all(&encode_data_frame(sid, b"x", None).unwrap())
                .await
                .is_err()
            {
                return;
            }
        }
        let _ = io.write_all(&encode_end_frame(sid, false)).await;
        // Keep the session alive so the assertions observe the
        // stream-level teardown, not a session abort.
        let _ = tokio::time::sleep(Duration::from_secs(30)).await;
    });
    let session = MuxCoolSession::client(Box::new(TestConn(client_io)))
        .await
        .unwrap();
    let mut stream = session.open_stream("a.example", 80, false).await.unwrap();
    let mut buf = vec![0u8; STREAM_QUEUE + 1];
    stream.read_exact(&mut buf).await.unwrap();
    assert!(buf.iter().all(|b| *b == b'x'));
    let mut eof = [0u8; 1];
    assert_eq!(
        stream.read(&mut eof).await.unwrap(),
        0,
        "the End frame must surface as EOF after the queued data"
    );
    assert!(
        session.streams.lock().await.is_empty(),
        "the End frame must remove the stream entry"
    );
}

/// Churn regression: opening and dropping many streams must not
/// leave stale entries behind, even against a sing-box-style server
/// that never echoes End frames - the client's own teardown removes
/// each entry, so the map stays empty on a long-lived session.
#[tokio::test]
async fn stream_churn_leaves_no_entries_behind() {
    let (session, _log) = session_pair_with(vec![], false).await;
    for _ in 0..50 {
        let mut stream = session.open_stream("a.example", 80, false).await.unwrap();
        stream.write_all(b"x").await.unwrap();
        drop(stream); // EndGuard sends End; the server answers nothing
    }
    for _ in 0..200 {
        if session.streams.lock().await.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("stream map must drain after churn");
}

/// sing-box's Mux.Cool server answers a client End with silence (it
/// never echoes End frames): the stream's read side must EOF when the
/// local shutdown queues the End, without waiting for an echo that
/// never comes - otherwise every half-closed relay lingers until its
/// own timeout.
#[tokio::test]
async fn shutdown_eofs_read_side_without_server_end_echo() {
    let (session, log) = session_pair_with(vec![], false).await;
    let mut stream = session.open_stream("a.example", 80, false).await.unwrap();
    stream.write_all(b"x").await.unwrap();
    let mut buf = [0u8; 1];
    assert_eq!(stream.read(&mut buf).await.unwrap(), 1);
    let sid = stream.parts.sid;
    stream.shutdown().await.unwrap();
    let mut eof = [0u8; 1];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), stream.read(&mut eof))
            .await
            .expect("read must not wait for a server End echo")
            .unwrap(),
        0,
        "shutdown must EOF the read side locally"
    );
    // The map removal runs detached; give it a tick.
    for _ in 0..100 {
        if session.streams.lock().await.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        session.streams.lock().await.is_empty(),
        "shutdown must remove the stream entry without a server End echo"
    );
    drop(stream); // EndGuard must not re-send: shutdown claimed the End
    wait_for(&log, "End frame after shutdown", |l| {
        l.end_frames.contains(&sid)
    })
    .await;
    // Give a duplicate End a chance to appear, then pin exactly-once
    // (a wait-for-absence can't be polled — a wider window is the
    // only way to shrink the false-pass margin).
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        log.lock()
            .unwrap()
            .end_frames
            .iter()
            .filter(|s| **s == sid)
            .count(),
        1,
        "End must be sent exactly once across shutdown + drop"
    );
}

/// After a local shutdown, inbound data the server already sent (it
/// cannot know about our End yet) is still delivered: the read side
/// drains what is queued before the channel close surfaces as EOF.
/// Mux.Cool End is a full close on the wire (both server
/// implementations kill the stream on receiving it), so nothing can
/// arrive after the server processes the End — but frames already in
/// flight must not be discarded.
#[tokio::test]
async fn shutdown_delivers_buffered_data_then_eofs() {
    let (session, _log) = session_pair_with(vec![], false).await;
    let mut stream = session.open_stream("a.example", 80, false).await.unwrap();
    stream.write_all(b"ab").await.unwrap();
    let mut buf = [0u8; 1];
    assert_eq!(stream.read(&mut buf).await.unwrap(), 1);
    assert_eq!(buf[0], b'a'); // 'b' stays buffered in the stream
    stream.shutdown().await.unwrap();
    let mut rest = [0u8; 1];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), stream.read(&mut rest))
            .await
            .expect("post-End read must not hang")
            .unwrap(),
        1,
        "data the server sent before the End must still be delivered"
    );
    assert_eq!(rest[0], b'b');
    // The map removal closes the inbound channel: EOF follows the
    // buffered data without waiting for a server End echo.
    let mut eof = [0u8; 1];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), stream.read(&mut eof))
            .await
            .expect("EOF must follow without a server End echo")
            .unwrap(),
        0
    );
}

/// The UDP flow's teardown rides the same EndGuard: dropping a
/// PacketConn must remove its map entry even when the server stays
/// silent, so a reader parked on the flow's channel observes the
/// closed channel instead of hanging forever.
#[tokio::test]
async fn packet_conn_drop_removes_map_entry_without_server_end_echo() {
    use crate::mux::client::{MuxSession, SessionKind};
    let (session, _log) = session_pair_with(vec![], false).await;
    let pool_session = Arc::new(MuxSession {
        kind: SessionKind::MuxCool(Arc::clone(&session)),
        streams: std::sync::atomic::AtomicUsize::new(1),
        last_used_ms: meow_common::atomic::AtomicU::new(0),
        generation: 0,
    });
    let parts = session
        .open_stream_parts("a.example", 80, true)
        .await
        .unwrap();
    let conn = PacketConn::new(parts, pool_session, "127.0.0.1:9".parse().unwrap());
    drop(conn);
    for _ in 0..100 {
        if session.streams.lock().await.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("PacketConn drop must remove the stream entry without a server End echo");
}

/// Dropping a stream without an explicit shutdown also removes its map
/// entry even when the server never answers (sing-box behavior), so a
/// long-lived session cannot accumulate entries under churn.
#[tokio::test]
async fn drop_removes_map_entry_without_server_end_echo() {
    let (session, log) = session_pair_with(vec![], false).await;
    let stream = session.open_stream("a.example", 80, false).await.unwrap();
    let sid = stream.parts.sid;
    drop(stream);
    wait_for(&log, "End frame after drop", |l| {
        l.end_frames.contains(&sid)
    })
    .await;
    for _ in 0..100 {
        if session.streams.lock().await.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("drop must remove the stream entry without a server End echo");
}

/// Once the 16-bit session id space is exhausted the session keeps
/// failing - ids never wrap around into a live stream's frames.
#[tokio::test]
async fn sid_space_exhaustion_errors_without_reuse() {
    let (session, _log) = session_pair(vec![]).await;
    session
        .next_sid
        .store(u16::MAX as u32 + 1, Ordering::SeqCst);
    assert!(session.alloc_sid().is_err());
    assert!(
        session.unavailable(),
        "the pool must retire the exhausted session"
    );
    assert!(
        !session.aborted(),
        "retirement must not abort streams that are already open"
    );
    assert!(
        session.alloc_sid().is_err(),
        "exhausted session ids must not wrap around"
    );
    assert!(
        session.open_stream("a.example", 80, false).await.is_err(),
        "open_stream must fail on an exhausted session"
    );
    // Retirement must not break traffic that is already open: existing
    // streams' frames (and the session keepalive) keep flowing even
    // though the pool will never select this session again.
    assert!(
        session.send_frame(encode_keepalive_frame()).await.is_ok(),
        "retired sessions must keep sending existing-stream frames"
    );
}

#[tokio::test]
async fn udp_packet_round_trip_with_destination() {
    let (session, _log) = session_pair(vec![]).await;
    let pool_session = Arc::new(MuxSession {
        kind: super::super::client::SessionKind::MuxCool(Arc::clone(&session)),
        streams: AtomicUsize::new(1),
        last_used_ms: meow_common::atomic::AtomicU::new(0),
        generation: 0,
    });
    let parts = session
        .open_stream_parts("127.0.0.1", 18082, true)
        .await
        .unwrap();
    let conn = PacketConn::new(parts, pool_session, "127.0.0.1:18082".parse().unwrap());
    let dst = "127.0.0.1:18082".parse().unwrap();
    conn.write_packet(b"ping-udp", &dst).await.unwrap();
    let mut buf = [0u8; 32];
    let (n, src) = conn.read_packet(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"ping-udp");
    assert_eq!(src, dst);
}

#[tokio::test]
async fn oversized_udp_rejection_preserves_packet_and_tcp_streams() {
    let (session, _log) = session_pair(vec![]).await;
    let pool_session = Arc::new(MuxSession {
        kind: super::super::client::SessionKind::MuxCool(Arc::clone(&session)),
        streams: AtomicUsize::new(1),
        last_used_ms: meow_common::atomic::AtomicU::new(0),
        generation: 0,
    });
    let parts = session
        .open_stream_parts("127.0.0.1", 18082, true)
        .await
        .unwrap();
    let dst = "127.0.0.1:18082".parse().unwrap();
    let conn = PacketConn::new(parts, pool_session, dst);
    let mut tcp = session.open_stream("a.example", 80, false).await.unwrap();

    // Xray accepts at most 8192 bytes per UDP frame. None of these
    // rejected packets may reach the peer or poison the shared session.
    for size in [8193, 32768, 65536] {
        conn.write_packet(&vec![0x55; size], &dst)
            .await
            .expect_err("oversized UDP must be rejected locally");
    }
    let payload = vec![0xa5; 8192];
    assert_eq!(conn.write_packet(&payload, &dst).await.unwrap(), 8192);
    let mut buf = vec![0; 8192];
    let (n, src) = tokio::time::timeout(Duration::from_secs(5), conn.read_packet(&mut buf))
        .await
        .expect("valid UDP must still receive a response")
        .unwrap();
    assert_eq!(&buf[..n], &payload);
    assert_eq!(src, dst);

    tcp.write_all(b"tcp-survives").await.unwrap();
    let mut reply = [0; 12];
    tokio::time::timeout(Duration::from_secs(5), tcp.read_exact(&mut reply))
        .await
        .expect("sibling TCP stream must remain usable")
        .unwrap();
    assert_eq!(&reply, b"tcp-survives");
}

// --- VLESS integration (request header + response consumption) -----------

/// Full VLESS Mux.Cool handshake over a duplex pair: the mock server
/// validates the CommandMux request header, answers the 2-byte VLESS
/// response, then speaks Mux.Cool frames.  Exercises the lazy response
/// consumption inside the session reader task.
#[cfg(feature = "vless")]
#[tokio::test]
async fn vless_mux_session_round_trip() {
    use crate::vless::VlessConn;

    const UUID: [u8; 16] = [
        0xb8, 0x31, 0x38, 0x1d, 0x63, 0x24, 0x4d, 0x53, 0xad, 0x4f, 0x8c, 0xda, 0x48, 0xb3, 0x08,
        0x11,
    ];
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let log = Arc::new(StdMutex::new(ServerLog::default()));
    let server_log = Arc::clone(&log);
    tokio::spawn(async move {
        let mut io = server_io;
        // VLESS request: version + uuid + addon_len + cmd = 19 bytes.
        let mut req = [0u8; 19];
        io.read_exact(&mut req).await.unwrap();
        assert_eq!(req[0], 0x00);
        assert_eq!(&req[1..17], &UUID);
        assert_eq!(req[17], 0x00);
        assert_eq!(req[18], 0x03, "command must be CommandMux");
        // VLESS response header, then Mux.Cool frames.
        io.write_all(&[0x00, 0x00]).await.unwrap();
        loop {
            let Ok(meta_len) = read_u16(&mut io).await else {
                return;
            };
            let mut meta = vec![0u8; meta_len as usize];
            if io.read_exact(&mut meta).await.is_err() {
                return;
            }
            let frame = decode_meta(&meta).unwrap();
            if frame.status == STATUS_KEEP && frame.option & OPTION_DATA != 0 {
                let data_len = read_u16(&mut io).await.unwrap();
                let mut data = vec![0u8; data_len as usize];
                io.read_exact(&mut data).await.unwrap();
                server_log.lock().unwrap().data_frames += 1;
                let echo = encode_data_frame(frame.session_id, &data, None).unwrap();
                if io.write_all(&echo).await.is_err() {
                    return;
                }
            }
        }
    });
    let vless = VlessConn::new_mux(Box::new(TestConn(client_io)), &UUID, None)
        .await
        .unwrap();
    let session = MuxCoolSession::client(Box::new(vless)).await.unwrap();
    let mut stream = session.open_stream("10.0.2.2", 18081, false).await.unwrap();
    stream.write_all(b"via-vless").await.unwrap();
    let mut buf = [0u8; 16];
    let n = stream.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"via-vless");
    assert_eq!(log.lock().unwrap().data_frames, 1);
}

// --- Live interop probe (repeatable regression, #[ignore]) ---------------

/// Live probe against a running sing-box VLESS inbound
/// (127.0.0.1:4411) and the host UDP echo server
/// (target/e2e/udp_echo.py on :18082).  Run with --ignored — requires
/// sing-box (target/e2e/mux-interop/sb.json) and udp_echo up.  Exercises
/// the full Mux.Cool path: VLESS CommandMux handshake -> New frame ->
/// Keep frames -> sing-vmess HandleMuxConnection -> direct relay -> echo.
#[tokio::test]
#[ignore]
#[cfg(feature = "vless")]
async fn live_singbox_vless_muxcool_probe() {
    use crate::mux::{DialFn, MuxClient, MuxOptions, Protocol};
    use crate::vless::VlessConn;
    use crate::StreamConn;
    use meow_common::{MeowError, ProxyConn};

    // b831381d-6324-4d53-ad4f-8cda48b30811 (sb.json vless-mux-in)
    const UUID: [u8; 16] = [
        0xb8, 0x31, 0x38, 0x1d, 0x63, 0x24, 0x4d, 0x53, 0xad, 0x4f, 0x8c, 0xda, 0x48, 0xb3, 0x08,
        0x11,
    ];
    let dial: DialFn = Arc::new(move || {
        Box::pin(async move {
            let tcp = tokio::net::TcpStream::connect(("127.0.0.1", 4411))
                .await
                .map_err(MeowError::Io)?;
            let conn = VlessConn::new_mux(Box::new(tcp), &UUID, None).await?;
            Ok(Box::new(StreamConn(Box::new(conn))) as Box<dyn ProxyConn>)
        })
    });
    let client = MuxClient::new(
        dial,
        MuxOptions {
            protocol: Protocol::MuxCool,
            ..MuxOptions::default()
        },
    );

    // TCP: GET the local websrv through the mux session.
    let mut conn = client.open_stream("127.0.0.1", 18081).await.unwrap();
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    conn.write_all(b"GET / HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n")
        .await
        .unwrap();
    let mut buf = vec![0u8; 256];
    let n = conn.read(&mut buf).await.unwrap();
    assert!(
        String::from_utf8_lossy(&buf[..n]).contains("200"),
        "websrv must answer 200 via muxcool; got {:?}",
        &buf[..n]
    );

    // UDP: datagram echo with per-packet destination.
    let packet = client.open_packet_stream("127.0.0.1", 18082).await.unwrap();
    packet
        .write_packet(b"muxcool-udp", &"127.0.0.1:18082".parse().unwrap())
        .await
        .unwrap();
    let mut pbuf = [0u8; 64];
    let (pn, src) = packet.read_packet(&mut pbuf).await.unwrap();
    assert_eq!(&pbuf[..pn], b"muxcool-udp");
    assert_eq!(
        src,
        "127.0.0.1:18082".parse::<std::net::SocketAddr>().unwrap()
    );
}
