use super::*;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt};

struct ChunkedStream {
    inner: tokio::io::DuplexStream,
    max_write: usize,
}

impl AsyncRead for ChunkedStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for ChunkedStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let count = buf.len().min(this.max_write);
        Pin::new(&mut this.inner).poll_write(cx, &buf[..count])
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// Test UUID: b831381d-6324-4d53-ad4f-8cda48b30811
const TEST_UUID: [u8; 16] = [
    0xb8, 0x31, 0x38, 0x1d, 0x63, 0x24, 0x4d, 0x53, 0xad, 0x4f, 0x8c, 0xda, 0x48, 0xb3, 0x08, 0x11,
];

/// Spawn a minimal VLESS mock server:
/// 1. Reads (and ignores) `header_len` bytes (the request header).
/// 2. Sends [0x00, 0x00] (valid response header).
/// 3. Echoes all subsequent bytes.
///
/// Returns `(server_stream, header_bytes_rx)` — `header_bytes_rx` yields
/// the raw request-header bytes the server received.
async fn spawn_mock(
    header_len: usize,
    server_side: Box<dyn Stream>,
) -> tokio::sync::oneshot::Receiver<Vec<u8>> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let mut s = server_side;
        let mut req_hdr = vec![0u8; header_len];
        s.read_exact(&mut req_hdr).await.unwrap();
        let _ = tx.send(req_hdr);
        // Send valid response header.
        s.write_all(&[0x00, 0x00]).await.unwrap();
        // Echo payload.
        let mut buf = [0u8; 4096];
        loop {
            match s.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if s.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
    rx
}

// ─── F1: request header written on connect ────────────────────────────────

#[tokio::test]
async fn vless_conn_writes_request_header_on_connect() {
    // The minimum VLESS header for IPv4 is:
    // version(1) + uuid(16) + addon_length(1) + cmd(1) + port(2) + addr_type(1) + ipv4(4) = 26
    let (client, server) = duplex(1024);
    let hdr_rx = spawn_mock(26, Box::new(server)).await;

    let conn = VlessConn::new(
        Box::new(client),
        &TEST_UUID,
        None,
        Cmd::Tcp,
        80,
        &VlessAddr::Ipv4([1, 2, 3, 4]),
    )
    .await
    .expect("VlessConn::new");

    let hdr = hdr_rx.await.expect("header bytes");
    drop(conn);

    // version must be 0x00
    assert_eq!(hdr[0], 0x00, "version byte must be 0x00");
    // uuid must match
    assert_eq!(&hdr[1..17], &TEST_UUID, "uuid must match");
}

#[cfg(any(feature = "mux", feature = "vless-vision"))]
#[tokio::test]
async fn deferred_header_handles_one_byte_partial_writes() {
    let (client, mut server) = duplex(1024);
    tokio::spawn(async move {
        let mut request = vec![0u8; 26 + 4];
        server.read_exact(&mut request).await.unwrap();
        assert_eq!(&request[26..], b"ping");
        server.write_all(&[0x00, 0x00, b'!']).await.unwrap();
    });

    let mut conn = VlessConn::new_deferred(
        Box::new(ChunkedStream {
            inner: client,
            max_write: 1,
        }),
        &TEST_UUID,
        None,
        Cmd::Tcp,
        80,
        &VlessAddr::Ipv4([1, 2, 3, 4]),
    )
    .await
    .unwrap();
    conn.write_all(b"ping").await.unwrap();
    let mut response = [0u8; 1];
    conn.read_exact(&mut response).await.unwrap();
    assert_eq!(response, [b'!']);
}

