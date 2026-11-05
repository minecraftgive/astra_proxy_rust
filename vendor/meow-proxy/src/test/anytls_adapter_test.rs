use super::*;
use crate::uot::UOT_ATYP_IPV4;
use anytls_rs::protocol::Command;
use meow_common::Network;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

async fn test_session() -> (Arc<Session>, DuplexStream) {
    let (io, peer) = tokio::io::duplex(64);
    let (reader, writer) = tokio::io::split(io);
    // Server sessions skip padding, making the wire assertions exact.
    let session = Arc::new(Session::new_server(
        reader,
        writer,
        PaddingFactory::default(),
    ));
    let writer_session = Arc::clone(&session);
    tokio::spawn(async move {
        writer_session.process_stream_data().await.unwrap();
    });
    // `new_server` does not start the inbound frame reader — the
    // real server spawns `recv_loop` per session (server.rs). Tests
    // that feed peer→session frames need it running.
    let reader_session = Arc::clone(&session);
    tokio::spawn(async move {
        let _ = reader_session.recv_loop().await;
    });
    (session, peer)
}

async fn wire_frame(peer: &mut DuplexStream) -> (Command, u32, Vec<u8>) {
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut header = [0; 7];
        peer.read_exact(&mut header).await.unwrap();
        let mut data = vec![0; u16::from_be_bytes([header[5], header[6]]) as usize];
        peer.read_exact(&mut data).await.unwrap();
        (
            Command::from(header[0]),
            u32::from_be_bytes(header[1..5].try_into().unwrap()),
            data,
        )
    })
    .await
    .expect("writer stalled")
}

fn metadata_for(host: &str, ip: Option<IpAddr>, port: u16) -> Metadata {
    Metadata {
        network: Network::Udp,
        host: smol_str::SmolStr::from(host),
        dst_ip: ip,
        dst_port: port,
        ..Default::default()
    }
}

#[test]
fn tcp_destination_uses_original_host_then_ip() {
    let mut metadata = metadata_for("requested.example", Some("192.0.2.1".parse().unwrap()), 443);
    metadata.network = Network::Tcp;
    metadata.sniff_host = smol_str::SmolStr::from("sniffed.example");
    assert_eq!(
        anytls_tcp_destination(&metadata).unwrap(),
        "requested.example",
        "a sniffed rule host must not replace the requested dial target"
    );

    metadata.host = smol_str::SmolStr::default();
    assert_eq!(anytls_tcp_destination(&metadata).unwrap(), "192.0.2.1");

    metadata.dst_ip = Some("2001:db8::1".parse().unwrap());
    assert_eq!(anytls_tcp_destination(&metadata).unwrap(), "2001:db8::1");

    metadata.dst_ip = None;
    assert!(anytls_tcp_destination(&metadata)
        .unwrap_err()
        .to_string()
        .contains("missing destination host and IP"));
}

#[tokio::test]
async fn tcp_drop_during_partial_frame_preserves_other_streams() {
    let (session, mut peer) = test_session().await;
    let (first, _) = session.open_stream().await.unwrap();
    let first_id = first.id();
    let (second, _) = session.open_stream().await.unwrap();
    let second_id = second.id();
    assert_eq!(wire_frame(&mut peer).await.0, Command::Syn);
    assert_eq!(wire_frame(&mut peer).await.0, Command::Syn);
    let mut first = AnytlsConn::new(first, Arc::clone(&session));
    let mut second = AnytlsConn::new(second, Arc::clone(&session));
    first.write_all(&[42; 512]).await.unwrap();

    // A 64-byte duplex cannot fit this frame. Seeing its prefix proves
    // the writer has started, while the rest is still backpressured.
    let mut prefix = [0; 4];
    peer.read_exact(&mut prefix).await.unwrap();
    drop(first);
    second.write_all(b"still-aligned").await.unwrap();

    let mut remainder = vec![0; 7 + 512 - prefix.len()];
    peer.read_exact(&mut remainder).await.unwrap();
    let mut full_frame = prefix.to_vec();
    full_frame.extend_from_slice(&remainder);
    assert_eq!(full_frame[0], u8::from(Command::Push));
    assert_eq!(
        u32::from_be_bytes(full_frame[1..5].try_into().unwrap()),
        first_id
    );
    assert_eq!(&full_frame[7..], &[42; 512]);
    assert_eq!(
        wire_frame(&mut peer).await,
        (Command::Fin, first_id, vec![])
    );
    assert_eq!(
        wire_frame(&mut peer).await,
        (Command::Push, second_id, b"still-aligned".to_vec())
    );
    drop(second);
    assert_eq!(
        wire_frame(&mut peer).await,
        (Command::Fin, second_id, vec![])
    );
    session.close().await.unwrap();
}

