use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[tokio::test]
async fn fast_peer_results_stream_before_stalled_first_node_and_collected_order_is_stable() {
    let slow = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = slow.local_addr().unwrap();
    let quick = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let quick_address = quick.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = slow.accept().await.unwrap();
        let mut greeting = [0; 3];
        socket.read_exact(&mut greeting).await.unwrap();
        assert!(socket.read_u8().await.is_err());
    });
    let fast = tokio::spawn(async move {
        let (mut socket, _) = quick.accept().await.unwrap();
        let mut greeting = [0; 3];
        socket.read_exact(&mut greeting).await.unwrap();
        socket.write_all(&[5, 0xff]).await.unwrap();
    });
    let app = App::from_text(std::path::Path::new("stream.toml"),format!("standalone=true\nstandalone_outbound='slow'\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\nkind='socks5'\ntag='slow'\nserver='{address}'\n[[outbounds]]\nkind='socks5'\ntag='fast'\nserver='{quick_address}'\n")).unwrap();
    let events = std::sync::Mutex::new(Vec::new());
    let start = Instant::now();
    let report = selected_stream(&app, &["slow".into(), "fast".into()], |event| {
        events.lock().unwrap().push((start.elapsed(), event));
        Ok(())
    })
    .await
    .unwrap();
    let events = events.into_inner().unwrap();
    let finished: Vec<_> = events
        .iter()
        .filter_map(|(time, event)| {
            if let Event::Finished { probe } = event {
                Some((*time, probe))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(finished[0].1.outbound, "fast");
    assert!(finished[0].0 < Duration::from_secs(2));
    assert_eq!(finished[1].1.outbound, "slow");
    assert!(finished[1]
        .1
        .error
        .as_deref()
        .unwrap()
        .contains("timed out after 3s"));
    assert!(finished[1].0 < Duration::from_secs(4));
    assert_eq!(
        report
            .probes
            .iter()
            .map(|p| p.outbound.as_str())
            .collect::<Vec<_>>(),
        ["slow", "fast"]
    );
    assert!(matches!(events.first().unwrap().1, Event::Started { .. }));
    assert!(matches!(events.last().unwrap().1, Event::Done));
    fast.await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn configured_connect_timeout_reports_the_actual_limit_and_cancels_the_socket() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let node = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut greeting = [0; 3];
        socket.read_exact(&mut greeting).await.unwrap();
        assert!(socket.read_u8().await.is_err());
    });
    let app = App::from_text(std::path::Path::new("timeout.toml"), format!("standalone=true\ndiagnostic_connect_timeout_secs=1\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\nkind='socks5'\ntag='node'\nserver='{address}'\n")).unwrap();
    let report = selected(&app, &["node".into()]).await.unwrap();
    let probe = &report.probes[0];
    assert!(probe.connect_ms.is_none());
    assert!(
        probe
            .error
            .as_deref()
            .unwrap()
            .contains("TCP/proxy handshake timed out after 1s"),
        "{probe:?}"
    );
    assert!(
        probe.elapsed_ms >= 900 && probe.elapsed_ms < 2500,
        "{probe:?}"
    );
    node.await.unwrap();
}