#[cfg(any(feature = "mux", feature = "vless-vision"))]
#[tokio::test]
async fn deferred_header_is_flushed_before_server_first_read() {
    let (client, mut server) = duplex(1024);
    tokio::spawn(async move {
        let mut request = vec![0u8; 26];
        server.read_exact(&mut request).await.unwrap();
        server.write_all(&[0x00, 0x00]).await.unwrap();
        server.write_all(b"banner").await.unwrap();
    });

    let mut conn = VlessConn::new_deferred(
        Box::new(client),
        &TEST_UUID,
        None,
        Cmd::Tcp,
        25,
        &VlessAddr::Ipv4([1, 2, 3, 4]),
    )
    .await
    .unwrap();
    let mut banner = [0u8; 6];
    conn.read_exact(&mut banner).await.unwrap();
    assert_eq!(&banner, b"banner");
}

// ─── F2: payload round-trip ───────────────────────────────────────────────

#[tokio::test]
async fn vless_conn_tcp_payload_round_trips() {
    use tokio::io::AsyncReadExt;
    let (client, server) = duplex(4096);
    spawn_mock(26, Box::new(server)).await;

    let mut conn = VlessConn::new(
        Box::new(client),
        &TEST_UUID,
        None,
        Cmd::Tcp,
        80,
        &VlessAddr::Ipv4([1, 2, 3, 4]),
    )
    .await
    .expect("VlessConn::new");

    let payload = vec![0xAB; 1024];
    conn.write_all(&payload).await.unwrap();

    let mut received = vec![0u8; 1024];
    conn.read_exact(&mut received).await.unwrap();
    assert_eq!(received, payload, "round-trip payload must match");
}

// ─── F3: response header discarded ───────────────────────────────────────

#[tokio::test]
async fn vless_conn_reads_and_discards_response_header() {
    use tokio::io::AsyncReadExt;
    let (client, server) = duplex(1024);
    // The mock sends [0x00, 0x00] then a distinguishable byte pattern.
    tokio::spawn(async move {
        let mut s = server;
        // Consume the request header.
        let mut hdr = vec![0u8; 26];
        s.read_exact(&mut hdr).await.unwrap();
        // Send response header + payload.
        s.write_all(&[0x00, 0x00, 0xDE, 0xAD, 0xBE, 0xEF])
            .await
            .unwrap();
    });

    let mut conn = VlessConn::new(
        Box::new(client),
        &TEST_UUID,
        None,
        Cmd::Tcp,
        80,
        &VlessAddr::Ipv4([1, 2, 3, 4]),
    )
    .await
    .expect("VlessConn::new");

    // First read must yield payload bytes (0xDE 0xAD 0xBE 0xEF), not response header.
    let mut buf = [0u8; 4];
    conn.read_exact(&mut buf).await.unwrap();
    assert_eq!(
        buf,
        [0xDE, 0xAD, 0xBE, 0xEF],
        "response header must be discarded"
    );
}

// ─── F4: nonzero addon_length in response ─────────────────────────────────

/// Mock sends [0x00, 0x03, 0xAA, 0xBB, 0xCC] (version=0, addon=3 bytes).
/// VlessConn must discard the 3 addon bytes and expose only subsequent payload.
#[tokio::test]
async fn vless_conn_response_with_nonzero_addon_length() {
    use tokio::io::AsyncReadExt;
    let (client, server) = duplex(1024);
    tokio::spawn(async move {
        let mut s = server;
        let mut hdr = vec![0u8; 26];
        s.read_exact(&mut hdr).await.unwrap();
        // Response: version=0, addon_length=3, 3 addon bytes, then payload.
        s.write_all(&[0x00, 0x03, 0xAA, 0xBB, 0xCC, 0x42])
            .await
            .unwrap();
    });

    let mut conn = VlessConn::new(
        Box::new(client),
        &TEST_UUID,
        None,
        Cmd::Tcp,
        80,
        &VlessAddr::Ipv4([1, 2, 3, 4]),
    )
    .await
    .expect("VlessConn::new");

    let mut buf = [0u8; 1];
    conn.read_exact(&mut buf).await.unwrap();
    assert_eq!(
        buf[0], 0x42,
        "addon bytes must be discarded; next byte is payload 0x42"
    );
}

// ─── F5: version mismatch → ConnError ────────────────────────────────────