#[tokio::test]
async fn udp_data_and_fin_use_the_stream_writer_fifo() {
    let (session, mut peer) = test_session().await;
    let (stream, _) = session.open_stream().await.unwrap();
    let id = stream.id();
    assert_eq!(wire_frame(&mut peer).await.0, Command::Syn);
    let conn = AnytlsPacketConn::new(stream, Arc::clone(&session), None);
    let destination: SocketAddr = "192.0.2.2:53".parse().unwrap();

    assert_eq!(conn.write_packet(b"udp", &destination).await.unwrap(), 3);
    drop(conn);
    let (cmd, stream_id, data) = wire_frame(&mut peer).await;
    assert_eq!((cmd, stream_id), (Command::Push, id));
    // One datagram is one frame: uot addr + u16 length + payload, with no
    // request prefix — in production the request was already flushed by
    // `create_proxy_stream_with_payload` before the SYNACK wait
    // (issue #535); this harness builds the conn directly so no request
    // exists on the wire here at all.
    assert_eq!(
        data,
        [UOT_ATYP_IPV4, 192, 0, 2, 2, 0, 53, 0, 3, b'u', b'd', b'p']
    );
    assert_eq!(wire_frame(&mut peer).await, (Command::Fin, id, vec![]));
    session.close().await.unwrap();
}

/// Push one data frame into the session from the peer end.
async fn push_frame(peer: &mut DuplexStream, stream_id: u32, data: &[u8]) {
    let mut frame = Vec::with_capacity(7 + data.len());
    frame.push(Command::Push as u8);
    frame.extend_from_slice(&stream_id.to_be_bytes());
    frame.extend_from_slice(&(data.len() as u16).to_be_bytes());
    frame.extend_from_slice(data);
    peer.write_all(&frame).await.unwrap();
}

/// Issue #543: same class as the trojan poison — a `read_packet`
/// cancelled mid-frame releases the reader lock with the stream
/// mid-datagram and every later read silently parses garbage. The
/// conn must poison itself so the tunnel re-dials instead.
#[tokio::test]
async fn udp_cancelled_read_poisons_packet_conn() {
    let (session, mut peer) = test_session().await;
    let (stream, _) = session.open_stream().await.unwrap();
    let id = stream.id();
    assert_eq!(wire_frame(&mut peer).await.0, Command::Syn);
    let conn = AnytlsPacketConn::new(stream, Arc::clone(&session), None);
    let mut buf = [0u8; 2048];

    // A lone ATYP byte: the address read stalls mid-frame.
    push_frame(&mut peer, id, &[UOT_ATYP_IPV4]).await;
    let cancelled =
        tokio::time::timeout(Duration::from_millis(50), conn.read_packet(&mut buf)).await;
    assert!(cancelled.is_err(), "read must have timed out mid-addr");

    // The peer then completes the datagram — the conn must not
    // resume mid-stream.
    push_frame(&mut peer, id, &[9, 9, 9, 9, 0, 53, 0, 1, b'x']).await;
    let err = conn.read_packet(&mut buf).await.unwrap_err();
    assert!(
        err.to_string().contains("desynced"),
        "expected desync error after cancelled read, got {err:?}"
    );

    // Writes fail fast on the desynced conn too — the tunnel
    // re-dials rather than emitting datagrams whose replies can
    // never be parsed.
    let destination: SocketAddr = "192.0.2.2:53".parse().unwrap();
    let err = conn.write_packet(b"x", &destination).await.unwrap_err();
    assert!(
        err.to_string().contains("desynced"),
        "write on a desynced conn must fail fast, got {err:?}"
    );
    session.close().await.unwrap();
}

