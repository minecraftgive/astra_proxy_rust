use super::*;

#[tokio::test]
async fn read_packet_handles_coalesced_frames() {
    use tokio::io::AsyncWriteExt;
    // QUIC first flight: server sends ~3 datagrams that coalesce into one
    // TLS record → meow must yield all three, not just the first.
    let (client, mut server) = tokio::io::duplex(64 * 1024);
    let conn = TrojanPacketConn::new(Box::new(client), None);

    let src: SocketAddr = "9.9.9.9:443".parse().unwrap();
    let payloads: [&[u8]; 3] = [b"\xc0one", b"\xc0two", b"\xc0three!"];
    let mut wire = Vec::new();
    for p in &payloads {
        encode_socks5_addr_socket(&mut wire, &src);
        wire.extend_from_slice(&(p.len() as u16).to_be_bytes());
        wire.extend_from_slice(b"\r\n");
        wire.extend_from_slice(p);
    }
    server.write_all(&wire).await.unwrap(); // one write → coalesced
    server.flush().await.unwrap();

    for expect in &payloads {
        let mut buf = [0u8; 2048];
        let (n, addr) = conn.read_packet(&mut buf).await.unwrap();
        assert_eq!(addr, src);
        assert_eq!(&buf[..n], *expect, "frame mismatch / dropped frame");
    }
}

/// Issue #514: cancelling `read_packet` mid-frame consumes an unknown
/// number of bytes — resuming would parse payload bytes as a header.
/// The conn must poison itself so the tunnel re-dials instead.
#[tokio::test]
async fn cancelled_read_poisons_packet_conn() {
    use tokio::io::AsyncWriteExt;
    let (client, mut server) = tokio::io::duplex(64 * 1024);
    let conn = TrojanPacketConn::new(Box::new(client), None);
    let mut buf = [0u8; 2048];

    // Peer sends a partial header then stalls: ATYP + one address byte.
    server.write_all(&[ATYP_IPV4, 9, 9]).await.unwrap();
    let cancelled = tokio::time::timeout(
        std::time::Duration::from_millis(50),
        conn.read_packet(&mut buf),
    )
    .await;
    assert!(cancelled.is_err(), "read must have timed out mid-header");

    // The peer then completes the frame — but the conn must not try to
    // resume mid-stream.
    server
        .write_all(&[9, 9, 0x01, 0xbb, 0, 1, b'\r', b'\n', b'x'])
        .await
        .unwrap();
    let err = conn.read_packet(&mut buf).await.unwrap_err();
    assert!(
        err.to_string().contains("desynced"),
        "expected desync error after cancelled read, got {err:?}"
    );
}

/// The same poison must fire on a parse error after bytes were consumed
/// (bad CRLF) — not only on future cancellation.
#[tokio::test]
async fn errored_read_poisons_packet_conn() {
    use tokio::io::AsyncWriteExt;
    let (client, mut server) = tokio::io::duplex(64 * 1024);
    let conn = TrojanPacketConn::new(Box::new(client), None);
    let mut buf = [0u8; 2048];

    // Valid addr + length, then a wrong CRLF marker.
    let mut wire = Vec::new();
    encode_socks5_addr_socket(&mut wire, &"9.9.9.9:443".parse().unwrap());
    wire.extend_from_slice(&1u16.to_be_bytes());
    wire.extend_from_slice(b"XX");
    wire.extend_from_slice(b"z");
    server.write_all(&wire).await.unwrap();

    let err = conn.read_packet(&mut buf).await.unwrap_err();
    assert!(err.to_string().contains("CRLF"), "got {err:?}");

    let err = conn.read_packet(&mut buf).await.unwrap_err();
    assert!(
        err.to_string().contains("desynced"),
        "second read must fail fast on poisoned conn, got {err:?}"
    );
}

/// Happy-path guard: a completed frame read must not poison the conn.
#[tokio::test]
async fn completed_read_leaves_conn_usable() {
    use tokio::io::AsyncWriteExt;
    let (client, mut server) = tokio::io::duplex(64 * 1024);
    let conn = TrojanPacketConn::new(Box::new(client), None);
    let mut buf = [0u8; 2048];
    let src: SocketAddr = "9.9.9.9:443".parse().unwrap();

    for i in 0u8..2 {
        let payload = [i, i, i];
        let mut wire = Vec::new();
        encode_socks5_addr_socket(&mut wire, &src);
        wire.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        wire.extend_from_slice(b"\r\n");
        wire.extend_from_slice(&payload);
        server.write_all(&wire).await.unwrap();
        let (n, addr) = conn.read_packet(&mut buf).await.unwrap();
        assert_eq!(addr, src);
        assert_eq!(&buf[..n], &payload);
    }
}

