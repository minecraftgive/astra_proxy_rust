use super::*;
use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt};

const UUID: [u8; UUID_LEN] = [0x11; UUID_LEN];
const CLIENT_HELLO: [u8; 6] = [TLS_HANDSHAKE, TLS_MAJOR, 0x01, 0x00, 0x01, TLS_CLIENT_HELLO];
const APP_DATA: [u8; 7] = [
    TLS_APPLICATION_DATA,
    TLS_MAJOR,
    TLS_MAJOR,
    0x00,
    0x02,
    b'h',
    b'i',
];

/// A Vision conn over an in-memory pipe, which (like ws, gRPC or VLESS
/// encryption) cannot switch to raw writes.
fn vision_over_pipe() -> (VisionConn, tokio::io::DuplexStream) {
    let (client, server) = duplex(64 * 1024);
    let vless = VlessConn {
        inner: Box::new(client),
        response_pending: true,
        response_buf: [0; 2],
        response_pos: 0,
        response_addon: Vec::new(),
        header_pending: None,
        header_needs_flush: false,
    };
    (VisionConn::new(vless, UUID), server)
}

async fn read_frame(server: &mut tokio::io::DuplexStream, with_uuid: bool) -> (u8, Vec<u8>) {
    if with_uuid {
        let mut uuid = [0u8; UUID_LEN];
        server.read_exact(&mut uuid).await.unwrap();
        assert_eq!(uuid, UUID);
    }
    let mut head = [0u8; PADDING_HEADER_LEN - UUID_LEN];
    server.read_exact(&mut head).await.unwrap();
    let mut content = vec![0u8; u16::from_be_bytes([head[1], head[2]]) as usize];
    server.read_exact(&mut content).await.unwrap();
    let mut padding = vec![0u8; u16::from_be_bytes([head[3], head[4]]) as usize];
    server.read_exact(&mut padding).await.unwrap();
    (head[0], content)
}

/// The Vision request plus a server frame carrying the target's TLS 1.3
/// ServerHello, which arms DIRECT for the next app-data write.
async fn handshake_with_tls13_target(conn: &mut VisionConn, server: &mut tokio::io::DuplexStream) {
    conn.write_all(&CLIENT_HELLO).await.unwrap();
    let hello = tls13_server_hello(0x1301);
    let mut downlink = vec![0x00, 0x00];
    downlink.extend(build_padding_frame(
        COMMAND_PADDING_CONTINUE,
        Some(&UUID),
        &hello,
        true,
    ));
    server.write_all(&downlink).await.unwrap();
    let mut got = vec![0u8; hello.len()];
    conn.read_exact(&mut got).await.unwrap();
    assert_eq!(got, hello);
    assert!(conn.write_direct_enabled);
}

#[tokio::test]
async fn uplink_direct_needs_a_raw_capable_transport() {
    let (mut conn, _server) = vision_over_pipe();
    assert!(!conn.raw_write_capable);
    conn.write_seen_tls = true;
    conn.write_direct_enabled = true;

    conn.build_write_frame(&APP_DATA);
    let pending = conn.write_pending.take().unwrap();
    assert_eq!(pending.command, COMMAND_PADDING_END);
    assert!(pending.end_padding_after_drain);

    conn.raw_write_capable = true;
    conn.build_write_frame(&APP_DATA);
    let pending = conn.write_pending.take().unwrap();
    assert_eq!(pending.command, COMMAND_PADDING_DIRECT);
    assert!(pending.end_padding_after_drain);
}

/// Issue #495: a TLS 1.3 target over a transport that cannot write raw
/// used to fail the first app-data write with "DIRECT requested but
/// transport cannot switch".  It must end padding and carry on.
#[tokio::test]
async fn tls13_target_over_non_raw_transport_ends_padding() {
    let (mut conn, mut server) = vision_over_pipe();
    handshake_with_tls13_target(&mut conn, &mut server).await;

    conn.write_all(&APP_DATA).await.unwrap();
    conn.write_all(b"tail").await.unwrap();
    conn.flush().await.unwrap();

    assert_eq!(
        read_frame(&mut server, true).await,
        (COMMAND_PADDING_CONTINUE, CLIENT_HELLO.to_vec())
    );
    assert_eq!(
        read_frame(&mut server, false).await,
        (COMMAND_PADDING_END, APP_DATA.to_vec())
    );
    let mut tail = [0u8; 4];
    server.read_exact(&mut tail).await.unwrap();
    assert_eq!(&tail, b"tail");
}