/// A read parked on the reader mutex while another is cancelled
/// mid-frame must fail fast at the post-lock re-check instead of
/// resuming the torn frame.
#[tokio::test]
async fn udp_parked_read_after_cancelled_read_fails_fast() {
    let (session, mut peer) = test_session().await;
    let (stream, _) = session.open_stream().await.unwrap();
    let id = stream.id();
    assert_eq!(wire_frame(&mut peer).await.0, Command::Syn);
    let conn = Arc::new(AnytlsPacketConn::new(stream, Arc::clone(&session), None));

    // read1 stalls mid-addr while holding the reader lock.
    push_frame(&mut peer, id, &[UOT_ATYP_IPV4]).await;
    let conn1 = Arc::clone(&conn);
    let first = tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        conn1.read_packet(&mut buf).await
    });
    tokio::time::sleep(Duration::from_millis(10)).await;
    // read2 parks on the mutex behind it.
    let conn2 = Arc::clone(&conn);
    let queued = tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        conn2.read_packet(&mut buf).await
    });
    tokio::time::sleep(Duration::from_millis(10)).await;
    first.abort();
    // A trailing datagram keeps a regressed implementation honest:
    // without the post-lock re-check, read2 would parse these bytes
    // as the start of a fresh frame and *succeed* — the assert below
    // fails — rather than parking forever on an empty stream.
    push_frame(
        &mut peer,
        id,
        &[UOT_ATYP_IPV4, 9, 9, 9, 9, 0, 53, 0, 1, b'x'],
    )
    .await;
    let err = tokio::time::timeout(Duration::from_secs(2), queued)
        .await
        .expect("queued read must resolve, not hang")
        .unwrap()
        .unwrap_err();
    assert!(
        err.to_string().contains("desynced"),
        "queued read must fail fast after the poisoned first read, got {err:?}"
    );
    session.close().await.unwrap();
}

/// A read cancelled while parked on the reader mutex consumed no
/// bytes — it must NOT poison the conn. Pins the invariant that the
/// guard arms only after the lock is held.
#[tokio::test]
async fn udp_parked_read_cancellation_does_not_poison() {
    let (session, mut peer) = test_session().await;
    let (stream, _) = session.open_stream().await.unwrap();
    let id = stream.id();
    assert_eq!(wire_frame(&mut peer).await.0, Command::Syn);
    let conn = Arc::new(AnytlsPacketConn::new(stream, Arc::clone(&session), None));

    // read1 stalls mid-addr holding the lock; read2 parks behind it.
    push_frame(&mut peer, id, &[UOT_ATYP_IPV4]).await;
    let conn1 = Arc::clone(&conn);
    let first = tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        conn1.read_packet(&mut buf).await
    });
    tokio::time::sleep(Duration::from_millis(10)).await;
    let conn2 = Arc::clone(&conn);
    let parked = tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        conn2.read_packet(&mut buf).await
    });
    tokio::time::sleep(Duration::from_millis(10)).await;
    parked.abort();
    let _ = parked.await;

    // read1's datagram completes cleanly once the rest lands, and the
    // conn still serves the following datagram — no poison.
    push_frame(&mut peer, id, &[9, 9, 9, 9, 0, 53, 0, 1, b'a']).await;
    tokio::time::timeout(Duration::from_secs(2), first)
        .await
        .expect("read1 must resolve, not hang")
        .unwrap()
        .unwrap();
    push_frame(
        &mut peer,
        id,
        &[UOT_ATYP_IPV4, 8, 8, 8, 8, 0, 53, 0, 1, b'b'],
    )
    .await;
    let mut buf = [0u8; 2048];
    tokio::time::timeout(Duration::from_secs(2), conn.read_packet(&mut buf))
        .await
        .expect("read after parked-cancel must resolve")
        .unwrap();
    session.close().await.unwrap();
}

/// The same poison fires on a parse error after bytes were consumed
/// (unknown atyp) — not only on future cancellation.
#[tokio::test]
async fn udp_errored_read_poisons_packet_conn() {
    let (session, mut peer) = test_session().await;
    let (stream, _) = session.open_stream().await.unwrap();
    let id = stream.id();
    assert_eq!(wire_frame(&mut peer).await.0, Command::Syn);
    let conn = AnytlsPacketConn::new(stream, Arc::clone(&session), None);
    let mut buf = [0u8; 2048];

    push_frame(&mut peer, id, &[0x7f]).await;
    let err = tokio::time::timeout(Duration::from_secs(2), conn.read_packet(&mut buf))
        .await
        .expect("errored read must resolve")
        .unwrap_err();
    assert!(err.to_string().contains("address type"), "got {err:?}");

    // The peer then sends a valid datagram — the conn must not
    // resume mid-stream.
    push_frame(
        &mut peer,
        id,
        &[UOT_ATYP_IPV4, 9, 9, 9, 9, 0, 53, 0, 1, b'x'],
    )
    .await;
    let err = tokio::time::timeout(Duration::from_secs(2), conn.read_packet(&mut buf))
        .await
        .expect("poisoned read must fail fast")
        .unwrap_err();
    assert!(
        err.to_string().contains("desynced"),
        "second read must fail fast on poisoned conn, got {err:?}"
    );
    session.close().await.unwrap();
}

