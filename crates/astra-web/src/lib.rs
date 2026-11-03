mod assets;
mod audit;
pub mod auth;
pub mod config;

use astra_management::{v2, ManagerClient};
use axum::{
    extract::{ConnectInfo, DefaultBodyLimit, Path, Query, State},
    http::{header, HeaderMap, Request, StatusCode},
    middleware::{self, Next},
    response::{
        sse::{Event, KeepAlive},
        IntoResponse, Response, Sse,
    },
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use std::{
    convert::Infallible,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

#[async_trait::async_trait]
pub trait Backend: Send + Sync {
    async fn request(&self, request: v2::Request) -> anyhow::Result<v2::Response>;
}
#[async_trait::async_trait]
impl Backend for ManagerClient {
    async fn request(&self, request: v2::Request) -> anyhow::Result<v2::Response> {
        self.request_v2(request).await
    }
}
#[derive(Clone)]
pub struct AppState {
    auth: Arc<auth::Auth>,
    backend: Arc<dyn Backend>,
    origin: String,
    authority: String,
    audit: Arc<Mutex<audit::Audit>>,
    event_streams: Arc<tokio::sync::Semaphore>,
}
impl AppState {
    pub fn new(config: &config::Config, backend: Arc<dyn Backend>) -> anyhow::Result<Self> {
        config.validate()?;
        let auth = Arc::new(auth::Auth::new(config.password_file())?);
        let audit = audit::Audit::new(&config.state_dir.join("audit.jsonl"))?;
        Ok(Self {
            auth,
            backend,
            origin: config.public_origin.clone(),
            authority: config
                .public_origin
                .parse::<axum::http::Uri>()?
                .authority()
                .unwrap()
                .to_string(),
            audit: Arc::new(Mutex::new(audit)),
            event_streams: Arc::new(tokio::sync::Semaphore::new(64)),
        })
    }
    fn audit(&self, event: &str, id: Option<&str>) {
        // Deliberately never serialize commands, document text, passwords or tokens.
        if let Ok(mut audit) = self.audit.lock() {
            if let Err(error) = audit.record(event, id) {
                eprintln!("Audit metadata write failed: {error}");
            }
        }
    }
}
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/v2/login", post(login))
        .route("/api/v2/logout", post(logout))
        .route("/api/v2/auth/login", post(login))
        .route("/api/v2/auth/logout", post(logout))
        .route("/api/v2/nodes", get(nodes))
        .route("/api/v2/nodes/{agent}/instances", get(instances))
        .route("/api/v2/nodes/{agent}/query", post(node_query))
        .route("/api/v2/nodes/{agent}/operations", post(operation))
        .route("/api/v2/nodes/{agent}/tasks/{operation}", get(task))
        .route("/api/v2/nodes/{agent}/drafts", post(drafts))
        .route("/api/v2/nodes/{agent}/artifacts", post(artifacts))
        .route("/api/v2/request", post(request))
        .route("/api/v2/events", get(events))
        .route(
            "/health",
            get(|| async { Json(serde_json::json!({"status":"ready"})) }),
        )
        .fallback(assets::serve)
        .layer(DefaultBodyLimit::max(v2::MAX_FRAME_BYTES))
        .layer(middleware::from_fn_with_state(state.clone(), security))
        .with_state(state)
}
async fn security(
    State(state): State<AppState>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|s| s.to_str().ok())
        .or_else(|| req.uri().authority().map(|a| a.as_str()));
    if host != Some(state.authority.as_str()) {
        return error(
            StatusCode::FORBIDDEN,
            "unrecognized_host",
            "Request host does not match public_origin",
        );
    }
    if let Some(origin) = req.headers().get(header::ORIGIN) {
        if origin.to_str().ok() != Some(state.origin.as_str()) {
            return error(
                StatusCode::FORBIDDEN,
                "cross_origin",
                "Cross-origin management is disabled",
            );
        }
    }
    let mut response = next.run(req).await;
    let headers = response.headers_mut();
    headers.insert("x-content-type-options", "nosniff".parse().unwrap());
    headers.insert("referrer-policy", "no-referrer".parse().unwrap());
    headers.insert("content-security-policy","default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'".parse().unwrap());
    if state.origin.starts_with("https:") {
        headers.insert(
            "strict-transport-security",
            "max-age=31536000".parse().unwrap(),
        );
    }
    response
}
fn error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({"error":{"code":code,"message":message}})),
    )
        .into_response()
}
fn token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}
fn authorized(state: &AppState, headers: &HeaderMap) -> bool {
    token(headers).is_some_and(|t| state.auth.verify(t))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Login {
    password: String,
}
async fn login(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(input): Json<Login>,
) -> Response {
    if !state.auth.permit_attempt(peer.ip()) {
        state.audit("login_rate_limited", None);
        return error(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "Too many login attempts; wait one minute",
        );
    }
    match state.auth.login(input.password).await {
        Ok(token) => {
            state.audit("login", None);
            (
                StatusCode::OK,
                [(header::CACHE_CONTROL, "no-store")],
                Json(serde_json::json!({"token":token,"expires_in":28800})),
            )
                .into_response()
        }
        Err(_) => {
            state.audit("login_failed", None);
            error(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "Invalid credentials or session capacity exceeded",
            )
        }
    }
}
async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !authorized(&state, &headers) {
        return error(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Sign in to continue",
        );
    }
    state.auth.logout(token(&headers).unwrap());
    state.audit("logout", None);
    StatusCode::NO_CONTENT.into_response()
}
async fn request(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<v2::Request>,
) -> Response {
    if !authorized(&state, &headers) {
        return error(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Sign in to continue",
        );
    }
    if matches!(&input.body, v2::RequestBody::Local(_)) {
        return error(
            StatusCode::FORBIDDEN,
            "requires_local_action",
            "Interactive privilege operations require a terminal on the target machine",
        );
    }
    if input.version != v2::VERSION || input.id.len() > 128 || input.id.is_empty() {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Expected management v2 and a request identity",
        );
    }
    let id = input.id.clone();
    let kind = match &input.body {
        v2::RequestBody::Query(_) => "query",
        v2::RequestBody::Execute { .. } => "execute",
        v2::RequestBody::Drafts(_) => "draft",
        v2::RequestBody::Artifacts(_) => "artifact",
        v2::RequestBody::Local(_) => unreachable!(),
    };
    state.audit(kind, Some(&id));
    match state.backend.request(input).await {
        Ok(response) => ([(header::CACHE_CONTROL, "no-store")], Json(response)).into_response(),
        Err(_) => {
            state.audit("manager_unavailable", Some(&id));
            error(StatusCode::BAD_GATEWAY,"outcome_unknown","Manager reply unavailable. Query the original operation ID before retrying a change.")
        }
    }
}
fn node_request(agent: Option<String>, body: v2::RequestBody) -> v2::Request {
    let mut entropy = [0u8; 16];
    getrandom::getrandom(&mut entropy).expect("OS random generator");
    let mut request = v2::Request::new(hex::encode(entropy), body);
    request.agent_id = agent;
    request
}
async fn nodes(State(state): State<AppState>, headers: HeaderMap) -> Response {
    request(
        State(state),
        headers,
        Json(node_request(
            None,
            v2::RequestBody::Query(v2::Query::Agents),
        )),
    )
    .await
}
async fn instances(
    State(state): State<AppState>,
    Path(agent): Path<String>,
    headers: HeaderMap,
) -> Response {
    request(
        State(state),
        headers,
        Json(node_request(
            Some(agent),
            v2::RequestBody::Query(v2::Query::Instances),
        )),
    )
    .await
}
async fn node_query(
    State(state): State<AppState>,
    Path(agent): Path<String>,
    headers: HeaderMap,
    Json(query): Json<v2::Query>,
) -> Response {
    request(
        State(state),
        headers,
        Json(node_request(Some(agent), v2::RequestBody::Query(query))),
    )
    .await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Operation {
    operation_id: String,
    command: v2::Command,
}
async fn operation(
    State(state): State<AppState>,
    Path(agent): Path<String>,
    headers: HeaderMap,
    Json(op): Json<Operation>,
) -> Response {
    request(
        State(state),
        headers,
        Json(node_request(
            Some(agent),
            v2::RequestBody::Execute {
                operation_id: op.operation_id,
                command: op.command,
            },
        )),
    )
    .await
}
async fn task(
    State(state): State<AppState>,
    Path((agent, operation_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    request(
        State(state),
        headers,
        Json(node_request(
            Some(agent),
            v2::RequestBody::Query(v2::Query::Task { operation_id }),
        )),
    )
    .await
}
async fn drafts(
    State(state): State<AppState>,
    Path(agent): Path<String>,
    headers: HeaderMap,
    Json(command): Json<v2::DraftCommand>,
) -> Response {
    request(
        State(state),
        headers,
        Json(node_request(Some(agent), v2::RequestBody::Drafts(command))),
    )
    .await
}
async fn artifacts(
    State(state): State<AppState>,
    Path(agent): Path<String>,
    headers: HeaderMap,
    Json(command): Json<v2::ArtifactCommand>,
) -> Response {
    request(
        State(state),
        headers,
        Json(node_request(
            Some(agent),
            v2::RequestBody::Artifacts(command),
        )),
    )
    .await
}
#[derive(Deserialize)]
struct EventQuery {
    agent_id: Option<String>,
    after: Option<String>,
}
async fn events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<EventQuery>,
) -> Response {
    if !authorized(&state, &headers) {
        return error(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Sign in to continue",
        );
    }
    let Ok(permit) = state.event_streams.clone().try_acquire_owned() else {
        return error(
            StatusCode::TOO_MANY_REQUESTS,
            "capacity",
            "Too many event subscribers",
        );
    };
    let token = token(&headers).unwrap().to_owned();
    let stream = futures_util::stream::unfold(
        (state, token, query, false, permit),
        |(state, token, mut query, ended, permit)| async move {
            if ended {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
            if !state.auth.verify(&token) {
                return Some((
                    Ok::<_, Infallible>(Event::default().event("expired").data("Sign in again")),
                    (state, token, query, true, permit),
                ));
            }
            let mut request = v2::Request::new(
                "web-events",
                v2::RequestBody::Query(v2::Query::Events {
                    after: query.after.clone(),
                    limit: Some(100),
                }),
            );
            request.agent_id = query.agent_id.clone();
            let event =
                match tokio::time::timeout(Duration::from_secs(10), state.backend.request(request))
                    .await
                {
                    Ok(Ok(response)) => {
                        if let Ok(v2::ResponseBody::Events(batch)) = &response.result {
                            query.after = Some(batch.cursor.clone());
                        }
                        Event::default()
                            .event("batch")
                            .json_data(response)
                            .unwrap_or_else(|_| {
                                Event::default().event("error").data("Invalid event batch")
                            })
                    }
                    _ => Event::default()
                        .event("disconnected")
                        .data("Manager unavailable; state is stale"),
                };
            Some((
                Ok::<_, Infallible>(event),
                (state, token, query, false, permit),
            ))
        },
    );
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

pub async fn run(config: config::Config) -> anyhow::Result<()> {
    // Workspace builds can enable both providers through different components.
    // This independent gateway chooses one before Rustls loads its certificate.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let backend = Arc::new(ManagerClient::new(config.manager_socket.clone()));
    let state = AppState::new(&config, backend)?;
    let service = router(state).into_make_service_with_connect_info::<SocketAddr>();
    if let Some(tls) = &config.tls {
        let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(
            &tls.certificate,
            &tls.private_key,
        )
        .await?;
        let handle = axum_server::Handle::new();
        let server = axum_server::bind_rustls(config.listen, tls)
            .handle(handle.clone())
            .serve(service);
        serve_until_shutdown(
            server,
            astra_platform::platform::shutdown_signal(),
            move || handle.graceful_shutdown(Some(Duration::from_secs(5))),
            Duration::from_secs(5),
        )
        .await?;
    } else {
        use std::future::IntoFuture;
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let server = axum::serve(tokio::net::TcpListener::bind(config.listen).await?, service)
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .into_future();
        serve_until_shutdown(
            server,
            astra_platform::platform::shutdown_signal(),
            move || {
                let _ = stop.send(());
            },
            Duration::from_secs(5),
        )
        .await?;
    }
    Ok(())
}

async fn serve_until_shutdown(
    server: impl std::future::Future<Output = std::io::Result<()>>,
    signal: impl std::future::Future<Output = std::io::Result<()>>,
    stop: impl FnOnce(),
    grace: Duration,
) -> anyhow::Result<()> {
    tokio::pin!(server);
    let signal = tokio::select! {
        result = &mut server => return result.map_err(Into::into),
        result = signal => result,
    };
    stop();
    let drained = tokio::time::timeout(grace, server).await;
    match (signal, drained) {
        (Ok(()), Ok(result)) => result.map_err(Into::into),
        (signal, result) => {
            anyhow::bail!("Gateway shutdown incomplete: signal={signal:?}; drain={result:?}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;
    #[tokio::test]
    async fn shutdown_stops_accepting_and_waits_for_in_flight_work() {
        let (stop, stopped) = tokio::sync::oneshot::channel();
        serve_until_shutdown(
            async {
                stopped.await.unwrap();
                Ok(())
            },
            async { Ok(()) },
            || {
                stop.send(()).unwrap();
            },
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        let error = serve_until_shutdown(
            std::future::pending(),
            async { Ok(()) },
            || {},
            Duration::from_millis(10),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("shutdown incomplete"));
    }
    struct Stub;
    #[async_trait::async_trait]
    impl Backend for Stub {
        async fn request(&self, r: v2::Request) -> anyhow::Result<v2::Response> {
            Ok(v2::Response::success(
                r.id,
                v2::ResponseBody::Agents(vec![]),
            ))
        }
    }
    #[tokio::test]
    async fn auth_and_origin_guards_precede_backend_access() {
        let dir = tempfile::tempdir().unwrap();
        let config = config::Config {
            manager_socket: dir.path().join("m.sock"),
            state_dir: dir.path().to_owned(),
            credentials_file: None,
            listen: "127.0.0.1:8787".parse().unwrap(),
            public_origin: "http://127.0.0.1:8787".into(),
            tls: None,
            dev_http: true,
        };
        auth::set_password(&config.password_file(), "integration password").unwrap();
        let state = AppState::new(&config, Arc::new(Stub)).unwrap();
        let app = router(state.clone());
        let body = serde_json::to_vec(&v2::Request::new(
            "test",
            v2::RequestBody::Query(v2::Query::Agents),
        ))
        .unwrap();
        let req = || {
            Request::builder()
                .method("POST")
                .uri("/api/v2/request")
                .header("host", "127.0.0.1:8787")
                .header("content-type", "application/json")
        };
        assert_eq!(
            app.clone()
                .oneshot(req().body(axum::body::Body::from(body.clone())).unwrap())
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let token = state
            .auth
            .login("integration password".into())
            .await
            .unwrap();
        assert_eq!(
            app.clone()
                .oneshot(
                    req()
                        .header("authorization", format!("Bearer {token}"))
                        .header("origin", "https://attacker.test")
                        .body(axum::body::Body::from(body.clone()))
                        .unwrap()
                )
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            app.clone()
                .oneshot(
                    req()
                        .header("authorization", format!("Bearer {token}"))
                        .body(axum::body::Body::from(body))
                        .unwrap()
                )
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        let privileged = serde_json::to_vec(&v2::Request::new(
            "local-only",
            v2::RequestBody::Local(v2::LocalCommand::EnableTun {
                instance: "test".into(),
                expected_pid: None,
            }),
        ))
        .unwrap();
        assert_eq!(
            app.oneshot(
                req()
                    .header("authorization", format!("Bearer {token}"))
                    .body(axum::body::Body::from(privileged))
                    .unwrap()
            )
            .await
            .unwrap()
            .status(),
            StatusCode::FORBIDDEN
        );
        let audit = std::fs::read_to_string(dir.path().join("audit.jsonl")).unwrap();
        assert!(!audit.contains(&token));
        assert!(!audit.contains("integration password"));
    }
}
