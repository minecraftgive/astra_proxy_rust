use super::*;
#[test]
fn monitor_reports_real_session_and_buffer_permits() {
    let source = "max_connections=10\nmax_udp_flows=8\n[[listeners]]\ntag='local'\nprotocol='socks5'\nlisten='127.0.0.1:0'\n[[listeners.users]]\nname='user'\npassword='secret'\n";
    let config = toml::from_str(source).unwrap();
    let app = App::new(
        config,
        Arc::new(Stats::default()),
        Arc::new(Semaphore::new(10)),
        Arc::new(Semaphore::new(8)),
        Arc::new(Semaphore::new(BUFFER_BUDGET)),
    );
    let session = app.session().unwrap();
    let buffer = app.bytes.try_acquire_many(4096).unwrap();
    let sample = internal_snapshot(&app, 1, 1, 3);
    let value = |name: &str| {
        sample
            .metrics
            .iter()
            .find(|m| m.name == name)
            .unwrap()
            .value
            .as_str()
    };
    assert_eq!(value("server.sessions.active"), "1");
    assert_eq!(value("server.sessions.available"), "9");
    assert_eq!(value("server.udp.buffer_used"), "4096");
    assert_eq!(value("server.sessions.accepted"), "1");
    drop(buffer);
    drop(session);
    let sample = internal_snapshot(&app, 1, 0, 3);
    assert_eq!(
        sample
            .metrics
            .iter()
            .find(|m| m.name == "server.sessions.active")
            .unwrap()
            .value,
        "0"
    );
    assert_eq!(
        sample
            .metrics
            .iter()
            .find(|m| m.name == "server.udp.buffer_used")
            .unwrap()
            .value,
        "0"
    );
}
#[tokio::test]
async fn monitor_server_command_returns_internal_json_without_changing_status_layout() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("server.toml");
    let endpoint = directory.path().join("control.sock");
    let reservation = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = reservation.local_addr().unwrap();
    let prepared = config::prepare_text(&config, &format!("shutdown_timeout_secs=0\n[[listeners]]\ntag='local'\nprotocol='socks5'\nlisten='{address}'\n[[listeners.users]]\nname='user'\npassword='secret'\n")).unwrap();
    drop(reservation);
    let task = tokio::spawn(run(config, endpoint.clone(), prepared));
    let reply = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(reply) = astra_management::server::call(&endpoint, Action::Monitor).await {
                break reply;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(reply.error.is_none());
    let internal: astra_model::monitor::InternalSnapshot =
        serde_json::from_str(&reply.logs).unwrap();
    assert_eq!(internal.epoch, astra_platform::lifecycle::boot_id());
    assert!(internal
        .metrics
        .iter()
        .any(|m| m.name == "server.udp.buffer_capacity"));
    assert!(!reply.logs.contains("secret"));
    let status = astra_management::server::call(&endpoint, Action::Status)
        .await
        .unwrap();
    assert_eq!(status.pid, reply.pid);
    assert!(status.logs.is_empty());
    astra_management::server::call(&endpoint, Action::Stop)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
#[tokio::test]
async fn panicked_listener_fails_service_and_releases_control_and_ingress() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("server.toml");
    let endpoint = directory.path().join("control.sock");
    let reservation = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = reservation.local_addr().unwrap();
    let source = format!("[[listeners]]\ntag='local'\nprotocol='socks5'\nlisten='{address}'\n[[listeners.users]]\nname='user'\npassword='secret'\n");
    let prepared = config::prepare_text(&path, &source).unwrap();
    drop(reservation);
    let fault = Task::spawn("injected server listener", async {
        panic!("listener fault injection")
    });
    let error = tokio::time::timeout(
        Duration::from_secs(5),
        run_supervised(path, endpoint.clone(), prepared, vec![fault]),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(error.to_string().contains("injected server listener"));
    assert!(!endpoint.exists());
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let Ok(listener) = tokio::net::TcpListener::bind(address).await {
                drop(listener);
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