/// A datagram larger than the caller buffer is truncated, not torn:
/// `read_packet`'s `length > to_copy` sink-drain keeps the stream
/// aligned so the next datagram still parses.
#[tokio::test]
async fn udp_undersized_buffer_truncates_without_desync() {
    let (session, mut peer) = test_session().await;
    let (stream, _) = session.open_stream().await.unwrap();
    let id = stream.id();
    assert_eq!(wire_frame(&mut peer).await.0, Command::Syn);
    let conn = AnytlsPacketConn::new(stream, Arc::clone(&session), None);
    let src: SocketAddr = "192.0.2.9:53".parse().unwrap();

    // 4-byte payload into a 2-byte buffer — 2 bytes must drain.
    let mut datagram = Vec::new();
    encode_uot_addr(&mut datagram, &src);
    datagram.extend_from_slice(&4u16.to_be_bytes());
    datagram.extend_from_slice(&[1, 2, 3, 4]);
    push_frame(&mut peer, id, &datagram).await;

    let mut buf = [0u8; 2];
    let (n, addr) = tokio::time::timeout(Duration::from_secs(2), conn.read_packet(&mut buf))
        .await
        .expect("truncated read must resolve")
        .unwrap();
    assert_eq!(addr, src);
    assert_eq!(&buf[..n], &[1, 2]);

    // The following datagram still parses — the drain kept framing.
    let mut datagram2 = Vec::new();
    encode_uot_addr(&mut datagram2, &src);
    datagram2.extend_from_slice(&1u16.to_be_bytes());
    datagram2.extend_from_slice(&[9]);
    push_frame(&mut peer, id, &datagram2).await;

    let mut buf2 = [0u8; 2048];
    let (n2, addr2) = tokio::time::timeout(Duration::from_secs(2), conn.read_packet(&mut buf2))
        .await
        .expect("post-truncation read must resolve")
        .unwrap();
    assert_eq!(addr2, src);
    assert_eq!(&buf2[..n2], &[9]);
    session.close().await.unwrap();
}

/// A drain larger than one 8 KiB sink pass: the loop must iterate and
/// still leave framing aligned (issue #621 review — single-pass drains
/// alone don't prove the loop).
#[tokio::test]
async fn udp_oversized_drain_iterates_sink() {
    let (session, mut peer) = test_session().await;
    let (stream, _) = session.open_stream().await.unwrap();
    let id = stream.id();
    assert_eq!(wire_frame(&mut peer).await.0, Command::Syn);
    let conn = AnytlsPacketConn::new(stream, Arc::clone(&session), None);
    let src: SocketAddr = "192.0.2.9:53".parse().unwrap();

    // 20 KiB payload into a 1-byte buffer → 19999 bytes drained over
    // three 8 KiB sink passes.
    let mut datagram = Vec::new();
    encode_uot_addr(&mut datagram, &src);
    datagram.extend_from_slice(&20000u16.to_be_bytes());
    datagram.extend_from_slice(&vec![0xAB; 20000]);
    push_frame(&mut peer, id, &datagram).await;

    let mut buf = [0u8; 1];
    let (n, addr) = tokio::time::timeout(Duration::from_secs(2), conn.read_packet(&mut buf))
        .await
        .expect("oversized read must resolve")
        .unwrap();
    assert_eq!(addr, src);
    assert_eq!(&buf[..n], &[0xAB]);

    let mut datagram2 = Vec::new();
    encode_uot_addr(&mut datagram2, &src);
    datagram2.extend_from_slice(&2u16.to_be_bytes());
    datagram2.extend_from_slice(&[7, 7]);
    push_frame(&mut peer, id, &datagram2).await;

    let mut buf2 = [0u8; 2048];
    let (n2, _) = tokio::time::timeout(Duration::from_secs(2), conn.read_packet(&mut buf2))
        .await
        .expect("post-drain read must resolve")
        .unwrap();
    assert_eq!(&buf2[..n2], &[7, 7]);
    session.close().await.unwrap();
}

