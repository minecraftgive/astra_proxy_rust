use super::*;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, DuplexStream},
    net::{TcpListener, UdpSocket},
    time::timeout,
};

fn app(settings: &str) -> Arc<crate::App> {
    Arc::new(crate::App::from_text(
        &std::env::temp_dir().join("astra-hosted-inbound.toml"),
        format!("standalone=true\nstandalone_outbound='direct'\nmixed_listen='127.0.0.1:7890'\n{settings}\n[proxy_groups.direct]\nmembers=['DIRECT']\n"),
    ).unwrap())
}

fn serve(
    app: Arc<crate::App>,
    inbound: Inbound,
    source: SocketAddr,
) -> (DuplexStream, tokio::task::JoinHandle<Result<()>>) {
    let (client, stream) = tokio::io::duplex(65536);
    let task = tokio::spawn(handle_stream(
        stream,
        app,
        inbound,
        source,
        "127.0.0.1:7890".parse().unwrap(),
    ));
    (client, task)
}

async fn header(client: &mut DuplexStream) -> Vec<u8> {
    timeout(Duration::from_secs(5), async {
        let mut bytes = Vec::new();
        while !bytes.ends_with(b"\r\n\r\n") {
            bytes.push(client.read_u8().await.unwrap());
            assert!(bytes.len() < 8192);
        }
        bytes
    })
    .await
    .unwrap()
}

async fn trailer_server() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut body = Vec::new();
        stream.read_to_end(&mut body).await.unwrap();
        stream.write_all(b"after-fin:").await.unwrap();
        stream.write_all(&body).await.unwrap();
        stream.shutdown().await.unwrap();
    });
    (address, task)
}

#[tokio::test]
async fn hosted_connect_replays_fragmented_auth_and_tail_and_preserves_half_close() {
    let (target, remote) = trailer_server().await;
    let (mut client, worker) = serve(
        app("authentication=['alice:correct']"),
        Inbound::Mixed,
        "127.0.0.1:45000".parse().unwrap(),
    );
    client.write_all(b"CON").await.unwrap();
    tokio::task::yield_now().await;
    client.write_all(format!("NECT {target} HTTP/1.1\r\nHost: {target}\r\nProxy-Authorization: Basic YWxpY2U6Y29ycmVjdA==\r\n\r\npipelined-body").as_bytes()).await.unwrap();
    assert!(header(&mut client).await.starts_with(b"HTTP/1.1 200"));
    client.shutdown().await.unwrap();
    let mut body = Vec::new();
    timeout(Duration::from_secs(5), client.read_to_end(&mut body))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(body, b"after-fin:pipelined-body");
    worker.await.unwrap().unwrap();
    remote.await.unwrap();
}

#[tokio::test]
async fn hosted_mixed_socks_keeps_upload_eof_separate_from_download() {
    let (target, remote) = trailer_server().await;
    let (mut client, worker) = serve(app(""), Inbound::Mixed, "127.0.0.1:45000".parse().unwrap());
    client.write_all(&[5, 1, 0]).await.unwrap();
    let mut method = [0; 2];
    client.read_exact(&mut method).await.unwrap();
    assert_eq!(method, [5, 0]);
    let mut request = vec![5, 1, 0, 1, 127, 0, 0, 1];
    request.extend_from_slice(&target.port().to_be_bytes());
    request.extend_from_slice(b"socks-body");
    client.write_all(&request).await.unwrap();
    let mut reply = [0; 10];
    client.read_exact(&mut reply).await.unwrap();
    assert_eq!(reply[1], 0);
    client.shutdown().await.unwrap();
    let mut body = Vec::new();
    timeout(Duration::from_secs(5), client.read_to_end(&mut body))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(body, b"after-fin:socks-body");
    worker.await.unwrap().unwrap();
    remote.await.unwrap();
}

