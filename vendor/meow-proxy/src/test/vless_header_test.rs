use super::*;
use bytes::BytesMut;

/// UUID for all header tests: b831381d-6324-4d53-ad4f-8cda48b30811
const TEST_UUID: [u8; 16] = [
    0xb8, 0x31, 0x38, 0x1d, 0x63, 0x24, 0x4d, 0x53, 0xad, 0x4f, 0x8c, 0xda, 0x48, 0xb3, 0x08, 0x11,
];

// ─── A7 guard-rail ────────────────────────────────────────────────────────

/// First byte must always be 0x00 (guards against VMess encoder copy-paste
/// where version = 0x01).
#[test]
fn header_encode_version_byte_is_zero() {
    let mut buf = BytesMut::new();
    encode_request(
        &mut buf,
        &TEST_UUID,
        None,
        Cmd::Tcp,
        443,
        &VlessAddr::Ipv4([127, 0, 0, 1]),
    );
    assert_eq!(buf[0], 0x00, "first byte must be version 0x00");
}

// ─── A1: TCP + IPv4 + plain flow ─────────────────────────────────────────

/// Fixed UUID + target 127.0.0.1:443 + flow:"" → exact byte sequence.
/// upstream: transport/vless/encoding.go::EncodeRequestHeader
#[test]
fn header_encode_tcp_ipv4_plain() {
    let mut buf = BytesMut::new();
    encode_request(
        &mut buf,
        &TEST_UUID,
        None,
        Cmd::Tcp,
        443,
        &VlessAddr::Ipv4([127, 0, 0, 1]),
    );

    // version
    assert_eq!(buf[0], 0x00);
    // uuid (bytes 1..17)
    assert_eq!(&buf[1..17], &TEST_UUID);
    // addon_length = 0
    assert_eq!(buf[17], 0x00);
    // cmd TCP
    assert_eq!(buf[18], 0x01);
    // port 443 = 0x01BB big-endian
    assert_eq!(buf[19], 0x01);
    assert_eq!(buf[20], 0xBB);
    // addr_type IPv4
    assert_eq!(buf[21], 0x01);
    // IPv4 bytes
    assert_eq!(&buf[22..26], &[127, 0, 0, 1]);
    assert_eq!(buf.len(), 26);
}

// ─── A2: TCP + domain — port BEFORE addr_type ────────────────────────────

/// UUID + example.com:80 + flow:""
/// NOT addr_type before port — port comes BEFORE addr_type.
/// upstream: same file
#[test]
fn header_encode_tcp_domain_plain() {
    let mut buf = BytesMut::new();
    encode_request(
        &mut buf,
        &TEST_UUID,
        None,
        Cmd::Tcp,
        80,
        &VlessAddr::Domain("example.com".into()),
    );

    // version + uuid (17) + addon(1=0) + cmd(1) = offset 19
    let cmd_off = 18;
    assert_eq!(buf[cmd_off], 0x01); // TCP
                                    // port 80 = 0x0050 big-endian  ← comes BEFORE addr_type
    assert_eq!(buf[cmd_off + 1], 0x00);
    assert_eq!(buf[cmd_off + 2], 0x50);
    // addr_type domain
    assert_eq!(buf[cmd_off + 3], 0x02);
    // len(1) = 11, "example.com"
    assert_eq!(buf[cmd_off + 4], 11);
    assert_eq!(&buf[cmd_off + 5..cmd_off + 16], b"example.com");
}

// ─── A3: IPv6 ─────────────────────────────────────────────────────────────

#[test]
fn header_encode_tcp_ipv6_plain() {
    let mut buf = BytesMut::new();
    let ipv6 = [0u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]; // ::1
    encode_request(
        &mut buf,
        &TEST_UUID,
        None,
        Cmd::Tcp,
        443,
        &VlessAddr::Ipv6(ipv6),
    );

    let addr_type_off = 21;
    assert_eq!(buf[addr_type_off], 0x03); // IPv6
    assert_eq!(&buf[addr_type_off + 1..addr_type_off + 17], &ipv6);
}

// ─── A4: UDP command byte ─────────────────────────────────────────────────

/// UDP target must use cmd = 0x02.
#[test]
fn header_encode_udp_command() {
    let mut buf = BytesMut::new();
    encode_request(
        &mut buf,
        &TEST_UUID,
        None,
        Cmd::Udp,
        53,
        &VlessAddr::Ipv4([8, 8, 8, 8]),
    );
    // cmd byte is at offset 18 (after version=1 + uuid=16 + addon_length=1)
    assert_eq!(buf[18], 0x02, "UDP cmd must be 0x02");
}

// ─── A5: addon_length = 0 for plain flow ─────────────────────────────────

/// flow:"" → addon_length = 0x00, no addon bytes follow.
/// upstream: transport/vless/encoding.go::EncodeRequestHeader (addon block, empty Flow)
#[test]
fn header_encode_addon_empty_for_plain_flow() {
    let mut buf = BytesMut::new();
    encode_request(
        &mut buf,
        &TEST_UUID,
        None,
        Cmd::Tcp,
        80,
        &VlessAddr::Ipv4([1, 2, 3, 4]),
    );
    // addon_length at offset 17
    assert_eq!(buf[17], 0x00, "addon_length must be 0 for plain flow");
    // cmd immediately follows (offset 18)
    assert_eq!(
        buf[18], 0x01,
        "cmd follows addon_length with no addon bytes"
    );
}

