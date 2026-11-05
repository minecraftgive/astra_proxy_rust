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
