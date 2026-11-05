use super::*;

fn response(status: &[u8], udp: &[u8]) -> Vec<u8> {
    let mut encoded = [0; 1024];
    let n = qpack::Encoder::new()
        .encode(
            &[
                Header::new(b":status", status),
                Header::new(b"hysteria-udp", udp),
            ],
            &mut encoded,
        )
        .unwrap();
    let mut frame = vec![1];
    proto::put_varint(n as u64, &mut frame).unwrap();
    frame.extend_from_slice(&encoded[..n]);
    frame
}

#[test]
fn validates_auth_status_and_udp_header() {
    assert!(decode_response(&response(b"233", b"TRUE")).unwrap());
    assert!(!decode_response(&response(b"233", b"false")).unwrap());
    assert!(decode_response(&response(b"403", b"true")).is_err());
    assert!(decode_response(&[]).is_err());
}

#[test]
fn rejects_truncated_or_duplicate_auth_headers() {
    let mut bytes = response(b"233", b"true");
    for n in 0..bytes.len() {
        assert!(decode_response(&bytes[..n]).is_err());
    }
    bytes.extend_from_slice(&response(b"233", b"true"));
    assert!(decode_response(&bytes).is_err());
}

#[test]
fn handles_body_and_extensions_but_rejects_invalid_framing() {
    let mut bytes = response(b"233", b"true");
    bytes.extend_from_slice(&[0, 3, b'a', b'b', b'c']);
    bytes.extend_from_slice(&[0x21, 0]); // Unknown, empty extension frame.
    assert!(decode_response(&bytes).unwrap());
    assert!(decode_response(&[0, 0]).is_err()); // DATA before HEADERS.
    assert!(decode_response(&[1, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]).is_err());
    bytes.extend_from_slice(&[4, 0]); // SETTINGS belongs on the control stream.
    assert!(decode_response(&bytes).is_err());
    assert!(decode_response(&[1, 2, 1, 0]).is_err()); // Dynamic QPACK reference.
}
