use crate::App;
use anyhow::{ensure, Context, Result};
use astra_ipc::{
    diagnostics::{Event, Probe, Report},
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

pub fn targets(app: &App) -> Vec<String> {
    std::iter::once("DIRECT".to_owned())
        .chain(app.config.outbounds.iter().map(|o| o.tag().to_owned()))
        // The built-in DIRECT group is covered by the direct check; REJECT has nothing to test.
        .chain(
            app.config
                .proxy_groups
                .keys()
                .filter(|name| !astra_ipc::groups::builtin(name))
                .cloned(),
        )
        .collect()
}
pub async fn selected(app: &App, tags: &[String]) -> Result<Report> {
    selected_stream(app, tags, |_| Ok(())).await
}
pub async fn selected_stream(
    app: &App,
    tags: &[String],
    emit: impl Fn(Event) -> Result<()>,
) -> Result<Report> {
    let available = targets(app);
    ensure!(!tags.is_empty(), "Select at least one node");
    ensure!(
        tags.iter().all(|tag| available.contains(tag)),
        "Unknown diagnostic node; refresh the test page"
    );
    let mut seen = std::collections::HashSet::new();
    let actions = tags
        .iter()
        .filter(|tag| seen.insert((*tag).clone()))
        .map(|tag| {
            if tag == "DIRECT" {
                RouteAction::Direct
            } else {
                RouteAction::Proxy(tag.clone())
            }
        })
        .collect();
    run_actions(app, actions, emit).await
}
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
                    .filter(|name| !astra_ipc::groups::builtin(name))
                    .cloned()
                    .map(RouteAction::Proxy),
            ),
    );
    run_actions(app, actions.collect(), |_| Ok(()))
        .await
        .expect("infallible diagnostic collector")
}
async fn run_actions(
    app: &App,
    actions: Vec<RouteAction>,
    emit: impl Fn(Event) -> Result<()>,
) -> Result<Report> {
    let order: Vec<_> = actions.iter().map(action_name).collect();
    emit(Event::Started {
        targets: order.clone(),
        url: URL.into(),
        running_config: false,
    })?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_root_certificates(roots)
    .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(config));
    let emit = &emit;
    let connector = &connector;
    let mut pending = stream::iter(actions)
        .map(|action| async move {
            let start = Instant::now();
            let outbound = action_name(&action);
            emit(Event::Running {
                outbound: outbound.clone(),
            })?;
            let mut connect_ms = None;
            let end = deadline.min(tokio::time::Instant::now() + Duration::from_secs(5));
            let result = tokio::time::timeout_at(end, async {
                ensure!(
                    tokio::time::Instant::now() < deadline,
                    "batch time limit reached; select fewer nodes and retry"
                );
                let target = Target {
                    host: HOST.into(),
                    port: 443,
                };
                let connection = tokio::time::timeout_at(
                    deadline.min(tokio::time::Instant::now() + Duration::from_secs(3)),
                    app.connect(&target, &action),
                )
                .await
                .context("TCP/proxy handshake timed out after 3s")??;
                connect_ms = Some(start.elapsed().as_millis() as u64);
                emit(Event::Connected {
                    outbound: outbound.clone(),
                    connect_ms: connect_ms.unwrap(),
                })?;
                let tls = connector
                    .connect(ServerName::try_from(HOST)?, connection)
                    .await
                    .context("HTTPS handshake")?;
                request_ip(tls).await
            })
            .await;
            let result = result.unwrap_or_else(|_| {
                Err(anyhow::anyhow!(
                    "node test timed out (5s per node, 30s per batch)"
                ))
            });
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
            Ok::<_, anyhow::Error>(Probe {
                outbound,
                direct: action == RouteAction::Direct,
                connect_ms,
                elapsed_ms: start.elapsed().as_millis() as u64,
                ip,
                error,
            })
        })
        .buffer_unordered(8);
    let mut probes = Vec::new();
    while let Some(probe) = pending.next().await {
        let probe = probe?;
        emit(Event::Finished {
            probe: probe.clone(),
        })?;
        probes.push(probe);
    }
    probes.sort_by_key(|probe| {
        order
            .iter()
            .position(|tag| tag == &probe.outbound)
            .unwrap_or(usize::MAX)
    });
    emit(Event::Done)?;
    Ok(Report {
        url: URL.into(),
        running_config: false,
        probes,
    })
}

fn action_name(action: &RouteAction) -> String {
    match action {
        RouteAction::Direct => "DIRECT".into(),
        RouteAction::Proxy(tag) => tag.clone(),
        _ => unreachable!(),
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