#[tokio::test]
async fn vless_conn_version_mismatch_tears_down() {
    let (client, server) = duplex(1024);
    tokio::spawn(async move {
        let mut s = server;
        let mut hdr = vec![0u8; 26];
        let _ = s.read_exact(&mut hdr).await;
        s.write_all(&[0x01, 0x00]).await.unwrap();
    });

    // VlessConn::new succeeds (response is read lazily).
    let mut conn = VlessConn::new(
        Box::new(client),
        &TEST_UUID,
        None,
        Cmd::Tcp,
        80,
        &VlessAddr::Ipv4([1, 2, 3, 4]),
    )
    .await
    .expect("new succeeds with lazy response");

    // First read triggers lazy response consumption and must error.
    let mut buf = [0u8; 1];
    let result = tokio::io::AsyncReadExt::read(&mut conn, &mut buf).await;
    assert!(result.is_err(), "version mismatch must return Err on read");
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("version") || msg.contains("mismatch"),
        "error must mention version mismatch, got: {msg}"
    );
}

// ─── F6: server EOF after header ─────────────────────────────────────────

#[tokio::test]
async fn vless_conn_server_eof_after_header() {
    let (client, server) = duplex(1024);
    tokio::spawn(async move {
        let mut s = server;
        let mut hdr = vec![0u8; 26];
        let _ = s.read_exact(&mut hdr).await;
        // Close immediately (no response).
        drop(s);
    });

    // VlessConn::new succeeds (response is read lazily).
    let mut conn = VlessConn::new(
        Box::new(client),
        &TEST_UUID,
        None,
        Cmd::Tcp,
        80,
        &VlessAddr::Ipv4([1, 2, 3, 4]),
    )
    .await
    .expect("new succeeds with lazy response");

    // First read triggers lazy response consumption and must error on EOF.
    let mut buf = [0u8; 1];
    let result = tokio::io::AsyncReadExt::read(&mut conn, &mut buf).await;
    assert!(result.is_err(), "EOF after header must return Err on read");
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("closed")
            || msg.contains("eof")
            || msg.contains("EOF")
            || msg.contains("server")
            || msg.contains("Eof"),
        "error must give diagnostic, got: {msg}"
    );
}

// ─── Issue #278: parked read must not starve writes ───────────────────────

