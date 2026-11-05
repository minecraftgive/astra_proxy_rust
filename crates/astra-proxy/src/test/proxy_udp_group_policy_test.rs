use super::*;
#[tokio::test]
async fn association_preserves_datagrams_across_receive_buffer_reuse() {
    timeout(Duration::from_secs(5), async {
        let echo = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        allow_max_datagrams(&echo).unwrap();
        let destination = echo.local_addr().unwrap();
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        allow_max_datagrams(&socket).unwrap();
        let source = socket.local_addr().unwrap();
        let app = Arc::new(
            App::from_text(
                std::path::Path::new("udp-buffers.toml"),
                "standalone=true\nstandalone_outbound='DIRECT'\nsocks5_listen='127.0.0.1:1080'\n"
                    .into(),
            )
            .unwrap(),
        );
        let (mut control, server) = tokio::io::duplex(64);
        let worker = tokio::spawn(associate_stream(
            server,
            Target {
                host: source.ip().to_string(),
                port: source.port(),
            },
            app,
            source,
            "127.0.0.1:1080".parse().unwrap(),
        ));
        let mut reply = [0; 10];
        control.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply[..8], &[5, 0, 0, 1, 127, 0, 0, 1]);
        let relay = SocketAddr::new(source.ip(), u16::from_be_bytes([reply[8], reply[9]]));
        let target = Target {
            host: destination.ip().to_string(),
            port: destination.port(),
        };
        let mut received = vec![0; 65536];
        for (sequence, size) in [0, 32, MAX_PACKET - 10, 7, 0, 16384, 1]
            .into_iter()
            .enumerate()
        {
            let body = vec![sequence as u8; size];
            let mut packet = vec![0, 0, 0];
            crate::outbound::append_socks_address(&mut packet, &target).unwrap();
            packet.extend_from_slice(&body);
            socket.send_to(&packet, relay).await.unwrap();
            let (count, peer) = echo.recv_from(&mut received).await.unwrap();
            assert_eq!(&received[..count], body);
            echo.send_to(&received[..count], peer).await.unwrap();
            let (count, _) = socket.recv_from(&mut received).await.unwrap();
            let (reply_target, reply_body) = decode(&received[..count]).unwrap();
            assert_eq!(reply_target, target);
            assert_eq!(reply_body, body);
        }
        drop(control);
        worker.await.unwrap().unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn configured_group_switch_cancels_idle_udp_receive_and_prevents_more_packets() {
    let remote = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let destination = remote.local_addr().unwrap();
    let settings = |selected: &str| {
        format!("standalone=true\nstandalone_outbound='main'\nsocks5_listen='127.0.0.1:1080'\n[proxy_groups.main]\nmode='select'\nmembers=['DIRECT','REJECT']\nselected='{selected}'\ndisconnect_on_manual=true\n")
    };
    let app = App::from_text(std::path::Path::new("udp-groups.toml"), settings("DIRECT")).unwrap();
    let target = Target {
        host: destination.ip().to_string(),
        port: destination.port(),
    };
    let connection = open(
        &app,
        &target,
        Inbound::Socks5,
        "127.0.0.1:40000".parse().unwrap(),
        destination,
    )
    .await
    .unwrap();
    connection.send(b"before", destination).await.unwrap();
    let mut bytes = [0; 32];
    let (size, _) = remote.recv_from(&mut bytes).await.unwrap();
    assert_eq!(&bytes[..size], b"before");
    let mut next =
        App::from_text(std::path::Path::new("udp-groups.toml"), settings("REJECT")).unwrap();
    next.groups.inherit(&app, &next.config);
    next.groups.committed(&app, &next.config);
    assert!(timeout(Duration::from_secs(1), connection.recv(&mut bytes))
        .await
        .unwrap()
        .is_err());
    assert!(connection.send(b"after", destination).await.is_err());
    assert!(
        timeout(Duration::from_millis(20), remote.recv_from(&mut bytes))
            .await
            .is_err()
    );
}
