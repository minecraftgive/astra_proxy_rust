use super::*;
use std::fmt::Write as _;

// ─── Helper ──────────────────────────────────────────────────────────────

/// Spawn a TCP loopback server that replies with `response` verbatim, then
/// echoes data.  Returns the local address.
async fn mock_proxy(response: &'static str) -> std::net::SocketAddr {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        // Drain the CONNECT request (read until \r\n\r\n).
        let mut buf = vec![0u8; 4096];
        let mut len = 0;
        loop {
            let n = stream.read(&mut buf[len..]).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            len += n;
            if buf[..len].windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        // Reply with the canned response.
        stream.write_all(response.as_bytes()).await.unwrap();
        // Echo payload.
        let mut echo_buf = [0u8; 256];
        loop {
            match stream.read(&mut echo_buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let _ = stream.write_all(&echo_buf[..n]).await;
                }
            }
        }
    });

    addr
}

fn make_metadata(host: &str, port: u16) -> Metadata {
    Metadata {
        host: host.into(),
        dst_port: port,
        ..Default::default()
    }
}

fn make_adapter_no_auth(server: &str, port: u16) -> HttpAdapter {
    HttpAdapter::new(
        server,
        server,
        port,
        None,
        false,
        false,
        vec![],
        Arc::new(crate::dialer::DirectDialer),
    )
}

// ─── http_connect_no_auth_succeeds ────────────────────────────────────────
// upstream: adapter/outbound/http.go::DialContext

#[tokio::test]
async fn http_connect_no_auth_succeeds() {
    let addr = mock_proxy("HTTP/1.1 200 Connection established\r\n\r\n").await;
    let adapter = make_adapter_no_auth("127.0.0.1", addr.port());
    let meta = make_metadata("example.com", 443);
    let conn = adapter.dial_tcp(&meta).await.expect("dial_tcp");
    drop(conn); // stream is live
}

// ─── http_connect_basic_auth_header_sent ─────────────────────────────────
// NOT md5 or digest — Basic only.

#[tokio::test]
async fn http_connect_basic_auth_header_sent() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (req_tx, req_rx) = tokio::sync::oneshot::channel::<String>();

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 4096];
        let mut len = 0;
        loop {
            let n = stream.read(&mut buf[len..]).await.unwrap_or(0);
            len += n;
            if buf[..len].windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
            if n == 0 {
                break;
            }
        }
        let req = String::from_utf8_lossy(&buf[..len]).to_string();
        let _ = req_tx.send(req);
        stream
            .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
            .await
            .unwrap();
    });

    let adapter = HttpAdapter::new(
        "test",
        "127.0.0.1",
        addr.port(),
        Some(("alice".into(), "s3cr3t".into())),
        false,
        false,
        vec![],
        Arc::new(crate::dialer::DirectDialer),
    );
    let meta = make_metadata("example.com", 443);
    let _ = adapter.dial_tcp(&meta).await.expect("dial_tcp");

    let req = req_rx.await.expect("request captured");
    // Basic base64("alice:s3cr3t") = "YWxpY2U6czNjcjN0"
    let expected_creds = base64::engine::general_purpose::STANDARD.encode("alice:s3cr3t");
    assert!(
        req.contains(&format!("Proxy-Authorization: Basic {expected_creds}")),
        "Basic auth header must be present with correct base64; req=\n{req}"
    );
}

// ─── http_connect_407_returns_proxy_auth_failed ───────────────────────────
// Class A per ADR-0002: 407 is a hard error.

#[tokio::test]
async fn http_connect_407_returns_proxy_auth_failed() {
    let addr = mock_proxy("HTTP/1.1 407 Proxy Authentication Required\r\n\r\n").await;
    let adapter = make_adapter_no_auth("127.0.0.1", addr.port());
    let meta = make_metadata("example.com", 443);
    let err = adapter.dial_tcp(&meta).await.err().expect("expected Err");
    assert!(
        matches!(err, MeowError::ProxyAuthFailed),
        "407 must map to ProxyAuthFailed; got {err:?}"
    );
}

// ─── http_connect_503_returns_http_connect_failed ─────────────────────────
// NOT panic, NOT timeout.

#[tokio::test]
async fn http_connect_503_returns_http_connect_failed() {
    let addr = mock_proxy("HTTP/1.1 503 Service Unavailable\r\n\r\n").await;
    let adapter = make_adapter_no_auth("127.0.0.1", addr.port());
    let meta = make_metadata("example.com", 443);
    let err = adapter.dial_tcp(&meta).await.err().expect("expected Err");
    assert!(
        matches!(err, MeowError::HttpConnectFailed(503)),
        "503 must map to HttpConnectFailed(503); got {err:?}"
    );
}

// ─── http_extra_headers_injected ─────────────────────────────────────────

#[tokio::test]
async fn http_extra_headers_injected() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (req_tx, req_rx) = tokio::sync::oneshot::channel::<String>();

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 4096];
        let mut len = 0;
        loop {
            let n = stream.read(&mut buf[len..]).await.unwrap_or(0);
            len += n;
            if buf[..len].windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
            if n == 0 {
                break;
            }
        }
        let _ = req_tx.send(String::from_utf8_lossy(&buf[..len]).to_string());
        stream
            .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
            .await
            .unwrap();
    });

    let adapter = HttpAdapter::new(
        "test",
        "127.0.0.1",
        addr.port(),
        None,
        false,
        false,
        vec![("X-Foo".into(), "bar".into())],
        Arc::new(crate::dialer::DirectDialer),
    );
    let meta = make_metadata("example.com", 443);
    let _ = adapter.dial_tcp(&meta).await.expect("dial_tcp");

    let req = req_rx.await.expect("request captured");
    assert!(
        req.contains("X-Foo: bar"),
        "extra header X-Foo must appear in CONNECT request; req=\n{req}"
    );
}