/// Regression for issue #278: with the whole stream behind one async
/// mutex, a `read_packet` parked waiting for a server datagram held the
/// lock and every `write_packet` deadlocked, stalling any flow that
/// interleaves sends with a pending recv (QUIC, WireGuard, games).
#[tokio::test]
async fn vless_packet_conn_write_proceeds_while_read_parked() {
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::time::timeout;

    let (client, server) = duplex(4096);
    // Mock server: consume the request header, ack, then echo one framed
    // datagram once it arrives.
    tokio::spawn(async move {
        let mut s = server;
        let mut hdr = vec![0u8; 26];
        s.read_exact(&mut hdr).await.unwrap();
        s.write_all(&[0x00, 0x00]).await.unwrap();
        let mut len = [0u8; 2];
        s.read_exact(&mut len).await.unwrap();
        let n = u16::from_be_bytes(len) as usize;
        let mut payload = vec![0u8; n];
        s.read_exact(&mut payload).await.unwrap();
        s.write_all(&len).await.unwrap();
        s.write_all(&payload).await.unwrap();
    });

    let conn = Arc::new(
        VlessPacketConn::new(
            Box::new(client),
            &TEST_UUID,
            53,
            &VlessAddr::Ipv4([8, 8, 8, 8]),
        )
        .await
        .expect("VlessPacketConn::new"),
    );

    // Park a reader while no server datagram is available.
    let reader_conn = Arc::clone(&conn);
    let reader = tokio::spawn(async move {
        let mut buf = [0u8; 64];
        reader_conn
            .read_packet(&mut buf)
            .await
            .map(|(n, _)| buf[..n].to_vec())
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Old design: deadlocked here (reader held the stream lock).
    let dst = "8.8.8.8:53".parse().unwrap();
    timeout(Duration::from_secs(5), conn.write_packet(b"ping", &dst))
        .await
        .expect("write_packet must not deadlock while a read is parked")
        .expect("write_packet");

    let echoed = timeout(Duration::from_secs(5), reader)
        .await
        .expect("parked reader timed out")
        .expect("reader task")
        .expect("read_packet");
    assert_eq!(echoed, b"ping");
}

// ─── F8b: lazy response header (xray/sing-vmess semantics) ───────────────

/// The server answers the VLESS response header only together with the
/// first reply datagram.  `VlessPacketConn::new` must return without
/// waiting for it (eager consumption deadlocks: the server waits for the
/// first datagram before answering, the client waits for the answer
/// before sending that datagram).
#[tokio::test]
async fn vless_packet_conn_handles_lazy_response_header() {
    use tokio::io::AsyncWriteExt;

    let (client, server) = duplex(4096);
    tokio::spawn(async move {
        let mut s = server;
        let mut hdr = vec![0u8; 26];
        s.read_exact(&mut hdr).await.unwrap();
        // NO response header yet — wait for the first datagram frame.
        let mut len = [0u8; 2];
        s.read_exact(&mut len).await.unwrap();
        let n = u16::from_be_bytes(len) as usize;
        let mut payload = vec![0u8; n];
        s.read_exact(&mut payload).await.unwrap();
        // First server write = response header + echoed datagram frame.
        s.write_all(&[0x00, 0x00]).await.unwrap();
        s.write_all(&len).await.unwrap();
        s.write_all(&payload).await.unwrap();
    });

    let conn = VlessPacketConn::new(
        Box::new(client),
        &TEST_UUID,
        53,
        &VlessAddr::Ipv4([8, 8, 8, 8]),
    )
    .await
    .expect("VlessPacketConn::new");

    let dst = "8.8.8.8:53".parse().unwrap();
    conn.write_packet(b"ping", &dst).await.unwrap();
    let mut buf = [0u8; 8];
    let (n, _src) = conn.read_packet(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"ping");
}

// ─── F7: UDP cmd byte is 0x02 ─────────────────────────────────────────────

/// Guards against copy-paste of the TCP path without flipping the cmd byte.
#[tokio::test]
async fn vless_packet_conn_cmd_byte_is_0x02() {
    let (client, server) = duplex(1024);
    let (tx, rx) = tokio::sync::oneshot::channel::<Vec<u8>>();
    tokio::spawn(async move {
        let mut s = server;
        // Capture first 26 bytes (UDP header = same size as basic IPv4 TCP header).
        let mut hdr = vec![0u8; 26];
        s.read_exact(&mut hdr).await.unwrap();
        let _ = tx.send(hdr);
        s.write_all(&[0x00, 0x00]).await.unwrap();
        // Drain.
        let mut buf = [0u8; 64];
        while s.read(&mut buf).await.unwrap_or(0) > 0 {}
    });

    let _conn = VlessPacketConn::new(
        Box::new(client),
        &TEST_UUID,
        53,
        &VlessAddr::Ipv4([8, 8, 8, 8]),
    )
    .await
    .expect("VlessPacketConn::new");

    let hdr = rx.await.expect("header");
    // cmd byte at offset 18 (version=1 + uuid=16 + addon_length=1)
    assert_eq!(hdr[18], 0x02, "UDP cmd must be 0x02, not 0x01");
}

// ─── F8: Mux request (CommandMux=0x03) omits port/address ─────────────────

/// The Mux.Cool session request is exactly
/// `version(1) + uuid(16) + addon_length(1) + cmd(1)` = 19 bytes — no
/// port or address follow (xray encoding.go::EncodeRequestHeader skips
/// them for `RequestCommandMux`; sing-vmess skips the parse likewise).
#[cfg(feature = "mux")]
#[tokio::test]
async fn vless_mux_request_omits_port_and_address() {
    let (client, server) = duplex(1024);
    let (tx, rx) = tokio::sync::oneshot::channel::<Vec<u8>>();
    tokio::spawn(async move {
        let mut s = server;
        let mut hdr = vec![0u8; 19];
        s.read_exact(&mut hdr).await.unwrap();
        let _ = tx.send(hdr);
        s.write_all(&[0x00, 0x00]).await.unwrap();
        // Drain anything further (there must be nothing after 19 bytes
        // until the client's first mux frame — covered by mux tests).
        let mut buf = [0u8; 64];
        while s.read(&mut buf).await.unwrap_or(0) > 0 {}
    });

    let _conn = VlessConn::new_mux(Box::new(client), &TEST_UUID, None)
        .await
        .expect("VlessConn::new_mux");

    let hdr = rx.await.expect("header");
    assert_eq!(hdr.len(), 19, "mux request must be exactly 19 bytes");
    assert_eq!(hdr[0], 0x00, "version must be 0x00");
    assert_eq!(&hdr[1..17], &TEST_UUID, "uuid must match");
    assert_eq!(hdr[17], 0x00, "no flow addon");
    assert_eq!(hdr[18], 0x03, "command must be CommandMux (0x03)");
}

// ─── F6: UDP reader cancellation safety ─────────────────────────────────

/// A read cancelled halfway through the response header must resume
/// instead of shifting the stream: the next read sees the full
/// datagram.
#[tokio::test]
async fn udp_read_survives_cancellation_mid_response() {
    let (client, mut server) = duplex(1024);
    let (go_tx, go_rx) = tokio::sync::oneshot::channel::<()>();
    let server_task = tokio::spawn(async move {
        let mut request = vec![0u8; 26];
        server.read_exact(&mut request).await.unwrap();
        server.write_all(&[0x00]).await.unwrap(); // first header byte only
        let _ = go_rx.await;
        server
            .write_all(&[0x00, 0x00, 0x04, b'p', b'o', b'n', b'g'])
            .await
            .unwrap();
    });
    let conn = VlessPacketConn::new(
        Box::new(client),
        &TEST_UUID,
        53,
        &VlessAddr::Ipv4([8, 8, 8, 8]),
    )
    .await
    .unwrap();
    let mut buf = [0u8; 16];
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            conn.read_packet(&mut buf)
        )
        .await
        .is_err(),
        "the read must wait for the second response byte"
    );
    go_tx.send(()).unwrap();
    let (n, _) = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        conn.read_packet(&mut buf),
    )
    .await
    .expect("the resumed read must complete")
    .unwrap();
    assert_eq!(&buf[..n], b"pong");
    server_task.await.unwrap();
}

