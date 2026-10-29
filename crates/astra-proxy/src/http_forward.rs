//! Streaming HTTP/1 forward proxy for all supported platforms.
//! CONNECT is handled by the existing bounded tunnel implementation.
use crate::App;
use astra_ipc::{Inbound, RouteAction, Target};
use bytes::Bytes;
use http::{header, HeaderMap, HeaderValue, Request, Response, StatusCode, Version};
use http_body_util::{combinators::UnsyncBoxBody, BodyExt, Full};
use hyper::{body::Incoming, service::service_fn};
use hyper_util::rt::{TokioIo, TokioTimer};
use std::{
    convert::Infallible,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

type Error = Box<dyn std::error::Error + Send + Sync>;
type Body = UnsyncBoxBody<Bytes, Error>;

struct ConnectionTask(tokio::task::JoinHandle<()>);
impl Drop for ConnectionTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}
struct ForwardBody {
    incoming: Incoming,
    _connection: ConnectionTask,
}
impl hyper::body::Body for ForwardBody {
    type Data = Bytes;
    type Error = hyper::Error;
    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<hyper::body::Frame<Bytes>, Self::Error>>> {
        Pin::new(&mut self.get_mut().incoming).poll_frame(cx)
    }
    fn is_end_stream(&self) -> bool {
        hyper::body::Body::is_end_stream(&self.incoming)
    }
    fn size_hint(&self) -> hyper::body::SizeHint {
        hyper::body::Body::size_hint(&self.incoming)
    }
}

fn response(status: StatusCode, message: &'static str) -> Response<Body> {
    let body = Full::new(Bytes::from_static(message.as_bytes()))
        .map_err(|never: Infallible| -> Error { match never {} })
        .boxed_unsync();
    let mut response = Response::new(body);
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("close"));
    response
}
fn strip_hop_headers(headers: &mut HeaderMap) {
    let nominated: Vec<_> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|v| header::HeaderName::from_bytes(v.trim().as_bytes()).ok())
        .collect();
    for name in nominated {
        headers.remove(name);
    }
    for name in [
        "connection",
        "proxy-connection",
        "keep-alive",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "proxy-authorization",
        "proxy-authenticate",
    ] {
        headers.remove(name);
    }
}

async fn forward(
    mut request: Request<Incoming>,
    app: Arc<App>,
    source: Option<std::net::SocketAddr>,
    destination: Option<std::net::SocketAddr>,
) -> Response<Body> {
    let auth: Vec<_> = request
        .headers()
        .get_all(header::PROXY_AUTHORIZATION)
        .iter()
        .map(|v| v.to_str().unwrap_or(""))
        .collect();
    if !crate::inbound::http_authorized(&auth, app.authentication(source.map(|s| s.ip()))) {
        let mut denied = response(
            StatusCode::PROXY_AUTHENTICATION_REQUIRED,
            "Proxy authentication required",
        );
        denied.headers_mut().insert(
            header::PROXY_AUTHENTICATE,
            HeaderValue::from_static("Basic realm=\"Astra\""),
        );
        return denied;
    }
    if request.method() == http::Method::CONNECT || request.headers().contains_key(header::UPGRADE)
    {
        return response(
            StatusCode::NOT_IMPLEMENTED,
            "Use a dedicated CONNECT connection for tunnels and upgrades",
        );
    }
    let uri = request.uri().clone();
    let Some(authority) = uri.authority() else {
        return response(
            StatusCode::BAD_REQUEST,
            "Proxy requests require an absolute HTTP URL",
        );
    };
    if uri.scheme_str() != Some("http") || authority.as_str().contains('@') {
        return response(
            StatusCode::BAD_REQUEST,
            "Only absolute http:// requests are accepted; use CONNECT for HTTPS",
        );
    }
    let Some(host) = uri.host() else {
        return response(StatusCode::BAD_REQUEST, "Missing destination host");
    };
    let target = Target {
        host: host.trim_matches(['[', ']']).to_owned(),
        port: uri.port_u16().unwrap_or(80),
    };
    if target.host.is_empty() || target.port == 0 {
        return response(StatusCode::BAD_REQUEST, "Invalid destination");
    }
    let action = match app
        .route_connection(
            &target,
            Inbound::HttpConnect,
            astra_ipc::Network::Tcp,
            source,
            destination,
        )
        .await
    {
        Ok(RouteAction::Block) => {
            return response(StatusCode::FORBIDDEN, "Blocked by routing policy")
        }
        Ok(action) => action,
        Err(error) => {
            tracing::debug!(%error, "HTTP forward routing failed");
            return response(StatusCode::SERVICE_UNAVAILABLE, "Routing unavailable");
        }
    };
    let remote = match app
        .connect_tracked(&target, &action, Inbound::HttpConnect, source)
        .await
    {
        Ok(remote) => remote,
        Err(error) => {
            tracing::debug!(%error, "HTTP forward connect failed");
            return response(StatusCode::BAD_GATEWAY, "Upstream connection failed");
        }
    };
    // Hyper validates HTTP framing and streams bodies with backpressure. A fresh
    // upstream and one request per client connection avoid cross-origin reuse.
    let (mut sender, connection) = match hyper::client::conn::http1::Builder::new()
        .max_buf_size(8192)
        .handshake(TokioIo::new(remote))
        .await
    {
        Ok(value) => value,
        Err(_) => return response(StatusCode::BAD_GATEWAY, "HTTP upstream handshake failed"),
    };
    let task = ConnectionTask(tokio::spawn(async move {
        let _ = connection.await;
    }));
    *request.uri_mut() = uri
        .path_and_query()
        .map_or("/", |v| v.as_str())
        .parse()
        .expect("validated URI path");
    *request.version_mut() = Version::HTTP_11;
    strip_hop_headers(request.headers_mut());
    let host = match HeaderValue::from_str(authority.as_str()) {
        Ok(host) => host,
        Err(_) => {
            return response(StatusCode::BAD_REQUEST, "Invalid Host");
        }
    };
    request.headers_mut().insert(header::HOST, host);
    request
        .headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("close"));
    match sender.send_request(request).await {
        Ok(mut upstream) if upstream.status() != StatusCode::SWITCHING_PROTOCOLS => {
            strip_hop_headers(upstream.headers_mut());
            upstream
                .headers_mut()
                .insert(header::CONNECTION, HeaderValue::from_static("close"));
            upstream.map(move |incoming| {
                ForwardBody {
                    incoming,
                    _connection: task,
                }
                .map_err(|error| -> Error { Box::new(error) })
                .boxed_unsync()
            })
        }
        _ => response(StatusCode::BAD_GATEWAY, "Invalid HTTP upstream response"),
    }
}

pub async fn handle(client: tokio::net::TcpStream, app: Arc<App>) -> anyhow::Result<()> {
    let source = client.peer_addr().ok();
    let destination = client.local_addr().ok();
    hyper::server::conn::http1::Builder::new()
        .keep_alive(false)
        .half_close(true)
        .max_headers(100)
        .max_buf_size(8192)
        .timer(TokioTimer::new())
        .header_read_timeout(Duration::from_secs(10))
        .serve_connection(
            TokioIo::new(client),
            service_fn(move |request| {
                let app = app.clone();
                async move { Ok::<_, Infallible>(forward(request, app, source, destination).await) }
            }),
        )
        .await?;
    Ok(())
}
