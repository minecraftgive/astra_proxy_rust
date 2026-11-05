use super::*;

#[test]
fn ip_unicast_if_value_is_the_index_in_network_byte_order() {
    assert_eq!(unicast_if_v4(5).to_ne_bytes(), [0, 0, 0, 5]);
    assert_eq!(unicast_if_v4(0x0102_0304).to_ne_bytes(), [1, 2, 3, 4]);
}

#[test]
fn loopback_peers_include_the_mapped_form() {
    for ip in ["127.0.0.1", "127.8.9.10", "::1", "::ffff:127.0.0.1"] {
        assert!(is_loopback(ip.parse().unwrap()), "{ip}");
    }
    for ip in [
        "192.0.2.1",
        "0.0.0.0",
        "::",
        "2001:db8::1",
        "::ffff:192.0.2.1",
    ] {
        assert!(!is_loopback(ip.parse().unwrap()), "{ip}");
    }
}