#[tokio::test]
async fn hosted_http_forward_uses_stream_and_strips_proxy_credentials() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = listener.local_addr().unwrap();
    let remote = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            request.push(socket.read_u8().await.unwrap());
        }
        let text = String::from_utf8(request).unwrap();
        assert!(text.starts_with("GET /through-host HTTP/1.1\r\n"));
        assert!(!text.to_ascii_lowercase().contains("proxy-authorization"));
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nbody")
            .await
            .unwrap();
    });
    let (mut client, worker) = serve(
        app("authentication=['alice:correct']"),
        Inbound::Mixed,
        "127.0.0.1:45000".parse().unwrap(),
    );
    client.write_all(format!("GET http://{target}/through-host HTTP/1.1\r\nHost: {target}\r\nProxy-Authorization: Basic YWxpY2U6Y29ycmVjdA==\r\n\r\n").as_bytes()).await.unwrap();
    let mut response = Vec::new();
    timeout(Duration::from_secs(5), client.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    assert!(response.starts_with(b"HTTP/1.1 200"));
    assert!(response.ends_with(b"body"));
    worker.await.unwrap().unwrap();
    remote.await.unwrap();
}

#[tokio::test]
async fn hosted_stream_uses_original_source_for_acl_auth_and_loopback_protection() {
    let source = "192.0.2.20:45000".parse().unwrap();
    let (client, worker) = serve(app(""), Inbound::Mixed, source);
    assert!(worker
        .await
        .unwrap()
        .unwrap_err()
        .to_string()
        .contains("source is not allowed"));
    drop(client);

    let app = app("authentication=['alice:correct']\nskip_auth_prefixes=['127.0.0.1/32']");
    app.overrides.write().unwrap().allow_lan = Some(true);
    let (mut client, worker) = serve(app.clone(), Inbound::HttpConnect, source);
    client
        .write_all(b"CONNECT 127.0.0.1:12345 HTTP/1.1\r\n\r\n")
        .await
        .unwrap();
    assert!(header(&mut client).await.starts_with(b"HTTP/1.1 407"));
    worker.await.unwrap().unwrap();

    let (mut client, worker) = serve(app, Inbound::HttpConnect, source);
    client.write_all(b"CONNECT 127.0.0.1:12345 HTTP/1.1\r\nProxy-Authorization: Basic YWxpY2U6Y29ycmVjdA==\r\n\r\n").await.unwrap();
    assert!(header(&mut client).await.starts_with(b"HTTP/1.1 403"));
    worker.await.unwrap().unwrap();
}

#[tokio::test]
async fn hosted_socks_udp_keeps_original_binding_source_and_control_lifetime() {
    let echo = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let target = echo.local_addr().unwrap();
    let remote = tokio::spawn(async move {
        let mut buffer = [0; 1024];
        let (count, source) = echo.recv_from(&mut buffer).await.unwrap();
        echo.send_to(&buffer[..count], source).await.unwrap();
    });
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let source = socket.local_addr().unwrap();
    let (mut client, worker) = serve(app(""), Inbound::Socks5, source);
    client.write_all(&[5, 1, 0]).await.unwrap();
    let mut method = [0; 2];
    client.read_exact(&mut method).await.unwrap();
    assert_eq!(method, [5, 0]);
    let mut request = vec![5, 3, 0, 1, 127, 0, 0, 1];
    request.extend_from_slice(&source.port().to_be_bytes());
    client.write_all(&request).await.unwrap();
    let mut reply = [0; 10];
    client.read_exact(&mut reply).await.unwrap();
    assert_eq!(&reply[..8], &[5, 0, 0, 1, 127, 0, 0, 1]);
    let relay = SocketAddr::new(source.ip(), u16::from_be_bytes([reply[8], reply[9]]));
    let mut packet = vec![0, 0, 0, 1, 127, 0, 0, 1];
    packet.extend_from_slice(&target.port().to_be_bytes());
    packet.extend_from_slice(b"udp-body");
    let wrong = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    wrong.send_to(&packet, relay).await.unwrap();
    let mut buffer = [0; 1024];
    assert!(
        timeout(Duration::from_millis(100), wrong.recv_from(&mut buffer))
            .await
            .is_err()
    );
    socket.send_to(&packet, relay).await.unwrap();
    let (count, _) = timeout(Duration::from_secs(5), socket.recv_from(&mut buffer))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&buffer[count - 8..count], b"udp-body");
    drop(client);
    timeout(Duration::from_secs(5), worker)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    remote.await.unwrap();
    // Closing the hosted TCP stream releases the association's UDP socket too.
    UdpSocket::bind(relay).await.unwrap();
}
