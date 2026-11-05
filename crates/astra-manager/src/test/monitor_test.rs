use super::*;
#[tokio::test]
async fn monitor_combines_live_process_internal_and_network_observations() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("monitor.sock");
    let control = astra_ipc::control::Control::bind(Some(socket.clone())).unwrap();
    let task = tokio::spawn(async move {
        control
            .run_with_runtime(
                |_| async { Ok(()) },
                || async { Err("no config fixture".into()) },
                |_| async { Err("no route fixture".into()) },
                || async { Ok(()) },
                |_| async {
                    Ok(astra_model::runtime::Snapshot {
                        pid: std::process::id(),
                        upload: 9_007_199_254_740_993,
                        download: 8192,
                        active: 2,
                        total: 3,
                        ..Default::default()
                    })
                },
            )
            .await
            .unwrap();
    });
    let sample = core("live", CoreKind::Proxy, Some(std::process::id()), &socket)
        .await
        .unwrap();
    assert!(sample.error.is_none(), "{:?}", sample.error);
    assert_eq!(sample.pid, Some(std::process::id()));
    assert!(
        sample
            .memory_bytes
            .as_ref()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            > 0
    );
    assert_eq!(sample.upload_bytes.as_deref(), Some("9007199254740993"));
    assert_eq!(sample.active_connections, Some(2));
    assert!(sample
        .internal
        .unwrap()
        .metrics
        .iter()
        .any(|m| m.name == "runtime.control_jobs"));
    astra_ipc::control::request(&socket, astra_model::control::Command::Stop)
        .await
        .unwrap();
    task.await.unwrap();
}
#[test]
fn monitor_rejects_partial_worker_totals_instead_of_showing_false_rates() {
    let mut view = astra_model::runtime::RuntimeView {
        snapshot: astra_model::runtime::Snapshot {
            pid: 42,
            upload: 9_007_199_254_740_993,
            ..Default::default()
        },
        telemetry: Some(astra_model::runtime::RuntimeTelemetry {
            coverage: astra_model::runtime::Coverage::Partial,
            ..Default::default()
        }),
    };
    assert!(network_view(serde_json::to_value(&view).unwrap(), 42).is_err());
    view.telemetry.as_mut().unwrap().coverage = astra_model::runtime::Coverage::Complete;
    assert_eq!(
        network_view(serde_json::to_value(&view).unwrap(), 42)
            .unwrap()
            .upload,
        9_007_199_254_740_993
    );
    assert!(network_view(serde_json::to_value(&view).unwrap(), 43).is_err());
}
#[tokio::test]
async fn monitor_stopped_core_has_unknown_metrics_without_opening_endpoint() {
    let sample = core(
        "stopped",
        CoreKind::Rule,
        None,
        Path::new("nonexistent-monitor.sock"),
    )
    .await
    .unwrap();
    assert_eq!(sample.pid, None);
    assert_eq!(sample.cpu_percent, None);
    assert_eq!(sample.memory_bytes, None);
    assert_eq!(sample.upload_bytes, None);
    assert_eq!(sample.error, None);
    assert_eq!(sample.uptime_secs, "0");
}
#[tokio::test]
async fn monitor_stopped_server_has_unknown_metrics() {
    let root = tempfile::tempdir().unwrap();
    let sample = server("server", &root.path().join("missing.sock"))
        .await
        .unwrap();
    assert_eq!(sample.pid, None);
    assert_eq!(sample.upload_bytes, None);
    assert_eq!(sample.error, None);
}
