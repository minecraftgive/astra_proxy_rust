use super::*;
#[test]
fn cancellation_is_remembered_before_a_late_offer_and_blocks_execution() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let active = Arc::new(AtomicBool::new(true));
    let control = Control::new(
        runtime.handle().clone(),
        PathBuf::from("/nonexistent/astra-elevation-test"),
        active.clone(),
    );
    control.cancel();
    control.register(Ticket {
        name: "test".into(),
        ticket: "test-only".into(),
    });
    assert!(control.cancelled());
    assert!(control.check().is_err());
    assert!(control.ticket.lock().unwrap().is_none());
    assert!(!active.load(Ordering::Acquire));
}
#[test]
fn dropping_an_approval_guard_releases_the_global_frontend_lock() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let active = Arc::new(AtomicBool::new(true));
    {
        let _control = Control::new(runtime.handle().clone(), PathBuf::new(), active.clone());
    }
    assert!(!active.load(Ordering::Acquire));
}
#[test]
fn a_stale_completed_guard_cannot_unlock_a_new_request() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let active = Arc::new(AtomicBool::new(true));
    let first = Control::new(runtime.handle().clone(), PathBuf::new(), active.clone());
    first.complete();
    active.store(true, Ordering::Release);
    drop(first);
    assert!(active.load(Ordering::Acquire));
}

#[cfg(unix)]
#[tokio::test]
async fn shutdown_revokes_the_exact_late_offer_and_waits_for_the_cancel_reply() {
    use astra_management::client::{receive, send};
    use astra_platform::platform::LocalListener;
    use tokio::sync::oneshot;
    let directory = std::env::temp_dir().join(format!("astra-late-offer-{}", std::process::id()));
    astra_platform::storage::private_dir(&directory).unwrap();
    let socket = directory.join("management.sock");
    let listener = LocalListener::bind(&socket).unwrap();
    astra_platform::platform::set_private(&socket, 0o600).unwrap();
    let (prepared_tx, prepared_rx) = oneshot::channel();
    let (offer_tx, offer_rx) = oneshot::channel();
    let (cancelled_tx, cancelled_rx) = oneshot::channel();
    let (ack_tx, ack_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request: v::Request = receive(&mut stream).await.unwrap();
        assert!(
            matches!(request.body, v::RequestBody::Local(v::LocalCommand::PrepareElevation { instance, expected_pid: Some(71) }) if instance == "late-core")
        );
        prepared_tx.send(()).unwrap();
        offer_rx.await.unwrap();
        let offer = astra_platform::unix_handoff::Offer {
            version: astra_platform::unix_handoff::PROTOCOL_VERSION,
            socket: PathBuf::from("/unused/test-only"),
            ticket: "late-test-ticket".into(),
            binary: PathBuf::from("/must-not-launch/test-only"),
            sudo: PathBuf::from("/must-not-launch/sudo"),
        };
        send(
            &mut stream,
            &v::Response::success(
                request.id,
                v::ResponseBody::Local(serde_json::to_value(offer).unwrap()),
            ),
        )
        .await
        .unwrap();
        let (mut stream, _) = listener.accept().await.unwrap();
        let request: v::Request = receive(&mut stream).await.unwrap();
        assert!(
            matches!(request.body, v::RequestBody::Local(v::LocalCommand::CancelElevation { instance, ticket }) if instance == "late-core" && ticket == "late-test-ticket")
        );
        cancelled_tx.send(()).unwrap();
        ack_rx.await.unwrap();
        send(
            &mut stream,
            &v::Response::success(request.id, v::ResponseBody::Local(serde_json::Value::Null)),
        )
        .await
        .unwrap();
    });
    let active = Arc::new(AtomicBool::new(true));
    let control = Arc::new(Control::new(
        tokio::runtime::Handle::current(),
        socket.clone(),
        active,
    ));
    let task = tokio::spawn(prepare(
        Request::Tun {
            name: "late-core".into(),
            expected_pid: Some(71),
        },
        control.clone(),
    ));
    tokio::time::timeout(std::time::Duration::from_secs(2), prepared_rx)
        .await
        .unwrap()
        .unwrap();
    let closing = tokio::spawn(shutdown());
    while !control.cancelled() {
        tokio::task::yield_now().await;
    }
    assert!(!closing.is_finished());
    offer_tx.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), cancelled_rx)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !closing.is_finished(),
        "exit must drain the actual Cancel RPC, not only enqueue it"
    );
    ack_tx.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), closing)
        .await
        .unwrap()
        .unwrap();
    assert!(task.await.unwrap().is_err());
    server.await.unwrap();
    astra_platform::platform::remove_socket(&socket);
    std::fs::remove_dir_all(directory).unwrap();
}
