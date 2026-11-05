use super::*;

impl App {
    async fn connect_resolved(
        &self,
        target: &Target,
        action: &RouteAction,
    ) -> Result<outbound::Stream> {
        self.connect_resolved_on(target, action, None).await
    }
}

#[tokio::test]
async fn tun_direct_keeps_loopback_and_rejects_unscoped_link_local() {
    let app = App::from_text(
        std::path::Path::new("tun-direct.toml"),
        "rule_socket='unused.sock'\nsocks5_listen='127.0.0.1:1080'\n[tun]\nname='test-only'\n"
            .into(),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = Target {
        host: "127.0.0.1".into(),
        port: listener.local_addr().unwrap().port(),
    };
    let _client = app
        .connect_resolved(&target, &RouteAction::Direct)
        .await
        .unwrap();
    let _server = listener.accept().await.unwrap();
    let link_local = Target {
        host: "fe80::123".into(),
        port: 53,
    };
    let result = app
        .connect_resolved(&link_local, &RouteAction::Direct)
        .await;
    match result {
        Err(error) => assert!(error.to_string().contains("tun.interface"), "{error:#}"),
        Ok(_) => panic!("unscoped link-local must be rejected before dialing"),
    }
}

#[tokio::test]
async fn tun_rejects_recaptured_proxy_connections_even_in_standalone_mode() {
    let app = App::from_text(
            std::path::Path::new("recapture.toml"),
            "standalone=true\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\nkind='socks5'\ntag='edge'\nserver='127.0.0.1:1081'\n".into(),
        ).unwrap();
    let target = Target {
        host: "fe80::123".into(),
        port: 53,
    };
    let own = ProcessInfo {
        pid: std::process::id(),
        name: "astra-proxy.exe".into(),
        path: "astra-proxy.exe".into(),
    };
    for network in [Network::Tcp, Network::Udp] {
        let result = app
            .route_with_process(&target, Inbound::Tun, network, Some(own.clone()))
            .await;
        assert!(
            result.is_err(),
            "recaptured {network:?} must not open another outbound"
        );
        assert!(result.unwrap_err().to_string().contains("recaptured"));
        let mut other = own.clone();
        other.pid = own.pid.wrapping_add(1);
        assert!(app
            .route_with_process(&target, Inbound::Tun, network, Some(other))
            .await
            .is_ok());
        // Explicit SOCKS use is not transparent recapture.
        assert!(app
            .route_with_process(&target, Inbound::Socks5, network, Some(own.clone()))
            .await
            .is_ok());
    }
    #[cfg(windows)]
    {
        // Exercise OS ownership lookup too: standalone TUN must not skip it.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let destination = listener.local_addr().unwrap();
        let client = tokio::net::TcpStream::connect(destination).await.unwrap();
        let (_server, _) = listener.accept().await.unwrap();
        let result = app
            .route_connection(
                &target,
                Inbound::Tun,
                Network::Tcp,
                Some(client.local_addr().unwrap()),
                Some(destination),
            )
            .await;
        assert!(result.unwrap_err().to_string().contains("recaptured"));
    }
}
