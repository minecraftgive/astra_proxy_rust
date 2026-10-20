use crate::App;
use anyhow::{ensure, Context, Result};
use astra_ipc::{
    diagnostics::{Probe, Report},
    RouteAction, Target,
};
use bytes::Bytes;
use futures_util::{stream, StreamExt};
use http_body_util::{BodyExt, Empty};
use hyper_util::rt::TokioIo;
use std::{
    net::IpAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio_rustls::{
    rustls::{self, pki_types::ServerName, RootCertStore},
    TlsConnector,
};

const HOST: &str = "api.ip.sb";
const URL: &str = "https://api.ip.sb/ip";

pub async fn run(app: &App) -> Report {
    let actions = std::iter::once(RouteAction::Direct).chain(
        app.config
            .outbounds
            .iter()
            .map(|o| RouteAction::Proxy(o.tag().to_owned()))
            .chain(
                app.config
                    .proxy_groups
                    .keys()
                    .cloned()
                    .map(RouteAction::Proxy),
            ),
    );
    run_actions(app, actions.collect()).await
}
async fn run_actions(app: &App, actions: Vec<RouteAction>) -> Report {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let probes = stream::iter(actions)
        .map(|action| async move {
            let start = Instant::now();
            let mut connect_ms = None;
            let end = deadline.min(tokio::time::Instant::now() + Duration::from_secs(8));
            let result = tokio::time::timeout_at(end, async {
                ensure!(
                    tokio::time::Instant::now() < deadline,
                    "diagnostic deadline exceeded"
                );
                let target = Target {
                    host: HOST.into(),
                    port: 443,
                };
                let connection = app.connect(&target, &action).await?;
                connect_ms = Some(start.elapsed().as_millis() as u64);
                let mut roots = RootCertStore::empty();
                roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
                let config = rustls::ClientConfig::builder_with_provider(Arc::new(
                    rustls::crypto::aws_lc_rs::default_provider(),
                ))
                .with_safe_default_protocol_versions()?
                .with_root_certificates(roots)
                .with_no_client_auth();
                let tls = TlsConnector::from(Arc::new(config))
                    .connect(ServerName::try_from(HOST)?, connection)
                    .await
                    .context("HTTPS handshake")?;
                request_ip(tls).await
            })
            .await;
            let result = result.unwrap_or_else(|_| Err(anyhow::anyhow!("request timed out")));
            let (ip, error) = match result {
                Ok(ip) => (Some(ip), None),
                Err(error) => {
                    // Arbitrary protocol errors may contain peer-provided data: bound and sanitize it.
                    let message: String = format!("{error:#}")
                        .chars()
                        .filter(|c| !c.is_control())
                        .take(240)
                        .collect();
                    (None, Some(message))
                }
            };
            Probe {
                outbound: match &action {
                    RouteAction::Direct => "DIRECT".into(),
                    RouteAction::Proxy(tag) => tag.clone(),
                    _ => unreachable!(),
                },
                direct: action == RouteAction::Direct,
                connect_ms,
                elapsed_ms: start.elapsed().as_millis() as u64,
                ip,
                error,
            }
        })
        .buffered(4)
        .collect()
        .await;
    Report {
        url: URL.into(),
        running_config: false,
        probes,
    }
}

struct Driver(tokio::task::JoinHandle<()>);
impl Drop for Driver {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn request_ip<S>(stream: S) -> Result<IpAddr>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, connection) = hyper::client::conn::http1::Builder::new()
        .max_buf_size(16 * 1024)
        .handshake(TokioIo::new(stream))
        .await?;
    let _driver = Driver(tokio::spawn(async move {
        let _ = connection.await;
    }));
    let request = hyper::Request::builder()
        .uri("/ip")
        .header("Host", HOST)
        .header("User-Agent", "astra-dual-core/0.1.0")
        .header("Accept", "text/plain")
        .header("Connection", "close")
        .body(Empty::<Bytes>::new())?;
    let response = sender
        .send_request(request)
        .await
        .context("HTTPS request")?;
    ensure!(
        response.status() == hyper::StatusCode::OK,
        "IP service returned HTTP {}",
        response.status()
    );
    let mut body = response.into_body();
    let mut bytes = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame?;
        if let Some(data) = frame.data_ref() {
            ensure!(
                bytes.len() + data.len() <= 256,
                "IP response exceeds 256 bytes"
            );
            bytes.extend_from_slice(data);
        }
    }
    std::str::from_utf8(&bytes)?
        .trim()
        .parse()
        .context("IP service did not return an IP address")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
            let report = run_actions(&app, vec![RouteAction::Proxy("edge".into())]).await;
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
        assert_eq!(response(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nc\r\n2001:db8::1\n\r\n0\r\n\r\n").await.unwrap(), "2001:db8::1".parse::<IpAddr>().unwrap());
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
}