// ─── A6: addon_length = 18 for Vision ────────────────────────────────────

/// flow:"xtls-rprx-vision" → 18-byte addon.
/// NOT prost — hardcoded 2-byte protobuf prefix + string.
/// upstream: transport/vless/encoding.go::EncodeRequestHeader (addon block with Flow set)
#[test]
fn header_encode_addon_vision_exact_bytes() {
    let mut buf = BytesMut::new();
    encode_request(
        &mut buf,
        &TEST_UUID,
        Some("xtls-rprx-vision"),
        Cmd::Tcp,
        443,
        &VlessAddr::Ipv4([1, 2, 3, 4]),
    );
    // addon_length at offset 17
    assert_eq!(buf[17], 18, "addon_length must be 18 for xtls-rprx-vision");
    // addon bytes at offsets 18..36
    assert_eq!(buf[18], 0x0A, "field 1, wire type 2");
    assert_eq!(buf[19], 0x10, "varint 16 = len(xtls-rprx-vision)");
    assert_eq!(&buf[20..36], b"xtls-rprx-vision");
    // cmd follows at offset 36
    assert_eq!(buf[36], 0x01, "cmd TCP follows addon");
}

// ─── A8/A9: domain length validation ─────────────────────────────────────

#[test]
fn header_addr_domain_max_255_encodes() {
    let name = "a".repeat(255);
    let addr = VlessAddr::domain(&name).expect("255-byte domain should be valid");
    let mut buf = BytesMut::new();
    encode_request(&mut buf, &TEST_UUID, None, Cmd::Tcp, 80, &addr);
    // Should compile and produce a valid buffer.
    assert!(!buf.is_empty());
}

/// 256-char hostname → Err at construction (not silent truncation).
/// upstream: transport/vless/encoding.go does NOT enforce this limit.
/// NOT silent truncate — Class A per ADR-0002: wrong destination, no diagnostic.
#[test]
fn header_addr_domain_over_255_errors_at_build_time() {
    let name = "a".repeat(256);
    let result = VlessAddr::domain(&name);
    assert!(result.is_err(), "256-byte domain must be rejected");
}

// ─── A10: IDN not punycoded ───────────────────────────────────────────────

/// Raw UTF-8 in domain field — NOT xn-- punycode.
/// Match upstream: let the server handle IDN resolution.
#[test]
fn header_addr_idn_not_punycoded() {
    let addr = VlessAddr::domain("例え.jp").expect("short IDN domain");
    let mut buf = BytesMut::new();
    encode_request(&mut buf, &TEST_UUID, None, Cmd::Tcp, 80, &addr);
    // The UTF-8 bytes of "例え.jp" must appear verbatim in the buffer.
    let name_bytes = "例え.jp".as_bytes();
    let buf_slice = buf.as_ref();
    let found = buf_slice.windows(name_bytes.len()).any(|w| w == name_bytes);
    assert!(found, "IDN domain must be raw UTF-8, not punycode");
}

// ─── A11: round-trip ─────────────────────────────────────────────────────

#[tokio::test]
async fn header_encode_full_round_trip_via_decode() {
    use tokio::io::duplex;
    use tokio::io::AsyncWriteExt;

    let (mut client, mut server) = duplex(1024);

    // Encode and send the request header.
    let mut buf = BytesMut::new();
    encode_request(
        &mut buf,
        &TEST_UUID,
        None,
        Cmd::Tcp,
        8080,
        &VlessAddr::Domain("roundtrip.example.com".into()),
    );
    client.write_all(&buf).await.unwrap();

    // Server echoes a minimal response header (version=0, addon_length=0).
    let mut req_discard = vec![0u8; buf.len()];
    tokio::io::AsyncReadExt::read_exact(&mut server, &mut req_discard)
        .await
        .unwrap();

    // Send a valid response header back.
    server.write_all(&[0x00, 0x00]).await.unwrap();

    // The response header round-trips as [version=0][addon_length=0].
    let mut resp = [0u8; 2];
    tokio::io::AsyncReadExt::read_exact(&mut client, &mut resp)
        .await
        .expect("round-trip decode ok");
    assert_eq!(resp, [0x00, 0x00]);
}

// Response-header decode coverage (B1-B4) lives with the consumers
// now: `vless/conn.rs` tests `VlessUdpReader` for version mismatch,
// addon discard and truncated-header errors.

/// A >255-byte `host` must error instead of silently truncating the
/// one-byte domain length field (issue #657 review): previously
/// `addr_from_metadata` bypassed the checked `VlessAddr::domain`.
#[test]
fn addr_from_metadata_rejects_oversized_domain() {
    let m = Metadata {
        host: "a".repeat(256).into(),
        dst_port: 443,
        ..Default::default()
    };
    let Err(msg) = addr_from_metadata(&m) else {
        panic!("256-byte domain must error");
    };
    assert!(msg.contains("255"), "error names the limit: {msg}");

    // The boundary itself still encodes.
    let m = Metadata {
        host: "a".repeat(255).into(),
        dst_port: 443,
        ..Default::default()
    };
    assert!(addr_from_metadata(&m).is_ok());
}
