use super::*;

#[tokio::test]
async fn telemetry_negotiates_capability_and_rejects_changed_boot() {
    let path = std::env::temp_dir().join(format!("art-{}.sock", std::process::id()));
    let control = Control::bind(Some(path.clone())).unwrap();
    let task = tokio::spawn(async move { control.shutdown().await.unwrap() });
    assert!(runtime_telemetry(&path).await.unwrap().is_none());
    request(&path, Command::Stop).await.unwrap();
    task.await.unwrap();
    let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observed = called.clone();
    let control = Control::bind(Some(path.clone()))
        .unwrap()
        .with_runtime_telemetry(move || {
            observed.store(true, std::sync::atomic::Ordering::Relaxed);
            async { Err("sample failed".into()) }
        });
    let task = tokio::spawn(async move { control.shutdown().await.unwrap() });
    let reply = rpc(
        &path,
        Command::RuntimeTelemetry {
            expected_boot_id: "previous-boot".into(),
        },
        3,
    )
    .await
    .unwrap();
    assert!(
        matches!(reply, Response::RuntimeTelemetry(Err(error)) if error.contains("boot changed"))
    );
    assert!(!called.load(std::sync::atomic::Ordering::Relaxed));
    assert!(runtime_telemetry(&path)
        .await
        .unwrap_err()
        .to_string()
        .contains("sample failed"));
    assert!(called.load(std::sync::atomic::Ordering::Relaxed));
    request(&path, Command::Stop).await.unwrap();
    task.await.unwrap();
}
#[test]
fn publisher_panic_cannot_poison_applied_snapshots() {
    let state = std::sync::Arc::new(arc_swap::ArcSwapOption::empty());
    let publisher = state.clone();
    assert!(std::thread::spawn(move || {
        publish_applied(&publisher, std::path::Path::new("first.toml"), "first");
        panic!("injected publisher failure");
    })
    .join()
    .is_err());
    let retained = state.load_full().unwrap();
    assert_eq!(retained.path, PathBuf::from("first.toml"));
    assert_eq!(retained.digest, config_digest("first"));
    publish_applied(&state, std::path::Path::new("next.toml"), "next");
    let next = state.load_full().unwrap();
    assert_eq!(next.path, PathBuf::from("next.toml"));
    assert_eq!(next.digest, config_digest("next"));
    assert_eq!(retained.digest, config_digest("first"));
}
#[test]
fn poisoned_peer_cache_is_reset_before_reuse() {
    let pool = std::sync::Arc::new(std::sync::Mutex::new(ControlPeers::new()));
    let writer = pool.clone();
    assert!(std::thread::spawn(move || {
        let _guard = writer.lock().unwrap();
        panic!("injected cache writer failure");
    })
    .join()
    .is_err());
    assert!(peer_pool(&pool).is_empty());
    assert!(!pool.is_poisoned());
    assert!(peer_pool(&pool).is_empty());
}
#[tokio::test]
async fn monitor_reads_internal_counters_without_changing_status() {
    let path = std::env::temp_dir().join(format!("astra-monitor-test-{}.sock", std::process::id()));
    let control = Control::bind(Some(path.clone())).unwrap();
    let task = tokio::spawn(async move {
        control.shutdown().await.unwrap();
    });
    astra_observe::counter!("test.monitor_requests", "测试请求").add(1);
    let snapshot = monitor(&path).await.unwrap();
    assert_eq!(snapshot.epoch, astra_platform::lifecycle::boot_id());
    assert!(snapshot
        .metrics
        .iter()
        .any(|m| m.name == "test.monitor_requests" && m.value == "1"));
    assert!(snapshot
        .metrics
        .iter()
        .any(|m| m.name == "runtime.control_queue"));
    assert_eq!(
        request(&path, Command::Status).await.unwrap().pid,
        std::process::id()
    );
    request(&path, Command::Stop).await.unwrap();
    task.await.unwrap();
}
#[tokio::test]
async fn snapshot_uses_bounded_management_frames_without_changing_status_replies() {
    let path =
        std::env::temp_dir().join(format!("astra-snapshot-test-{}.sock", std::process::id()));
    let control = Control::bind(Some(path.clone())).unwrap();
    let task = tokio::spawn(async move {
        control
            .run_with_snapshot(
                |_| async { Ok(()) },
                || async {
                    Ok(ConfigSnapshot {
                        api_version: crate::API_VERSION,
                        pid: std::process::id(),
                        path: PathBuf::from("loaded.toml"),
                        text: "x".repeat(32 * 1024),
                    })
                },
            )
            .await
            .unwrap();
    });
    let snapshot = config_snapshot(&path).await.unwrap();
    assert_eq!(snapshot.text.len(), 32 * 1024);
    assert_eq!(snapshot.path, PathBuf::from("loaded.toml"));
    assert_eq!(snapshot.pid, std::process::id());
    assert!(request(&path, Command::GetConfig).await.is_err());
    assert!(request(&path, Command::Status).await.is_ok());
    request(&path, Command::Stop).await.unwrap();
    task.await.unwrap();
}
#[tokio::test]
async fn status_version_rejection_stop_and_cleanup() {
    let path = std::env::temp_dir().join(format!("astra-ctl-test-{}.sock", std::process::id()));
    let control = Control::bind(Some(path.clone())).unwrap();
    let task = tokio::spawn(async move {
        control.shutdown().await.unwrap();
    });
    assert_eq!(
        request(&path, Command::Status).await.unwrap().pid,
        std::process::id()
    );
    let mut stream = astra_platform::platform::LocalStream::connect(&path)
        .await
        .unwrap();
    crate::send(
        &mut stream,
        &Request {
            api_version: 999,
            command: Command::Stop,
        },
    )
    .await
    .unwrap();
    assert!(crate::receive::<Status>(&mut stream).await.is_err());
    assert!(request(&path, Command::Status).await.is_ok());
    request(&path, Command::Stop).await.unwrap();
    task.await.unwrap();
    assert!(!path.exists());
}
#[tokio::test]
async fn slow_reload_streams_progress_without_blocking_status_or_runtime_push() {
    let path = std::env::temp_dir().join(format!("astra-concurrent-{}.sock", std::process::id()));
    let control = Control::bind(Some(path.clone())).unwrap();
    let entered = std::sync::Arc::new(tokio::sync::Notify::new());
    let release = std::sync::Arc::new(tokio::sync::Notify::new());
    let (started, finish) = (entered.clone(), release.clone());
    let server = tokio::spawn(async move {
        control
            .run_with_runtime(
                move |_| {
                    let (started, finish) = (started.clone(), finish.clone());
                    async move {
                        crate::progress::report("Reload waiting", "test barrier");
                        started.notify_one();
                        finish.notified().await;
                        Ok(())
                    }
                },
                || async { Err("unused".into()) },
                |_| async { Err("unused".into()) },
                || async { Ok(()) },
                |_| async {
                    Ok(astra_model::runtime::Snapshot {
                        pid: std::process::id(),
                        ..Default::default()
                    })
                },
            )
            .await
            .unwrap();
    });
    let (progress, mut updates) = tokio::sync::watch::channel(None);
    let endpoint = path.clone();
    let reload = tokio::spawn(crate::progress::scope(progress, async move {
        request(&endpoint, Command::Reload { path: None }).await
    }));
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    assert!(!reload.is_finished());
    tokio::time::timeout(Duration::from_secs(1), async {
        assert_eq!(
            request(&path, Command::Status).await.unwrap().pid,
            std::process::id()
        );
        let mut push = watch_runtime(&path, 100).await.unwrap();
        for _ in 0..2 {
            let snapshot: Result<astra_model::runtime::Snapshot, String> =
                crate::receive_management(&mut push).await.unwrap();
            assert_eq!(snapshot.unwrap().pid, std::process::id());
        }
        loop {
            if updates
                .borrow_and_update()
                .as_ref()
                .is_some_and(|p| p.phase == "Reload waiting")
            {
                break;
            }
            updates.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    release.notify_one();
    reload.await.unwrap().unwrap();
    request(&path, Command::Stop).await.unwrap();
    server.await.unwrap();
}
#[test]
fn existing_socket_is_never_overwritten() {
    let path = std::env::temp_dir().join(format!("astra-ctl-existing-{}.sock", std::process::id()));
    std::fs::write(&path, "keep").unwrap();
    // Bind rejects existing regular files before creating a Tokio listener.
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        assert!(Control::bind(Some(path.clone())).is_err());
    });
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "keep");
    std::fs::remove_file(path).unwrap();
}
