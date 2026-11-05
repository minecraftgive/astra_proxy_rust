use super::*;

#[test]
fn packet_header_encodes_uot_family_bytes() {
    let mut buf = Vec::new();
    encode_uot_addr(&mut buf, &"1.2.3.4:80".parse().unwrap());
    assert_eq!(buf, vec![UOT_ATYP_IPV4, 1, 2, 3, 4, 0, 80]);

    let mut buf = Vec::new();
    encode_uot_addr(&mut buf, &"[::1]:80".parse().unwrap());
    assert_eq!(buf[0], UOT_ATYP_IPV6);
    assert_eq!(buf.len(), 1 + 16 + 2);

    // v4-mapped peers are unmapped, as upstream does before serializing.
    let mut buf = Vec::new();
    encode_uot_addr(&mut buf, &"[::ffff:1.2.3.4]:80".parse().unwrap());
    assert_eq!(buf, vec![UOT_ATYP_IPV4, 1, 2, 3, 4, 0, 80]);
}

#[tokio::test]
async fn read_uot_addr_round_trips_encode() {
    for addr in ["1.2.3.4:53", "[2001:db8::1]:443"] {
        let addr: SocketAddr = addr.parse().unwrap();
        let mut buf = Vec::new();
        encode_uot_addr(&mut buf, &addr);
        let mut reader: &[u8] = &buf;
        assert_eq!(read_uot_addr(&mut reader).await.unwrap(), addr);
    }
}

/// The domain write form (`UdpTarget::Name`, issue #657) round-trips
/// through the parser's FQDN branch — which degrades a non-literal to
/// `0.0.0.0:<port>` — and an IP-literal host survives as an address.
#[tokio::test]
async fn encode_uot_addr_domain_wire_layout() {
    let mut buf = Vec::new();
    encode_uot_addr_domain(&mut buf, "back.internal", 8388);
    assert_eq!(
        buf[..2],
        [UOT_ATYP_DOMAIN, 13],
        "atyp + one-byte host length"
    );
    assert_eq!(&buf[2..15], b"back.internal");
    assert_eq!(&buf[15..], &8388u16.to_be_bytes());

    // Parser accepts the form (degrades non-literal to unspecified+port).
    let mut reader: &[u8] = &buf;
    assert_eq!(
        read_uot_addr(&mut reader).await.unwrap(),
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 8388)
    );

    let mut buf = Vec::new();
    encode_uot_addr_domain(&mut buf, "203.0.113.9", 5300);
    let mut reader: &[u8] = &buf;
    assert_eq!(
        read_uot_addr(&mut reader).await.unwrap(),
        "203.0.113.9:5300".parse().unwrap(),
        "IP-literal domain form round-trips to the literal"
    );
}

/// Domain-form replies degrade to `0.0.0.0:<port>` unless the "domain" is
/// an IP literal — same best-effort rule as `trojan.rs`.
#[tokio::test]
async fn read_uot_addr_handles_domain_form() {
    let mut wire = vec![UOT_ATYP_DOMAIN, 11];
    wire.extend_from_slice(b"example.com");
    wire.extend_from_slice(&443u16.to_be_bytes());
    let mut reader: &[u8] = &wire;
    assert_eq!(
        read_uot_addr(&mut reader).await.unwrap(),
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 443)
    );

    let mut wire = vec![UOT_ATYP_DOMAIN, 7];
    wire.extend_from_slice(b"1.2.3.4");
    wire.extend_from_slice(&53u16.to_be_bytes());
    let mut reader: &[u8] = &wire;
    assert_eq!(
        read_uot_addr(&mut reader).await.unwrap(),
        "1.2.3.4:53".parse::<SocketAddr>().unwrap()
    );
}

#[tokio::test]
async fn read_uot_addr_rejects_unknown_family() {
    let mut reader: &[u8] = &[0x09, 0, 0, 0, 0, 0, 0];
    let Err(err) = read_uot_addr(&mut reader).await else {
        panic!("unknown address family must error");
    };
    assert!(err.to_string().contains("unknown address type"), "{err}");
}
