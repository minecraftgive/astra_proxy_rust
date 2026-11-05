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
