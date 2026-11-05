use super::*;
use std::net::Ipv6Addr;

#[test]
fn ipv4_encoding_matches_sing_layout() {
    let bytes = encode_address("10.0.2.2", 18081).unwrap();
    assert_eq!(bytes, [0x01, 10, 0, 2, 2, 0x46, 0xa1]);
}

#[test]
fn stream_request_tcp_flags_then_socksaddr() {
    let bytes = encode_stream_request("10.0.2.2", 18081).unwrap();
    assert_eq!(bytes, [0x00, 0x00, 0x01, 10, 0, 2, 2, 0x46, 0xa1]);
}

#[test]
fn stream_request_udp_flag_matches_sing_mux() {
    // flagUDP = 1 (sing-mux protocol.go).
    let bytes = encode_stream_request_with_flags("10.0.2.2", 18081, 1).unwrap();
    assert_eq!(bytes[0], 0x00);
    assert_eq!(bytes[1], 0x01);
}

#[test]
fn ipv6_encoding() {
    let ip: Ipv6Addr = "2001:db8::1".parse().unwrap();
    let bytes = encode_address("2001:db8::1", 443).unwrap();
    assert_eq!(bytes[0], TYPE_IPV6);
    assert_eq!(&bytes[1..17], &ip.octets());
    assert_eq!(u16::from_be_bytes([bytes[17], bytes[18]]), 443);
}

#[test]
fn fqdn_encoding() {
    let bytes = encode_address("sp.mux.sing-box.arpa", 444).unwrap();
    assert_eq!(bytes[0], TYPE_FQDN);
    assert_eq!(bytes[1] as usize, "sp.mux.sing-box.arpa".len());
    assert_eq!(
        &bytes[2..2 + "sp.mux.sing-box.arpa".len()],
        b"sp.mux.sing-box.arpa"
    );
    let port_off = 2 + "sp.mux.sing-box.arpa".len();
    assert_eq!(
        u16::from_be_bytes([bytes[port_off], bytes[port_off + 1]]),
        444
    );
}

#[test]
fn overlong_fqdn_is_rejected_not_truncated() {
    let host = "a".repeat(256);
    let err = encode_address(&host, 443).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    let err = encode_stream_request_with_flags(&host, 443, 1).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn ipv4_mapped_ipv6_takes_ipv6_type() {
    // ::ffff:10.0.2.2 parses as an IPv6 literal and must encode as such.
    let bytes = encode_address("::ffff:10.0.2.2", 80).unwrap();
    assert_eq!(bytes[0], TYPE_IPV6);
    assert_eq!(bytes.len(), 1 + 16 + 2);
}