#[tokio::test]
async fn downlink_direct_over_non_raw_transport_is_a_clear_error() {
    let (mut conn, mut server) = vision_over_pipe();
    handshake_with_tls13_target(&mut conn, &mut server).await;

    let direct = build_padding_frame(COMMAND_PADDING_DIRECT, None, &APP_DATA, true);
    server.write_all(&direct).await.unwrap();
    let mut got = [0u8; APP_DATA.len()];
    conn.read_exact(&mut got).await.unwrap();
    assert_eq!(got, APP_DATA);
    let err = conn.read(&mut [0u8; 8]).await.unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::Unsupported);
}

/// Vision has no end-of-stream frame: peers terminate by closing the
/// transport, possibly while the downlink is still padded.  Xray and
/// mihomo surface that as io.EOF; erroring here would make the relay
/// skip the graceful half-close (shutdown + linger) and drop the
/// inbound socket abruptly.
#[tokio::test]
async fn eof_between_padding_frames_is_a_clean_eof() {
    let (mut conn, mut server) = vision_over_pipe();
    let mut downlink = vec![0x00, 0x00];
    downlink.extend(build_padding_frame(
        COMMAND_PADDING_CONTINUE,
        Some(&UUID),
        b"payload",
        false,
    ));
    server.write_all(&downlink).await.unwrap();

    let mut got = [0u8; 7];
    conn.read_exact(&mut got).await.unwrap();
    assert_eq!(&got, b"payload");

    drop(server);
    assert_eq!(conn.read(&mut [0u8; 8]).await.unwrap(), 0);
}

/// A server that closes after the bare VLESS response header — before
/// any padding frame — is still a normal termination (born-dead conn).
#[tokio::test]
async fn eof_before_first_padding_frame_is_a_clean_eof() {
    let (mut conn, mut server) = vision_over_pipe();
    server.write_all(&[0x00, 0x00]).await.unwrap();
    drop(server);
    assert_eq!(conn.read(&mut [0u8; 8]).await.unwrap(), 0);
}

/// A FIN landing mid-frame likewise ends the stream cleanly: the bytes
/// already decoded are delivered and the next read observes EOF.
#[tokio::test]
async fn eof_mid_padded_content_is_a_clean_eof() {
    let (mut conn, mut server) = vision_over_pipe();
    let mut downlink = vec![0x00, 0x00];
    downlink.extend_from_slice(&UUID);
    downlink.push(COMMAND_PADDING_CONTINUE);
    downlink.extend_from_slice(&100u16.to_be_bytes()); // claims 100 B content
    downlink.extend_from_slice(&0u16.to_be_bytes()); // no padding
    downlink.extend_from_slice(b"only-7b");
    server.write_all(&downlink).await.unwrap();

    let mut got = [0u8; 7];
    conn.read_exact(&mut got).await.unwrap();
    assert_eq!(&got, b"only-7b");

    drop(server);
    assert_eq!(conn.read(&mut [0u8; 8]).await.unwrap(), 0);
}

/// Same for a FIN landing mid-header: the partial header bytes are
/// undeliverable framing, the conn still ends as a clean EOF.
#[tokio::test]
async fn eof_mid_padding_header_is_a_clean_eof() {
    let (mut conn, mut server) = vision_over_pipe();
    let mut downlink = vec![0x00, 0x00];
    downlink.extend_from_slice(&UUID);
    downlink.extend_from_slice(&[COMMAND_PADDING_CONTINUE, 0x00]); // 2 of 5 header bytes
    server.write_all(&downlink).await.unwrap();

    drop(server);
    assert_eq!(conn.read(&mut [0u8; 8]).await.unwrap(), 0);
}

/// And mid-padding-bytes: all content was delivered, the truncated
/// padding run still ends as clean EOF.
#[tokio::test]
async fn eof_mid_padding_bytes_is_a_clean_eof() {
    let (mut conn, mut server) = vision_over_pipe();
    let mut downlink = vec![0x00, 0x00];
    downlink.extend_from_slice(&UUID);
    downlink.push(COMMAND_PADDING_CONTINUE);
    downlink.extend_from_slice(&4u16.to_be_bytes()); // 4 B content
    downlink.extend_from_slice(&50u16.to_be_bytes()); // claims 50 B padding
    downlink.extend_from_slice(b"data");
    downlink.extend_from_slice(&[0u8; 20]); // only 20 of 50 padding bytes
    server.write_all(&downlink).await.unwrap();

    let mut got = [0u8; 4];
    conn.read_exact(&mut got).await.unwrap();
    assert_eq!(&got, b"data");

    drop(server);
    assert_eq!(conn.read(&mut [0u8; 8]).await.unwrap(), 0);
}

