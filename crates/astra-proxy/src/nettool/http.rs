//! Bounded HTTP/1 transactions over the same diagnostic route as TCP and DNS.
use super::detailed::path::Route;
use anyhow::{bail, ensure, Context, Result};
use astra_model::{
    nettool::Event,
    network_tls::{HttpDetail, HttpHeader, HttpTimings},
};
use astra_protocols::outbound::Stream;
use http_body_util::{BodyExt, Empty};
use hyper::body::Body;
use std::{net::IpAddr, time::Instant};
use url::Url;
const HEADERS: usize = 32 * 1024;
const PREVIEW: usize = 64 * 1024;
const TEXT_CHUNK: usize = 4 * 1024;

pub(super) struct Endpoint {
    pub url: Url,
    pub bootstrap: Option<IpAddr>,
}
impl Endpoint {
    pub fn parse(value: &str) -> Result<Self> {
        ensure!(value.len() <= 4096, "HTTP URL exceeds limit");
        let mut url = Url::parse(value).map_err(|_| anyhow::anyhow!("Invalid HTTP URL"))?;
        ensure!(
            matches!(url.scheme(), "http" | "https"),
            "Only HTTP and HTTPS URLs are supported"
        );
        ensure!(
            url.username().is_empty() && url.password().is_none(),
            "HTTP URL credentials are not accepted"
        );
        ensure!(url.host_str().is_some(), "HTTP URL is missing a host");
        let bootstrap = url
            .fragment()
            .map(|fragment| {
                fragment
                    .strip_prefix("bootstrap=")
                    .context("Only #bootstrap=IP is accepted for HTTP URL fragments")
                    .and_then(|ip| ip.parse::<IpAddr>().context("Invalid HTTP bootstrap IP"))
            })
            .transpose()?;
        url.set_fragment(None);
        Ok(Self { url, bootstrap })
    }
    pub fn service(&self) -> String {
        service(&self.url)
    }
}
fn service(url: &Url) -> String {
    url.origin().ascii_serialization()
}
fn emit_detail(seq: u32, detail: HttpDetail, emit: &impl Fn(Event) -> Result<()>) -> Result<()> {
    let event = Event::Http {
        seq,
        detail: Box::new(detail),
    };
    ensure!(
        serde_json::to_vec(&event)?.len() < 56 * 1024,
        "HTTP evidence exceeds event budget"
    );
    emit(event)
}
fn sensitive(key: &str) -> bool {
    let key = key.to_ascii_lowercase().replace(['-', '_'], "");
    [
        "authorization",
        "password",
        "passwd",
        "secret",
        "token",
        "cookie",
        "session",
        "credential",
        "apikey",
        "privatekey",
    ]
    .iter()
    .any(|part| key.contains(part))
        || key == "key"
}
fn redact_json(value: &mut serde_json::Value, depth: usize) -> bool {
    if depth > 24 {
        *value = serde_json::Value::String("[nested content omitted]".into());
        return true;
    }
    match value {
        serde_json::Value::Object(map) => {
            let mut changed = false;
            for (key, value) in map {
                if sensitive(key) {
                    *value = "[redacted]".into();
                    changed = true;
                } else {
                    changed |= redact_json(value, depth + 1);
                }
            }
            changed
        }
        serde_json::Value::Array(items) => {
            // Visit every item: short-circuiting any() would leave later secrets.
            let mut changed = false;
            for value in items {
                changed |= redact_json(value, depth + 1);
            }
            changed
        }
        serde_json::Value::String(text) => {
            if text.starts_with("https://") || text.starts_with("http://") {
                if let Ok(url) = Url::parse(text) {
                    *text = format!("{}/[path and query omitted]", service(&url));
                    return true;
                }
            }
            false
        }
        _ => false,
    }
}
fn preview(content_type: &str, bytes: &[u8], truncated: bool) -> (String, bool) {
    if content_type.contains("json") {
        if truncated {
            return ("[preview omitted: JSON body exceeds limit]".into(), true);
        }
        if let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(bytes) {
            let changed = redact_json(&mut value, 0);
            return (value.to_string(), changed);
        }
        return ("[preview omitted: invalid or incomplete JSON]".into(), true);
    }
    if !(content_type.starts_with("text/") || content_type.is_empty()) {
        return ("[binary preview omitted]".into(), true);
    }
    let text = String::from_utf8_lossy(bytes).into_owned();
    let lower = text.to_ascii_lowercase();
    if [
        "authorization",
        "bearer ",
        "password",
        "passwd",
        "secret",
        "token",
        "cookie",
        "session",
        "credential",
        "api_key",
        "api-key",
        "apikey",
    ]
    .iter()
    .any(|word| lower.contains(word))
    {
        return (
            "[preview omitted: authentication or credential fields detected]".into(),
            true,
        );
    }
    (text, false)
}
fn safe_header(name: &str, value: &str, url: &Url) -> (String, bool) {
    match name {
        "content-type"
        | "content-length"
        | "content-encoding"
        | "cache-control"
        | "date"
        | "server"
        | "age"
        | "expires"
        | "last-modified"
        | "allow"
        | "accept-ranges"
        | "strict-transport-security" => (value.to_owned(), false),
        "location" => (
            url.join(value)
                .map(|url| format!("{}/[path and query omitted]", service(&url)))
                .unwrap_or_else(|_| "[invalid redirect omitted]".into()),
            true,
        ),
        _ => ("[redacted]".into(), true),
    }
}
fn emit_headers(
    seq: u32,
    hop: u32,
    status: u16,
    version: &str,
    headers: &http::HeaderMap,
    url: &Url,
    emit: &impl Fn(Event) -> Result<()>,
) -> Result<bool> {
    let mut records = vec![];
    let mut size = 512;
    let mut redacted = false;
    for (name, value) in headers {
        let (value, changed) = safe_header(
            name.as_str(),
            value.to_str().unwrap_or("[non-text header]"),
            url,
        );
        redacted |= changed;
        let record = HttpHeader {
            name: name.to_string(),
            value,
        };
        let bytes = serde_json::to_vec(&record)?.len();
        if size + bytes > 24 * 1024 && !records.is_empty() {
            emit_detail(
                seq,
                HttpDetail::Headers {
                    hop,
                    status,
                    version: version.into(),
                    headers: std::mem::take(&mut records),
                },
                emit,
            )?;
            size = 512;
        }
        // One legal 32 KiB header can expand considerably in JSON. Preserve its
        // name and explicitly omit such a value rather than exceeding framing.
        let record = if bytes > 24 * 1024 {
            redacted = true;
            HttpHeader {
                name: record.name,
                value: "[header value omitted: evidence chunk limit]".into(),
            }
        } else {
            record
        };
        size += serde_json::to_vec(&record)?.len();
        records.push(record);
    }
    if !records.is_empty() {
        emit_detail(
            seq,
            HttpDetail::Headers {
                hop,
                status,
                version: version.into(),
                headers: records,
            },
            emit,
        )?;
    }
    Ok(redacted)
}
struct Response {
    status: u16,
    location: Option<String>,
    content_type: String,
    body: Vec<u8>,
    truncated: bool,
    header_bytes: usize,
    headers_redacted: bool,
    headers_ms: f64,
    body_ms: f64,
    version: String,
}
async fn transaction(
    stream: Stream,
    endpoint: &Endpoint,
    method: &str,
    seq: u32,
    hop: u32,
    emit: &impl Fn(Event) -> Result<()>,
) -> Result<Response> {
    let (mut sender, connection) = hyper::client::conn::http1::Builder::new()
        .max_buf_size(HEADERS)
        .max_headers(128)
        .handshake(hyper_util::rt::TokioIo::new(stream))
        .await?;
    let mut path = endpoint.url.path().to_owned();
    if path.is_empty() {
        path.push('/');
    }
    if let Some(query) = endpoint.url.query() {
        path.push('?');
        path.push_str(query);
    }
    let request = http::Request::builder()
        .method(method)
        .uri(path)
        .header(http::header::HOST, endpoint.url.authority())
        .header(http::header::USER_AGENT, "astra-network-diagnostic")
        .header(http::header::ACCEPT_ENCODING, "identity")
        .body(Empty::<bytes::Bytes>::new())?;
    let response = async move {
        let started = Instant::now();
        let reply = sender
            .send_request(request)
            .await
            .context("HTTP response headers failed")?;
        let headers_ms = started.elapsed().as_secs_f64() * 1000.;
        let header_bytes = reply
            .headers()
            .iter()
            .map(|(name, value)| name.as_str().len() + value.as_bytes().len() + 4)
            .sum::<usize>();
        ensure!(
            header_bytes <= HEADERS,
            "HTTP response headers exceed 32 KiB"
        );
        let status = reply.status().as_u16();
        let version = format!("{:?}", reply.version());
        let location = reply
            .headers()
            .get(http::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let content_type = reply
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let headers_redacted = emit_headers(
            seq,
            hop,
            status,
            &version,
            reply.headers(),
            &endpoint.url,
            emit,
        )?;
        let mut body = reply.into_body();
        let mut bytes = Vec::new();
        let mut truncated = false;
        let body_start = Instant::now();
        // Redirect payloads and HEAD bodies are never needed or retained.
        if method != "HEAD" && !matches!(status, 301 | 302 | 303 | 307 | 308) {
            while let Some(frame) = body.frame().await {
                if let Ok(data) = frame.context("HTTP body read failed")?.into_data() {
                    let take = data.len().min(PREVIEW - bytes.len());
                    bytes.extend_from_slice(&data[..take]);
                    if take < data.len() {
                        truncated = true;
                        break;
                    }
                    if bytes.len() == PREVIEW {
                        truncated = !body.is_end_stream();
                        break;
                    }
                }
            }
        }
        Ok::<_, anyhow::Error>(Response {
            status,
            location,
            content_type,
            body: bytes,
            truncated,
            header_bytes,
            headers_redacted,
            headers_ms,
            body_ms: body_start.elapsed().as_secs_f64() * 1000.,
            version,
        })
    };
    tokio::pin!(response);
    tokio::select! { result=&mut response=>result,result=connection=>{result.context("HTTP connection failed")?;response.await} }
}
fn public_ip(bytes: &[u8]) -> Result<IpAddr> {
    if let Ok(text) = std::str::from_utf8(bytes) {
        if let Ok(ip) = text.trim().parse() {
            return Ok(ip);
        }
    }
    let value: serde_json::Value =
        serde_json::from_slice(bytes).context("HTTPS IP service did not return an IP address")?;
    let text = value
        .as_str()
        .or_else(|| {
            ["ip", "address", "origin"]
                .into_iter()
                .find_map(|key| value.get(key).and_then(|v| v.as_str()))
        })
        .context("HTTPS IP response has no single address field")?;
    text.trim()
        .parse()
        .context("HTTPS IP response is not a single IPv4 or IPv6 address")
}
pub(super) async fn run(
    route: &Route<'_>,
    seq: u32,
    url: &str,
    method: &str,
    compare: bool,
    observe_ip: bool,
    emit: &impl Fn(Event) -> Result<()>,
) -> Result<String> {
    let mut endpoint = Endpoint::parse(url)?;
    let method = method.to_ascii_uppercase();
    ensure!(
        matches!(method.as_str(), "GET" | "HEAD"),
        "HTTP diagnostic method must be GET or HEAD"
    );
    ensure!(
        !observe_ip || endpoint.url.scheme() == "https",
        "Public IP observation requires HTTPS"
    );
    for hop in 0..=5 {
        let started = Instant::now();
        let host = endpoint
            .url
            .host_str()
            .context("HTTP hostname missing")?
            .trim_matches(['[', ']']);
        let port = endpoint
            .url
            .port_or_known_default()
            .context("HTTP port missing")?;
        let connecting = Instant::now();
        let (stream, connect_timings) = match route
            .connect_timed(seq, "business_target", host, port, endpoint.bootstrap, emit)
            .await
        {
            Ok(value) => value,
            Err(error) => {
                emit_detail(
                    seq,
                    HttpDetail::Failure {
                        hop: Some(hop),
                        phase: "resolution / connection / proxy handshake".into(),
                        message: super::clean(&format!("{error:#}"), 2048),
                    },
                    emit,
                )?;
                return Err(error);
            }
        };
        let connect_ms = connecting.elapsed().as_secs_f64() * 1000.;
        let (stream, tls_ms): (Stream, Option<f64>) = if endpoint.url.scheme() == "https" {
            let start = Instant::now();
            let tls = match route
                .tls
                .connect_http(seq, host, stream, compare, emit)
                .await
            {
                Ok(value) => value,
                Err(error) => {
                    emit_detail(
                        seq,
                        HttpDetail::Failure {
                            hop: Some(hop),
                            phase: "TLS".into(),
                            message: super::clean(&format!("{error:#}"), 2048),
                        },
                        emit,
                    )?;
                    return Err(error);
                }
            };
            (Box::new(tls), Some(start.elapsed().as_secs_f64() * 1000.))
        } else {
            (stream, None)
        };
        let reply = match transaction(stream, &endpoint, &method, seq, hop, emit).await {
            Ok(value) => value,
            Err(error) => {
                emit_detail(
                    seq,
                    HttpDetail::Failure {
                        hop: Some(hop),
                        phase: "HTTP response".into(),
                        message: super::clean(&format!("{error:#}"), 2048),
                    },
                    emit,
                )?;
                return Err(error);
            }
        };
        let (mut text, mut redacted) = preview(&reply.content_type, &reply.body, reply.truncated);
        if text.len() > PREVIEW {
            let mut end = PREVIEW - 32;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            text.push_str(" [preview truncated]");
            redacted = true;
        }
        emit_detail(
            seq,
            HttpDetail::Response {
                hop,
                service: endpoint.service(),
                method: method.clone(),
                status: reply.status,
                version: reply.version,
                timings: HttpTimings {
                    connect_ms,
                    dns_ms: connect_timings.dns_ms,
                    transport_connect_ms: connect_timings.transport_connect_ms,
                    proxy_handshake_ms: connect_timings.proxy_handshake_ms,
                    tls_ms,
                    response_headers_ms: reply.headers_ms,
                    body_ms: reply.body_ms,
                    total_ms: started.elapsed().as_secs_f64() * 1000.,
                },
                header_bytes: reply.header_bytes as u32,
                preview_bytes: reply.body.len() as u32,
                body_truncated: reply.truncated,
                headers_redacted: reply.headers_redacted,
                preview_redacted: redacted,
            },
            emit,
        )?;
        let mut offset = 0;
        while offset < text.len() {
            let mut end = (offset + TEXT_CHUNK).min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            emit_detail(
                seq,
                HttpDetail::Preview {
                    hop,
                    offset: offset as u32,
                    text: text[offset..end].to_owned(),
                },
                emit,
            )?;
            offset = end;
        }
        if matches!(reply.status, 301 | 302 | 303 | 307 | 308) {
            let location = reply
                .location
                .context("Redirect response is missing Location")?;
            let mut next = endpoint
                .url
                .join(&location)
                .map_err(|_| anyhow::anyhow!("Invalid redirect Location"))?;
            // Bootstrap is user input, never authority granted by a redirect.
            next.set_fragment(None);
            let next = Endpoint::parse(next.as_str())?;
            let downgrade = endpoint.url.scheme() == "https" && next.url.scheme() == "http";
            let followed = !downgrade && hop < 5;
            emit_detail(
                seq,
                HttpDetail::Redirect {
                    hop,
                    from: endpoint.service(),
                    to: next.service(),
                    followed,
                    reason: if downgrade {
                        "HTTPS downgrade refused"
                    } else if hop == 5 {
                        "Redirect limit reached (5)"
                    } else {
                        "Redirect follows the same pinned egress and resolver constraints"
                    }
                    .into(),
                },
                emit,
            )?;
            ensure!(!downgrade, "HTTPS downgrade redirect refused");
            ensure!(hop < 5, "HTTP redirect limit reached (5)");
            // A bootstrap is a claim about one origin only. Same-origin hops
            // retain it; a different service must pass the resolver boundary.
            let bootstrap = if endpoint.url.origin() == next.url.origin() {
                next.bootstrap.or(endpoint.bootstrap)
            } else {
                next.bootstrap
            };
            endpoint = Endpoint {
                url: next.url,
                bootstrap,
            };
            continue;
        }
        if observe_ip {
            ensure!(
                (200..300).contains(&reply.status),
                "HTTPS IP service returned an unsuccessful status"
            );
            ensure!(!reply.truncated, "HTTPS IP response exceeds body limit");
            let ip = public_ip(&reply.body)?;
            emit(Event::PublicIp {
                seq,
                address: ip.to_string(),
                source: "https".into(),
                service: endpoint.service(),
            })?;
            return Ok(format!("HTTPS observed address {ip}"));
        }
        return Ok(format!("HTTP {} from {}", reply.status, endpoint.service()));
    }
    bail!("HTTP redirect limit reached")
}
#[cfg(test)]
#[path = "../test/nettool_http_test.rs"]
mod tests;