/// A read cancelled halfway through a datagram payload must resume
/// the same datagram.
#[tokio::test]
async fn udp_read_survives_cancellation_mid_payload() {
    let (client, mut server) = duplex(1024);
    let (go_tx, go_rx) = tokio::sync::oneshot::channel::<()>();
    let server_task = tokio::spawn(async move {
        let mut request = vec![0u8; 26];
        server.read_exact(&mut request).await.unwrap();
        // Full response + length prefix + half the payload.
        server
            .write_all(&[0x00, 0x00, 0x00, 0x04, b'p', b'o'])
            .await
            .unwrap();
        let _ = go_rx.await;
        server.write_all(b"ng").await.unwrap();
    });
    let conn = VlessPacketConn::new(
        Box::new(client),
        &TEST_UUID,
        53,
        &VlessAddr::Ipv4([8, 8, 8, 8]),
    )
    .await
    .unwrap();
    let mut buf = [0u8; 16];
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            conn.read_packet(&mut buf)
        )
        .await
        .is_err(),
        "the read must wait for the rest of the payload"
    );
    go_tx.send(()).unwrap();
    let (n, _) = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        conn.read_packet(&mut buf),
    )
    .await
    .expect("the resumed read must complete")
    .unwrap();
    assert_eq!(&buf[..n], b"pong");
    server_task.await.unwrap();
}