/// EOF is latched: further reads stay EOF without re-emitting the
/// mid-frame debug note.
#[tokio::test]
async fn eof_is_latched_for_repeated_reads() {
    let (mut conn, mut server) = vision_over_pipe();
    server.write_all(&[0x00, 0x00]).await.unwrap();
    drop(server);
    assert_eq!(conn.read(&mut [0u8; 8]).await.unwrap(), 0);
    assert_eq!(conn.read(&mut [0u8; 8]).await.unwrap(), 0);
}

#[test]
fn padding_frame_with_uuid_matches_mihomo_layout() {
    let content = b"\x16\x03\x01\x00\x01\x01";
    let frame = build_padding_frame(COMMAND_PADDING_CONTINUE, Some(&UUID), content, true);

    assert_eq!(&frame[..UUID_LEN], &UUID);
    assert_eq!(frame[UUID_LEN], COMMAND_PADDING_CONTINUE);

    let content_len = u16::from_be_bytes([frame[UUID_LEN + 1], frame[UUID_LEN + 2]]) as usize;
    let padding_len = u16::from_be_bytes([frame[UUID_LEN + 3], frame[UUID_LEN + 4]]) as usize;

    assert_eq!(content_len, content.len());
    assert_eq!(
        &frame[PADDING_HEADER_LEN..PADDING_HEADER_LEN + content.len()],
        content
    );
    assert_eq!(
        frame.len(),
        PADDING_HEADER_LEN + content.len() + padding_len
    );
    assert!(padding_len >= 900 - content.len());
    assert!(padding_len < 1400 - content.len());
}

#[test]
fn padding_frame_without_uuid_uses_short_header() {
    let content = b"abc";
    let frame = build_padding_frame(COMMAND_PADDING_END, None, content, false);

    assert_eq!(frame[0], COMMAND_PADDING_END);
    let content_len = u16::from_be_bytes([frame[1], frame[2]]) as usize;
    let padding_len = u16::from_be_bytes([frame[3], frame[4]]) as usize;

    assert_eq!(content_len, content.len());
    assert_eq!(&frame[5..8], content);
    assert_eq!(frame.len(), 5 + content.len() + padding_len);
    assert!(padding_len < 256);
}

#[test]
fn detects_client_hello_inside_vless_prefixed_payload() {
    let mut payload = vec![0u8; 56];
    payload.extend_from_slice(&[0x16, 0x03, 0x01, 0x00, 0x01, 0x01]);

    assert!(contains_tls_client_hello(&payload));
    assert!(!contains_tls_client_hello(b"GET / HTTP/1.1\r\n"));
}

#[test]
fn server_hello_filter_detects_tls13() {
    let mut filter = ServerHelloFilter::new();

    assert!(filter.observe(&tls13_server_hello(0x1301)));
}

#[test]
fn server_hello_filter_handles_fragmented_record_header() {
    let hello = tls13_server_hello(0x1301);
    let mut filter = ServerHelloFilter::new();

    assert!(!filter.observe(&hello[..3]));
    assert!(filter.observe(&hello[3..]));
}

#[test]
fn server_hello_filter_rejects_non_tls13_cipher() {
    let mut filter = ServerHelloFilter::new();

    assert!(!filter.observe(&tls13_server_hello(0xc02f)));
}

fn tls13_server_hello(cipher_suite: u16) -> Vec<u8> {
    let supported_versions = TLS13_SUPPORTED_VERSIONS_EXT;
    let mut body = Vec::new();
    body.extend_from_slice(&[0x03, 0x03]);
    body.extend_from_slice(&[0x42; 32]);
    body.push(0);
    body.extend_from_slice(&cipher_suite.to_be_bytes());
    body.push(0);
    body.extend_from_slice(&(supported_versions.len() as u16).to_be_bytes());
    body.extend_from_slice(&supported_versions);

    let mut handshake = Vec::new();
    handshake.push(TLS_SERVER_HELLO);
    handshake.extend_from_slice(&[
        ((body.len() >> 16) & 0xff) as u8,
        ((body.len() >> 8) & 0xff) as u8,
        (body.len() & 0xff) as u8,
    ]);
    handshake.extend_from_slice(&body);

    let mut record = Vec::new();
    record.extend_from_slice(&[
        TLS_HANDSHAKE,
        TLS_MAJOR,
        TLS_MAJOR,
        ((handshake.len() >> 8) & 0xff) as u8,
        (handshake.len() & 0xff) as u8,
    ]);
    record.extend_from_slice(&handshake);
    record
}
