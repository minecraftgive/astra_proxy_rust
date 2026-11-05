use super::*;

/// Plain and bracketed IP literals parse; names and malformed
/// brackets do not — the UDP demux fast path keys on exactly this
/// fold (issue #648).
#[test]
fn metadata_ip_literal_parses_plain_and_bracketed() {
    assert_eq!(
        metadata_ip_literal("203.0.113.7"),
        Some("203.0.113.7".parse().unwrap())
    );
    assert_eq!(
        metadata_ip_literal("2001:db8::1"),
        Some("2001:db8::1".parse().unwrap())
    );
    assert_eq!(
        metadata_ip_literal("[2001:db8::1]"),
        Some("2001:db8::1".parse().unwrap())
    );
    assert_eq!(metadata_ip_literal("example.com"), None);
    assert_eq!(metadata_ip_literal(""), None);
    assert_eq!(metadata_ip_literal("[2001:db8::1"), None);
    assert_eq!(metadata_ip_literal("203.0.113.7]"), None);
}

/// `swap_src_dst` exchanges exactly the source/destination tuple —
/// ip, port, geo-ip — and leaves host/process/inbound fields alone,
/// mirroring upstream `Metadata.SwapSrcDst` (there is no ASN pair to
/// swap; ASN rules range-match the IP itself).
#[test]
fn swap_src_dst_swaps_only_src_dst_pairs() {
    let mut m = Metadata {
        src_ip: Some("10.0.0.1".parse().unwrap()),
        dst_ip: Some("203.0.113.7".parse().unwrap()),
        src_port: 12345,
        dst_port: 443,
        src_geo_ip: vec!["CN".into()],
        dst_geo_ip: vec!["US".into()],
        host: "example.com".into(),
        process: "curl".into(),
        in_name: "mixed".into(),
        ..Default::default()
    };
    let before = m.clone();
    m.swap_src_dst();
    assert_eq!(m.src_ip, before.dst_ip);
    assert_eq!(m.dst_ip, before.src_ip);
    assert_eq!(m.src_port, before.dst_port);
    assert_eq!(m.dst_port, before.src_port);
    assert_eq!(m.src_geo_ip, before.dst_geo_ip);
    assert_eq!(m.dst_geo_ip, before.src_geo_ip);
    // Everything else is untouched.
    assert_eq!(m.host, before.host);
    assert_eq!(m.sniff_host, before.sniff_host);
    assert_eq!(m.process, before.process);
    assert_eq!(m.in_name, before.in_name);
    // A second swap restores the original tuple.
    m.swap_src_dst();
    assert_eq!(m.src_ip, before.src_ip);
    assert_eq!(m.dst_ip, before.dst_ip);
    assert_eq!(m.src_port, before.src_port);
    assert_eq!(m.dst_port, before.dst_port);
}
