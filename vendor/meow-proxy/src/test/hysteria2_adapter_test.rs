use super::*;

fn base_options() -> Hy2Options {
    Hy2Options {
        name: "hy2".into(),
        server: "example.com".into(),
        port: 443,
        password: "secret".into(),
        sni: None,
        skip_cert_verify: false,
        udp: true,
        up_bps: 0,
        down_bps: 0,
        obfs: None,
        obfs_password: None,
        ports: None,
        hop_interval: None,
        fingerprint: None,
        fast_open: true,
    }
}

#[test]
fn adapter_constructor_rejects_empty_password() {
    let options = Hy2Options {
        password: String::new(),
        ..base_options()
    };
    let Err(err) = Hy2Adapter::new(options) else {
        panic!("must fail");
    };
    assert!(err.contains("password must not be empty"));
}

#[test]
fn adapter_constructor_builds_hysteria_config() {
    let adapter = Hy2Adapter::new(Hy2Options {
        server: "127.0.0.1".into(),
        sni: Some("example.com".into()),
        up_bps: 10,
        down_bps: 20,
        obfs: Some(Hy2Obfs::Salamander),
        obfs_password: Some("obfs-secret".into()),
        ports: Some("443,8443".into()),
        hop_interval: Some(Hy2HopInterval {
            min_secs: 15,
            max_secs: 30,
        }),
        fingerprint: Some("00".repeat(32)),
        ..base_options()
    })
    .unwrap();
    assert_eq!(adapter.name(), "hy2");
    assert_eq!(adapter.addr(), "127.0.0.1:443");
    assert!(adapter.support_udp());
}

#[test]
fn target_prefers_host() {
    let md = Metadata {
        host: "example.com".into(),
        dst_ip: Some("127.0.0.1".parse().unwrap()),
        dst_port: 443,
        ..Default::default()
    };
    assert_eq!(target_from_metadata(&md).unwrap(), "example.com:443");
}

#[test]
fn target_formats_ipv6_socket_addr() {
    let md = Metadata {
        dst_ip: Some("::1".parse().unwrap()),
        dst_port: 443,
        ..Default::default()
    };
    assert_eq!(target_from_metadata(&md).unwrap(), "[::1]:443");
}

#[test]
fn target_brackets_ipv6_host_literal() {
    let md = Metadata {
        host: "::1".into(),
        dst_port: 443,
        ..Default::default()
    };
    assert_eq!(target_from_metadata(&md).unwrap(), "[::1]:443");
}

/// QUIC/UDP cannot ride a TCP stream, so hysteria2 is first-hop-only:
/// the trait-default `connect_over` must keep returning NotSupported.
#[tokio::test]
async fn connect_over_is_not_supported() {
    let adapter = Hy2Adapter::new(base_options()).unwrap();
    // The error surfaces before any stream IO — a dead listener suffices.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let metadata = Metadata::default();
    match adapter.connect_over(Box::new(upstream), &metadata).await {
        Err(MeowError::NotSupported(_)) => {}
        Err(other) => panic!("expected NotSupported, got {other:?}"),
        Ok(_) => panic!("hysteria2 must not support connect_over"),
    }
}

/// A chained `UdpTarget::Name` whose `host:port` exceeds
/// `MAX_ADDRESS_LENGTH` would be silently dropped by the datagram
/// driver while reporting success — the adapter refuses `NotSupported`
/// instead so the caller falls back to a literal target (issue #657
/// review). The guard runs before `udp()`, so no session is needed.
#[tokio::test]
async fn dial_udp_refuses_oversized_domain_target() {
    let adapter = Hy2Adapter::new(base_options()).unwrap();
    let meta = Metadata {
        network: meow_common::Network::Udp,
        // 2048-byte bound on "host:port": this host alone exceeds it.
        host: "a".repeat(2048).into(),
        dst_port: 443,
        ..Default::default()
    };
    match adapter.dial_udp(&meta).await {
        Err(MeowError::NotSupported(_)) => {}
        Err(other) => panic!("expected NotSupported, got {other:?}"),
        Ok(_) => panic!("oversized domain target must not associate"),
    }
}