/// Happy-path guard: a completed datagram read must not poison.
#[tokio::test]
async fn udp_completed_read_leaves_conn_usable() {
    let (session, mut peer) = test_session().await;
    let (stream, _) = session.open_stream().await.unwrap();
    let id = stream.id();
    assert_eq!(wire_frame(&mut peer).await.0, Command::Syn);
    let conn = AnytlsPacketConn::new(stream, Arc::clone(&session), None);
    let mut buf = [0u8; 2048];
    let src: SocketAddr = "192.0.2.9:53".parse().unwrap();

    for i in 0u8..2 {
        // uot addr + u16 len + payload, exactly what the server sends.
        let mut datagram = Vec::new();
        encode_uot_addr(&mut datagram, &src);
        datagram.extend_from_slice(&3u16.to_be_bytes());
        datagram.extend_from_slice(&[i, i, i]);
        push_frame(&mut peer, id, &datagram).await;

        let (n, addr) = tokio::time::timeout(Duration::from_secs(2), conn.read_packet(&mut buf))
            .await
            .expect("completed read must resolve")
            .unwrap();
        assert_eq!(addr, src);
        assert_eq!(&buf[..n], &[i, i, i]);
    }
    session.close().await.unwrap();
}

#[tokio::test]
async fn udp_cancelled_admission_preserves_close_order() {
    let (session, mut peer) = test_session().await;
    let (stream, _) = session.open_stream().await.unwrap();
    let id = stream.id();
    assert_eq!(wire_frame(&mut peer).await.0, Command::Syn);
    // Fill the session budget while the first frame blocks on the wire.
    for _ in 0..64 {
        stream.send_data(Bytes::from(vec![42; 512])).await.unwrap();
    }
    let conn = AnytlsPacketConn::new(stream, Arc::clone(&session), None);
    let destination: SocketAddr = "192.0.2.2:53".parse().unwrap();
    let mut write = Box::pin(conn.write_packet(b"cancelled", &destination));
    assert!(futures::poll!(&mut write).is_pending());
    drop(write);

    let mut write = Box::pin(conn.write_packet(b"closed", &destination));
    assert!(futures::poll!(&mut write).is_pending());
    conn.close().unwrap();
    assert!(write.await.is_err(), "close must wake blocked admission");
    for _ in 0..64 {
        assert_eq!(
            wire_frame(&mut peer).await,
            (Command::Push, id, vec![42; 512])
        );
    }
    assert_eq!(wire_frame(&mut peer).await, (Command::Fin, id, vec![]));
    session.close().await.unwrap();
}

/// The request header uses SOCKS5 family bytes, *not* the uot ones.
#[test]
fn request_encodes_socks5_family_bytes() {
    let v4 = encode_uot_request(&metadata_for("", Some("8.8.8.8".parse().unwrap()), 53));
    assert_eq!(v4, vec![0, SOCKS5_ATYP_IPV4, 8, 8, 8, 8, 0, 53]);

    let v6 = encode_uot_request(&metadata_for(
        "",
        Some("2001:4860:4860::8888".parse().unwrap()),
        53,
    ));
    assert_eq!(v6[0], 0, "isConnect must be false (Bind format)");
    assert_eq!(v6[1], SOCKS5_ATYP_IPV6);
    assert_eq!(v6.len(), 1 + 1 + 16 + 2);

    let domain = encode_uot_request(&metadata_for("example.com", None, 443));
    assert_eq!(domain[0], 0);
    assert_eq!(domain[1], SOCKS5_ATYP_DOMAIN);
    assert_eq!(domain[2], 11);
    assert_eq!(&domain[3..14], b"example.com");
    assert_eq!(u16::from_be_bytes([domain[14], domain[15]]), 443);
}

/// A resolved IPv4-mapped destination goes out as plain IPv4, matching
/// upstream's `Socksaddr` normalization.
#[test]
fn request_unmaps_v4_mapped_destination() {
    let mapped = encode_uot_request(&metadata_for(
        "",
        Some("::ffff:1.2.3.4".parse().unwrap()),
        8080,
    ));
    assert_eq!(mapped, vec![0, SOCKS5_ATYP_IPV4, 1, 2, 3, 4, 0x1f, 0x90]);
}

/// Metadata with neither an IP nor a host must still produce an address
/// the upstream serializer accepts.
#[test]
fn request_falls_back_to_unspecified_without_address() {
    let empty = encode_uot_request(&metadata_for("", None, 1234));
    assert_eq!(empty, vec![0, SOCKS5_ATYP_IPV4, 0, 0, 0, 0, 0x04, 0xd2]);
}