#[tokio::test]
async fn extended_diagnostic_budget_crosses_old_limits_without_changing_normal_connections() {
    // Exercise both the streamed batch and the persistent exit-probe worker.
    // Six seconds exceeds the previous 3s handshake and 5s per-node deadlines.
    for persistent in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let node = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut greeting = [0; 3];
            socket.read_exact(&mut greeting).await.unwrap();
            tokio::time::sleep(Duration::from_secs(6)).await;
            socket.write_all(&[5, 0]).await.unwrap();
            let mut head = [0; 5];
            socket.read_exact(&mut head).await.unwrap();
            let mut rest = vec![0; head[4] as usize + 2];
            socket.read_exact(&mut rest).await.unwrap();
            socket
                .write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0])
                .await
                .unwrap();
        });
        let app = App::from_text(std::path::Path::new("extended.toml"), format!("standalone=true\nconnect_timeout_secs=1\ndiagnostic_connect_timeout_secs=12\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\nkind='socks5'\ntag='node'\nserver='{address}'\n")).unwrap();
        let probe = if persistent {
            let requests = b"{\"outbound\":\"node\",\"ip\":false}\n";
            let mut replies = Vec::new();
            serve(&app, &requests[..], &mut replies).await.unwrap();
            serde_json::from_slice::<Probe>(&replies).unwrap()
        } else {
            selected(&app, &["node".into()])
                .await
                .unwrap()
                .probes
                .remove(0)
        };
        node.await.unwrap();
        assert!(probe.connect_ms.is_some_and(|ms| ms >= 5900), "{probe:?}");
        if persistent {
            assert!(probe.error.is_none(), "{probe:?}");
        }
        assert_eq!(app.config.connect_timeout_secs, 1);
    }
}
#[tokio::test]
async fn probe_uses_selected_proxy_and_distinguishes_connection_from_https_failure() {
    for accept in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut greeting = [0u8; 3];
            stream.read_exact(&mut greeting).await.unwrap();
            assert_eq!(greeting, [5, 1, 0]);
            stream.write_all(&[5, 0]).await.unwrap();
            let mut header = [0u8; 5];
            stream.read_exact(&mut header).await.unwrap();
            assert_eq!(&header[..4], &[5, 1, 0, 3]);
            let mut name = vec![0; header[4] as usize];
            stream.read_exact(&mut name).await.unwrap();
            assert_eq!(name, b"api.ip.sb");
            assert_eq!(stream.read_u16().await.unwrap(), 443);
            stream
                .write_all(&[5, if accept { 0 } else { 5 }, 0, 1, 127, 0, 0, 1, 0, 0])
                .await
                .unwrap();
            if accept {
                let mut hello = [0; 5];
                stream.read_exact(&mut hello).await.unwrap();
                stream.write_all(b"not a TLS server").await.unwrap();
            }
        });
        let path = std::env::current_dir()
            .unwrap()
            .join("diagnostic-test.toml");
        let app = App::from_text(&path, format!("standalone=true\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\nkind='socks5'\ntag='edge'\nserver='127.0.0.1:{port}'\n")).unwrap();
        assert!(selected(&app, &[]).await.is_err());
        assert!(selected(&app, &["missing".into()]).await.is_err());
        let report = selected(&app, &["edge".into(), "edge".into()])
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(report.probes.len(), 1);
        let probe = &report.probes[0];
        assert_eq!(probe.outbound, "edge");
        assert!(!probe.direct);
        assert_eq!(probe.connect_ms.is_some(), accept);
        assert!(probe.ip.is_none());
        assert!(probe.error.is_some());
    }
}
async fn response(raw: &'static [u8]) -> Result<IpAddr> {
    let (client, mut server) = tokio::io::duplex(2048);
    tokio::spawn(async move {
        let mut request = vec![];
        loop {
            request.push(server.read_u8().await.unwrap());
            if request.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let request = String::from_utf8(request).unwrap();
        assert!(request.starts_with("GET /ip HTTP/1.1"));
        assert!(request.to_lowercase().contains("host: api.ip.sb"));
        server.write_all(raw).await.unwrap();
    });
    request_ip(client).await
}
#[tokio::test]
async fn bounded_ip_response_supports_v4_v6_and_chunking_and_rejects_errors() {
    assert_eq!(
        response(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n192.0.2.1\n")
            .await
            .unwrap(),
        "192.0.2.1".parse::<IpAddr>().unwrap()
    );
    assert_eq!(
        response(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nc\r\n2001:db8::1\n\r\n0\r\n\r\n"
        )
        .await
        .unwrap(),
        "2001:db8::1".parse::<IpAddr>().unwrap()
    );
    assert!(
        response(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
            .await
            .is_err()
    );
    assert!(
        response(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nhtml")
            .await
            .is_err()
    );
    assert!(response(b"HTTP/1.1 200 OK\r\nContent-Length: 300\r\n\r\n")
        .await
        .is_err());
}

#[tokio::test]
async fn probe_server_times_handshakes_per_request_without_the_ip_and_ends_on_eof() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    // A SOCKS5 node that accepts every CONNECT; the IP lookup is never attempted.
    let node = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut greeting = [0; 3];
            socket.read_exact(&mut greeting).await.unwrap();
            socket.write_all(&[5, 0]).await.unwrap();
            let mut head = [0; 5];
            socket.read_exact(&mut head).await.unwrap();
            let mut rest = vec![0; head[4] as usize + 2];
            socket.read_exact(&mut rest).await.unwrap();
            assert_eq!(&rest[..head[4] as usize], HOST.as_bytes());
            socket
                .write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0])
                .await
                .unwrap();
        }
    });
    let app = App::from_text(std::path::Path::new("probe.toml"),format!("standalone=true\nstandalone_outbound='node'\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\nkind='socks5'\ntag='node'\nserver='{address}'\n")).unwrap();
    let requests = b"{\"outbound\":\"node\",\"ip\":false}\n{\"outbound\":\"gone\",\"ip\":false}\n{\"outbound\":\"node\",\"ip\":false}\n";
    let mut replies = Vec::new();
    serve(&app, &requests[..], &mut replies).await.unwrap();
    node.await.unwrap();
    let probes: Vec<Probe> = std::str::from_utf8(&replies)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(probes.len(), 3, "one reply per request, then EOF ends it");
    for probe in [&probes[0], &probes[2]] {
        assert!(probe.connect_ms.is_some(), "{probe:?}");
        assert!(probe.ip.is_none() && probe.error.is_none(), "{probe:?}");
    }
    assert!(probes[1].connect_ms.is_none());
    assert!(probes[1]
        .error
        .as_deref()
        .unwrap()
        .contains("Unknown diagnostic node"));
}
