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

fn audit_state(directory: &std::path::Path, backend: Arc<dyn Backend>) -> AppState {
    let config = config::Config {
        manager_socket: directory.join("m.sock"),
        state_dir: directory.to_owned(),
        credentials_file: None,
        listen: "127.0.0.1:8787".parse().unwrap(),
        public_origin: "http://127.0.0.1:8787".into(),
        tls: None,
        dev_http: true,
    };
    auth::set_password(&config.password_file(), "integration password").unwrap();
    AppState::new(&config, backend).unwrap()
}

struct AuditedBackend(std::path::PathBuf);
#[async_trait::async_trait]
impl Backend for AuditedBackend {
    async fn request(&self, request: v2::Request) -> anyhow::Result<v2::Response> {
        let records = std::fs::read_to_string(&self.0).unwrap();
        let record: serde_json::Value =
            serde_json::from_str(records.lines().last().unwrap()).unwrap();
        assert_eq!(record["event"], "query");
        assert_eq!(record["request_id"], request.id);
        anyhow::bail!("backend unavailable")
    }
}

#[tokio::test]
async fn audit_completes_before_backend_and_records_backend_failure_in_order() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("audit.jsonl");
    let state = audit_state(directory.path(), Arc::new(AuditedBackend(path.clone())));
    let token = state
        .auth
        .login("integration password".into())
        .await
        .unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        format!("Bearer {token}").parse().unwrap(),
    );
    let response = request(
        State(state),
        headers,
        Json(v2::Request::new(
            "audit-order",
            v2::RequestBody::Query(v2::Query::Agents),
        )),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let records: Vec<serde_json::Value> = std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["event"], "query");
    assert_eq!(records[1]["event"], "manager_unavailable");
    for record in records {
        assert_eq!(record["request_id"], "audit-order");
        assert_eq!(record.as_object().unwrap().len(), 3);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn waiting_for_audit_lock_does_not_block_the_async_worker() {
    let directory = tempfile::tempdir().unwrap();
    let state = audit_state(directory.path(), Arc::new(Stub));
    let audit = Arc::clone(&state.audit);
    let (locked, ready) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let owner = std::thread::spawn(move || {
        let _guard = audit.lock().unwrap();
        locked.send(()).unwrap();
        // A bound prevents a synchronous-lock regression from hanging the suite.
        released.recv_timeout(Duration::from_secs(5)).is_ok()
    });
    ready.await.unwrap();
    let writer = tokio::spawn(async move { state.audit("query", Some("blocked-write")).await });
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    let finished_while_locked = writer.is_finished();
    let _ = release.send(());
    assert!(
        owner.join().unwrap(),
        "audit blocked the async worker until the watchdog fired"
    );
    assert!(
        !finished_while_locked,
        "audit returned before its write completed"
    );
    writer.await.unwrap();
    let record: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(directory.path().join("audit.jsonl")).unwrap(),
    )
    .unwrap();
    assert_eq!(record["request_id"], "blocked-write");
}

#[tokio::test]
async fn offloaded_audit_preserves_rotation_and_nonfatal_write_errors() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("audit.jsonl");
    let previous = path.with_extension("previous.jsonl");
    std::fs::File::create(&path)
        .unwrap()
        .set_len(4 * 1024 * 1024)
        .unwrap();
    std::fs::write(&previous, b"old rotation").unwrap();
    let state = audit_state(directory.path(), Arc::new(Stub));
    state.audit("query", Some("rotated")).await;
    assert_eq!(std::fs::metadata(&previous).unwrap().len(), 4 * 1024 * 1024);
    let record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(record["request_id"], "rotated");

    drop(state);
    std::fs::File::create(&path)
        .unwrap()
        .set_len(4 * 1024 * 1024)
        .unwrap();
    std::fs::remove_file(&previous).unwrap();
    std::fs::create_dir(&previous).unwrap();
    let state = audit_state(directory.path(), Arc::new(Stub));
    // Rotation fails because the previous-file path is a directory. Audit errors
    // remain nonfatal and do not prevent the operation's original dispatch.
    let token = state
        .auth
        .login("integration password".into())
        .await
        .unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        format!("Bearer {token}").parse().unwrap(),
    );
    let response = request(
        State(state),
        headers,
        Json(v2::Request::new(
            "write-failed",
            v2::RequestBody::Query(v2::Query::Agents),
        )),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(previous.is_dir());
}