/// Issue #514 review: a `write_packet` future cancelled mid-`write_all`
/// has already pushed a frame PREFIX onto the wire — the peer reads
/// `len` payload bytes from the next frame's header and the stream is
/// desynced on the uplink direction, which the read-side poison cannot
/// observe. The write path must poison the conn symmetrically.
#[tokio::test]
async fn cancelled_write_poisons_packet_conn() {
    // Tiny pipe: a 4 KiB datagram exceeds the buffer, so `write_all`
    // pends mid-frame with a prefix already on the wire.
    let (client, server) = tokio::io::duplex(1024);
    let conn = TrojanPacketConn::new(Box::new(client), None);
    let dst: SocketAddr = "9.9.9.9:53".parse().unwrap();

    let cancelled = tokio::time::timeout(
        std::time::Duration::from_millis(50),
        conn.write_packet(&[0xAA; 4096], &dst),
    )
    .await;
    assert!(cancelled.is_err(), "write must have timed out mid-frame");
    drop(server);

    let err = conn.write_packet(b"x", &dst).await.unwrap_err();
    assert!(
        err.to_string().contains("desynced"),
        "write after a cancelled write must fail fast, got {err:?}"
    );
    let mut buf = [0u8; 64];
    let err = conn.read_packet(&mut buf).await.unwrap_err();
    assert!(
        err.to_string().contains("desynced"),
        "read after a cancelled write must fail fast, got {err:?}"
    );
}

/// A write parked on the writer mutex while another write is cancelled
/// mid-frame must fail fast instead of appending after the torn frame.
#[tokio::test]
async fn queued_write_after_cancelled_write_fails_fast() {
    let (client, server) = tokio::io::duplex(1024);
    let conn = std::sync::Arc::new(TrojanPacketConn::new(Box::new(client), None));
    let dst: SocketAddr = "9.9.9.9:53".parse().unwrap();

    // Occupy the writer mid-frame.
    let conn2 = std::sync::Arc::clone(&conn);
    let first = tokio::spawn(async move { conn2.write_packet(&[0xAA; 4096], &dst).await });
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    // Queue a second write behind it, then abort the first mid-frame.
    let conn3 = std::sync::Arc::clone(&conn);
    let queued = tokio::spawn(async move { conn3.write_packet(b"queued", &dst).await });
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    first.abort();
    let err = queued.await.unwrap().unwrap_err();
    assert!(
        err.to_string().contains("desynced"),
        "queued write must fail fast after the poisoned first write, got {err:?}"
    );
    drop(server);
}

#[test]
fn encode_socket_v4() {
    let mut buf = Vec::new();
    encode_socks5_addr_socket(&mut buf, &"127.0.0.1:53".parse().unwrap());
    assert_eq!(buf, vec![ATYP_IPV4, 127, 0, 0, 1, 0, 53]);
}

#[test]
fn encode_socket_v6() {
    let mut buf = Vec::new();
    encode_socks5_addr_socket(&mut buf, &"[::1]:1234".parse().unwrap());
    assert_eq!(buf[0], ATYP_IPV6);
    assert_eq!(buf.len(), 1 + 16 + 2);
    assert_eq!(&buf[buf.len() - 2..], &1234u16.to_be_bytes());
}

#[test]
fn encode_metadata_domain_takes_precedence() {
    let mut buf = Vec::new();
    let md = Metadata {
        host: "example.com".into(),
        dst_ip: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        dst_port: 443,
        ..Default::default()
    };
    encode_socks5_addr_from_metadata(&mut buf, &md);
    assert_eq!(buf[0], ATYP_DOMAIN);
    assert_eq!(buf[1] as usize, "example.com".len());
    assert_eq!(&buf[2..2 + "example.com".len()], b"example.com");
}

#[test]
fn build_header_accepts_max_length_domain() {
    let adapter = TrojanAdapter::new(
        "t",
        "127.0.0.1",
        443,
        "secret",
        "example.com",
        true,
        false,
        std::sync::Arc::new(crate::dialer::DirectDialer),
    );
    let md = Metadata {
        host: "a".repeat(MAX_DOMAIN_LEN).into(),
        dst_port: 443,
        ..Default::default()
    };
    let mut buf = [0u8; TROJAN_HEADER_BUF_SIZE];
    let header = adapter
        .build_header(&md, CMD_CONNECT, &mut buf)
        .expect("255-byte domain should fit");

    assert_eq!(header.len(), TROJAN_HEADER_BUF_SIZE);
    assert_eq!(header[59], ATYP_DOMAIN);
    assert_eq!(header[60] as usize, MAX_DOMAIN_LEN);
}

#[test]
fn build_header_rejects_overlong_domain_without_panic() {
    let adapter = TrojanAdapter::new(
        "t",
        "127.0.0.1",
        443,
        "secret",
        "example.com",
        true,
        false,
        std::sync::Arc::new(crate::dialer::DirectDialer),
    );
    let md = Metadata {
        host: "a".repeat(MAX_DOMAIN_LEN + 1).into(),
        dst_port: 443,
        ..Default::default()
    };
    let mut buf = [0u8; TROJAN_HEADER_BUF_SIZE];
    let err = adapter
        .build_header(&md, CMD_CONNECT, &mut buf)
        .expect_err("256-byte domain must error");

    assert!(
        matches!(err, MeowError::Proxy(ref msg) if msg.contains("domain name too long")),
        "expected domain length proxy error, got {err:?}"
    );
}

/// The per-packet domain stamp a bound UDP association writes when the
/// chained target is a `UdpTarget::Name` (issue #657): ATYP_DOMAIN +
/// u8 length + host + port — the server resolves the name itself.
#[test]
fn encode_socks5_addr_domain_wire_layout() {
    let mut buf = Vec::new();
    encode_socks5_addr_domain(&mut buf, "back.internal", 8388);
    assert_eq!(buf[0], ATYP_DOMAIN);
    assert_eq!(buf[1] as usize, "back.internal".len());
    assert_eq!(&buf[2..15], b"back.internal");
    assert_eq!(&buf[15..], &8388u16.to_be_bytes());
}
