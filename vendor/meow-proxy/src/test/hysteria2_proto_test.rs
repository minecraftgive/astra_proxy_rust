use super::*;

#[test]
fn quic_varint_round_trip_boundaries() {
    for value in [0, 63, 64, 16_383, 16_384, 1_073_741_823, 1_073_741_824] {
        let mut encoded = Vec::new();
        put_varint(value, &mut encoded).unwrap();
        let mut pos = 0;
        assert_eq!(read_varint(&encoded, &mut pos).unwrap(), value);
        assert_eq!(pos, encoded.len());
    }
}

#[test]
fn tcp_request_layout_starts_with_frame_type() {
    let encoded = encode_tcp_request("example.com:443", b"hello").unwrap();
    let mut pos = 0;
    assert_eq!(
        read_varint(&encoded, &mut pos).unwrap(),
        FRAME_TYPE_TCP_REQUEST
    );
    assert_eq!(read_varint(&encoded, &mut pos).unwrap(), 15);
    assert_eq!(&encoded[pos..pos + 15], b"example.com:443");
}

#[test]
fn tcp_response_parse_needs_more_then_completes() {
    // status=0, msg_len=0, pad_len=3, pad="abc"
    let full = [0u8, 0u8, 3u8, b'a', b'b', b'c'];
    assert_eq!(parse_tcp_response(&full[..1]).unwrap(), None);
    assert_eq!(parse_tcp_response(&full[..5]).unwrap(), None);
    assert_eq!(parse_tcp_response(&full).unwrap(), Some(6));
}

#[test]
fn tcp_response_error_status_is_reported() {
    // status=1 (error), msg_len=2 "no", pad_len=0
    let full = [1u8, 2u8, b'n', b'o', 0u8];
    assert!(parse_tcp_response(&full).is_err());
}

#[test]
fn udp_message_round_trip() {
    let message = UdpMessage {
        session_id: 7,
        packet_id: 9,
        frag_id: 1,
        frag_count: 2,
        addr: "127.0.0.1:53".into(),
        data: b"payload".to_vec(),
    };
    let encoded = encode_udp_message(&message).unwrap();
    assert_eq!(decode_udp_message(&encoded).unwrap(), message);
}
