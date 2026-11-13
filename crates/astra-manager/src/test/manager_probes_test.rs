use super::*;

/// A stand-in core: reads the configuration line, then answers every request
/// with its own PID as `connect_ms`, so reuse and respawns are visible.
fn fake_core(dir: &std::path::Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("fake-core");
    std::fs::write(
        &path,
        "#!/bin/sh\n[ \"$1\" = --probe-serve ] || exit 2\nread config\nwhile read request; do\n  printf '{\"outbound\":\"node\",\"direct\":false,\"connect_ms\":%s,\"elapsed_ms\":0,\"ip\":null,\"error\":null}\\n' $$\ndone\n",
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn plan(dir: &std::path::Path, text: &str) -> DiagnosticPlan {
    DiagnosticPlan {
        runtime_dir: dir.to_owned(),
        log_path: dir.join("core.log"),
        executable: fake_core(dir),
        config_path: dir.join("proxy.toml"),
        text: format!("# {text}\n"),
        running: true,
        control: None,
    }
}

fn request() -> ProbeRequest {
    ProbeRequest {
        outbound: "node".into(),
        ip: false,
    }
}

#[tokio::test]
async fn configured_probe_budget_survives_the_old_manager_watchdog() {
    let dir = tempfile::tempdir().unwrap();
    let mut plan = plan(dir.path(), "slow probe");
    plan.text.push_str("diagnostic_connect_timeout_secs = 20\n");
    std::fs::write(&plan.executable,
        "#!/bin/sh\nread config\nread request\nsleep 16\nprintf '{\"outbound\":\"node\",\"direct\":false,\"connect_ms\":16000,\"elapsed_ms\":16000,\"ip\":null,\"error\":null}\\n'\n"
    ).unwrap();
    let result = Probes::default()
        .probe("proxy", plan, request(), None)
        .await
        .unwrap();
    assert_eq!(result.connect_ms, Some(16000));
}

async fn pid(probes: &Probes, plan: DiagnosticPlan, keep: Option<Duration>) -> u64 {
    probes
        .probe("proxy", plan, request(), keep)
        .await
        .unwrap()
        .connect_ms
        .unwrap()
}

#[tokio::test]
async fn kept_workers_answer_later_rounds_and_one_shot_or_changed_config_respawns() {
    let dir = tempfile::tempdir().unwrap();
    let probes = Probes::default();
    let keep = Some(Duration::from_secs(60));
    let first = pid(&probes, plan(dir.path(), "a"), keep).await;
    assert_eq!(
        pid(&probes, plan(dir.path(), "a"), keep).await,
        first,
        "idle worker reused"
    );
    let changed = pid(&probes, plan(dir.path(), "b"), keep).await;
    assert_ne!(changed, first, "a new configuration loads a new worker");
    // Without keep-alive the round's worker exits and the next one starts over.
    assert_eq!(pid(&probes, plan(dir.path(), "b"), None).await, changed);
    assert_ne!(pid(&probes, plan(dir.path(), "b"), keep).await, changed);
}

#[tokio::test]
async fn an_idle_worker_exits_after_its_keep_alive() {
    let dir = tempfile::tempdir().unwrap();
    let probes = Probes::default();
    let short = Some(Duration::from_millis(100));
    let first = pid(&probes, plan(dir.path(), "a"), short).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(probes.slot("proxy").lock().await.worker.is_none());
    assert_ne!(pid(&probes, plan(dir.path(), "a"), short).await, first);
}

#[tokio::test]
async fn a_core_without_probe_support_asks_for_an_update() {
    let dir = tempfile::tempdir().unwrap();
    let mut old = plan(dir.path(), "a");
    old.executable = PathBuf::from("/usr/bin/true");
    let error = Probes::default()
        .probe("proxy", old, request(), Some(Duration::from_secs(60)))
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("Update proxy core"),
        "{error:#}"
    );
}