/// Response version mismatch → hard error naming the problem (the
/// shared decode_response behavior, now owned by the UDP reader).
#[tokio::test]
async fn udp_response_version_mismatch_errors() {
    let (client, mut server) = duplex(1024);
    let server_task = tokio::spawn(async move {
        let mut request = vec![0u8; 26];
        server.read_exact(&mut request).await.unwrap();
        server.write_all(&[0x01, 0x00]).await.unwrap();
    });
    let conn = VlessPacketConn::new(
        Box::new(client),
        &TEST_UUID,
        53,
        &VlessAddr::Ipv4([8, 8, 8, 8]),
    )
    .await
    .unwrap();
    let mut buf = [0u8; 16];
    let err = conn.read_packet(&mut buf).await.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("version mismatch"),
        "error must mention the version mismatch, got: {msg}"
    );
    server_task.await.unwrap();
}

/// Non-zero addon_length: the addon bytes are discarded and the next
/// datagram reads aligned.
#[tokio::test]
async fn udp_response_addon_is_discarded() {
    let (client, mut server) = duplex(1024);
    let server_task = tokio::spawn(async move {
        let mut request = vec![0u8; 26];
        server.read_exact(&mut request).await.unwrap();
        server
            .write_all(&[0x00, 0x02, 0xAA, 0xBB, 0x00, 0x04, b'p', b'o', b'n', b'g'])
            .await
            .unwrap();
    });
    let conn = VlessPacketConn::new(
        Box::new(client),
        &TEST_UUID,
        53,
        &VlessAddr::Ipv4([8, 8, 8, 8]),
    )
    .await
    .unwrap();
    let mut buf = [0u8; 16];
    let (n, _) = conn.read_packet(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"pong");
    server_task.await.unwrap();
}

/// A response truncated mid-header must error cleanly, not panic.
#[tokio::test]
async fn udp_truncated_response_errors() {
    let (client, mut server) = duplex(1024);
    let server_task = tokio::spawn(async move {
        let mut request = vec![0u8; 26];
        server.read_exact(&mut request).await.unwrap();
        server.write_all(&[0x00]).await.unwrap();
        drop(server);
    });
    let conn = VlessPacketConn::new(
        Box::new(client),
        &TEST_UUID,
        53,
        &VlessAddr::Ipv4([8, 8, 8, 8]),
    )
    .await
    .unwrap();
    let mut buf = [0u8; 16];
    assert!(conn.read_packet(&mut buf).await.is_err());
    server_task.await.unwrap();
}

/// Issue #514 review: the resumable reader makes cancelled READS safe,
/// but a `write_packet` future dropped mid-`write_all` has already put
/// a frame prefix on the wire — the peer parses the next frame's header
/// as payload. The write path poisons the conn so every later op fails
/// fast instead of feeding a desynced stream.
#[tokio::test]
async fn udp_cancelled_write_poisons_conn() {
    let (client, mut server) = duplex(1024);
    let server_task = tokio::spawn(async move {
        // Consume the request header, then never read again so the
        // next datagram write pends mid-frame.
        let mut request = vec![0u8; 26];
        server.read_exact(&mut request).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    });
    let conn = VlessPacketConn::new(
        Box::new(client),
        &TEST_UUID,
        53,
        &VlessAddr::Ipv4([8, 8, 8, 8]),
    )
    .await
    .unwrap();
    let dst: std::net::SocketAddr = "9.9.9.9:53".parse().unwrap();

    let cancelled = tokio::time::timeout(
        std::time::Duration::from_millis(50),
        conn.write_packet(&[0xAA; 4096], &dst),
    )
    .await;
    assert!(cancelled.is_err(), "write must have timed out mid-frame");

    let err = conn.write_packet(b"x", &dst).await.unwrap_err();
    assert!(
        err.to_string().contains("desynced"),
        "write after a cancelled write must fail fast, got {err:?}"
    );
    let mut buf = [0u8; 64];
    let err = conn.read_packet(&mut buf).await.unwrap_err();
    assert!(
        err.to_string().contains("desynced"),
        "read on a write-poisoned conn must fail fast, got {err:?}"
    );
    server_task.abort();
}