#[tokio::test]
async fn http_extra_headers_reject_crlf_in_value() {
    let adapter = HttpAdapter::new(
        "test",
        "127.0.0.1",
        8080,
        None,
        false,
        false,
        vec![("X-Foo".into(), "bar\r\nX-Injected: yes".into())],
        Arc::new(crate::dialer::DirectDialer),
    );
    let (mut client, _server) = tokio::io::duplex(64);
    let target = "example.com:443";
    let err = adapter
        .run_connect(&mut client, &target)
        .await
        .expect_err("invalid header must error");
    assert!(
        matches!(err, MeowError::Config(ref msg) if msg.contains("X-Foo")),
        "expected config error naming X-Foo, got {err:?}"
    );
}

#[tokio::test]
async fn http_extra_headers_are_not_truncated() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (req_tx, req_rx) = tokio::sync::oneshot::channel::<String>();

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 512];
        loop {
            let n = stream.read(&mut chunk).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let _ = req_tx.send(String::from_utf8_lossy(&buf).to_string());
        stream
            .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
            .await
            .unwrap();
    });

    let long_value = "a".repeat(1500);
    let adapter = HttpAdapter::new(
        "test",
        "127.0.0.1",
        addr.port(),
        None,
        false,
        false,
        vec![("X-Long".into(), long_value.clone())],
        Arc::new(crate::dialer::DirectDialer),
    );
    let meta = make_metadata("example.com", 443);
    let _ = adapter.dial_tcp(&meta).await.expect("dial_tcp");

    let req = req_rx.await.expect("request captured");
    assert!(
        req.contains(&format!("X-Long: {long_value}\r\n\r\n")),
        "full long header and terminator must be present; len={}",
        req.len()
    );
}

// ─── http_response_header_count_exceeded_returns_error ───────────────────
// Pre-VLESS hardening (M1.B-3): > 100 response headers → Proxy error.
// NOT infinite drain. NOT connection hang.
// ADR-0002 Class A divergence: upstream http.go has no explicit header cap.

#[tokio::test]
async fn http_response_header_count_exceeded_returns_error() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        // Drain the CONNECT request.
        let mut buf = vec![0u8; 4096];
        let mut len = 0;
        loop {
            let n = stream.read(&mut buf[len..]).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            len += n;
            if buf[..len].windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        // 200 status + 101 headers (exceeds the 100-header cap) + blank line.
        let mut resp = String::from("HTTP/1.1 200 Connection established\r\n");
        for i in 0..101usize {
            let _ = write!(resp, "X-Flood-{i}: value\r\n");
        }
        resp.push_str("\r\n");
        let _ = stream.write_all(resp.as_bytes()).await;
    });

    let adapter = make_adapter_no_auth("127.0.0.1", addr.port());
    let meta = make_metadata("example.com", 443);
    let err = adapter.dial_tcp(&meta).await.err().expect("expected Err");
    assert!(
        matches!(err, MeowError::Proxy(ref msg) if msg.contains("more than 100 headers")),
        "101 response headers must trigger the cap error; got {err:?}"
    );
}

// ─── http_connect_over_relay ─────────────────────────────────────────────
// connect_over runs the handshake over an existing stream (relay chain),
// NOT a fresh TCP connect.

#[tokio::test]
async fn http_connect_over_relay() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        // drain request
        let mut buf = [0u8; 4096];
        let mut len = 0;
        loop {
            let n = stream.read(&mut buf[len..]).await.unwrap_or(0);
            len += n;
            if buf[..len].windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
            if n == 0 {
                break;
            }
        }
        stream
            .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
            .await
            .unwrap();
        // echo
        let mut echo = [0u8; 256];
        loop {
            match stream.read(&mut echo).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let _ = stream.write_all(&echo[..n]).await;
                }
            }
        }
    });

    // Establish the "outer" connection (simulating a relay's first hop).
    let tcp = TcpStream::connect(addr).await.unwrap();
    let outer: Box<dyn ProxyConn> = Box::new(tcp);

    let adapter = make_adapter_no_auth("127.0.0.1", addr.port());
    let meta = make_metadata("example.com", 443);

    // connect_over must NOT dial a new TCP connection.
    let mut conn = adapter
        .connect_over(outer, &meta)
        .await
        .expect("connect_over");

    // Verify the tunnel is live.
    conn.write_all(b"hello").await.unwrap();
    let mut buf = [0u8; 5];
    conn.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"hello");
}

// ─── parse_http_status ────────────────────────────────────────────────────

#[test]
fn parse_http_status_cases() {
    // (label, response line, expected): `Some(code)` => `Ok(code)`, `None` => `Err`.
    let cases: &[(&str, &str, Option<u16>)] = &[
        ("200 ok", "HTTP/1.1 200 OK", Some(200)),
        (
            "407 proxy auth required",
            "HTTP/1.0 407 Proxy Auth Required",
            Some(407),
        ),
        ("bad line", "GARBAGE", None),
    ];

    // Collect instead of asserting inline so every case runs even if one fails.
    let mut failures = Vec::new();
    for &(label, line, expected) in cases {
        match (parse_http_status(line), expected) {
            (Ok(code), Some(want)) if code == want => {}
            (Err(_), None) => {}
            (got, want) => failures.push(format!(
                "case {label:?}: parse_http_status({line:?}) expected {want:?}, got {got:?}"
            )),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
