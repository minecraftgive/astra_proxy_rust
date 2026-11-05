use super::*;

#[test]
fn udp_headers_reject_truncation_fragments_and_preserve_empty_payloads() {
    for host in ["127.0.0.1", "::1", "example.test"] {
        let expected = Target {
            host: host.into(),
            port: 53,
        };
        let mut packet = vec![0, 0, 0];
        crate::outbound::append_socks_address(&mut packet, &expected).unwrap();
        for size in 0..packet.len() {
            assert!(decode(&packet[..size]).is_err());
        }
        let (target, body) = decode(&packet).unwrap();
        assert_eq!(target, expected);
        assert!(body.is_empty());
        packet.extend_from_slice(b"payload");
        assert_eq!(decode(&packet).unwrap().1, b"payload");
        packet[2] = 1;
        assert!(decode(&packet).is_err());
        packet[2] = 0;
        packet[0] = 1;
        assert!(decode(&packet).is_err());
    }
}
