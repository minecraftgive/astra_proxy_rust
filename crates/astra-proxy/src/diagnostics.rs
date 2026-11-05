use crate::App;
use anyhow::{ensure, Context, Result};
use astra_model::diagnostics::Event;
use astra_model::diagnostics::Probe;
use astra_model::diagnostics::Report;
use astra_model::RouteAction;
use astra_model::Target;
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
                .filter(|name| !astra_model::groups::builtin(name))
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
                    .filter(|name| !astra_model::groups::builtin(name))
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
#[path = "test/diagnostics_test.rs"]
mod tests;
