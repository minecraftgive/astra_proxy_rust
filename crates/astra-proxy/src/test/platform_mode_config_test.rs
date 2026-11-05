use super::*;

#[test]
fn wildcard_listeners_use_matching_loopback_family_for_os_proxy() {
    for (bind, expected) in [
        ("0.0.0.0:8080", "127.0.0.1:8080"),
        ("[::]:8080", "[::1]:8080"),
        ("127.0.0.1:8080", "127.0.0.1:8080"),
        ("[::1]:8080", "[::1]:8080"),
    ] {
        assert_eq!(local_proxy_address(bind).unwrap().to_string(), expected);
    }
    for invalid in ["0.0.0.0:0", "[::]:0", "127.0.0.1:0", "192.0.2.1:8080"] {
        assert!(local_proxy_address(invalid).is_err(), "{invalid}");
    }
}

#[test]
fn validation_and_os_proxy_share_the_same_listener_resolution() {
    let config: crate::config::ProxyConfig = toml::from_str(
        "socks5_listen='0.0.0.0:1080'\nhttp_listen='0.0.0.0:8080'\n[system_proxy]\n",
    )
    .unwrap();
    validate(&config).unwrap();
    let expected_port = 8080;
    let target = system_proxy_address(&config).unwrap();
    assert_eq!(target.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
    assert_eq!(target.port(), expected_port);
    assert_eq!(config.http_listen.as_deref(), Some("0.0.0.0:8080"));
}

#[test]
fn mixed_port_is_preferred_for_all_system_proxy_protocols() {
    let config = toml::from_str("mixed_listen='0.0.0.0:7890'\nhttp_listen='127.0.0.1:8080'\nsocks5_listen='127.0.0.1:1080'\n").unwrap();
    let (http, socks) = system_proxy_addresses(&config).unwrap();
    assert_eq!(http.unwrap().to_string(), "127.0.0.1:7890");
    assert_eq!(http, socks);
}

#[tokio::test]
async fn mapped_target_reaches_the_wildcard_listener() {
    let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    let target = local_proxy_address(&listener.local_addr().unwrap().to_string()).unwrap();
    let _client = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tokio::net::TcpStream::connect(target),
    )
    .await
    .unwrap()
    .unwrap();
    let (_, peer) = listener.accept().await.unwrap();
    assert!(peer.ip().is_loopback());
}
