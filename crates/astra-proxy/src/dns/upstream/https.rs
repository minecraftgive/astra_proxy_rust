use anyhow::{ensure, Result};
use astra_dns::Upstream;
use tokio::io::{AsyncRead, AsyncWrite};
struct Connection(tokio::task::JoinHandle<()>);
impl Drop for Connection {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(crate) async fn exchange<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    stream: S,
    upstream: &Upstream,
    query: Vec<u8>,
    http2: bool,
) -> Result<Vec<u8>> {
    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};
    let authority = if upstream.host.contains(':') {
        format!("[{}]:{}", upstream.host, upstream.port)
    } else {
        format!("{}:{}", upstream.host, upstream.port)
    };
    let request = http::Request::builder()
        .method("POST")
        .uri(if http2 {
            format!("https://{authority}{}", upstream.path)
        } else {
            upstream.path.clone()
        })
        .header("Host", authority)
        .header("Content-Type", "application/dns-message")
        .header("Accept", "application/dns-message")
        .body(Full::new(Bytes::from(query)))?;
    let (mut response, _connection) = if http2 {
        let (mut sender, connection) = hyper::client::conn::http2::handshake(
            hyper_util::rt::TokioExecutor::new(),
            hyper_util::rt::TokioIo::new(stream),
        )
        .await?;
        let guard = Connection(tokio::spawn(async move {
            let _ = connection.await;
        }));
        (sender.send_request(request).await?, guard)
    } else {
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream)).await?;
        let guard = Connection(tokio::spawn(async move {
            let _ = connection.await;
        }));
        (sender.send_request(request).await?, guard)
    };
    ensure!(
        response.status() == http::StatusCode::OK,
        "DoH HTTP status {}",
        response.status()
    );
    ensure!(
        response
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|h| h.to_str().ok())
            .is_some_and(|h| h
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("application/dns-message")),
        "invalid DoH content type"
    );
    let mut bytes = Vec::new();
    while let Some(frame) = response.body_mut().frame().await {
        let frame = frame?;
        if let Some(data) = frame.data_ref() {
            ensure!(
                bytes.len() + data.len() <= 65535,
                "DoH response exceeds DNS size limit"
            );
            bytes.extend_from_slice(data);
        }
    }
    Ok(bytes)
}
