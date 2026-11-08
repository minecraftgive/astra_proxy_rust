use super::*;
fn record(home: &Path) -> Record {
    Record {
        version: 1,
        operation_id: "operation-1".into(),
        checkpoint_id: "checkpoint-1".into(),
        phase: Phase::Prepared,
        release_id: "new".into(),
        target: home.join("new"),
        manifest_sha256: "hash".into(),
        coordinator: home.join("new"),
        coordinator_sha256: "hash".into(),
        helper_sha256: "hash".into(),
        previous: home.join("old"),
        previous_manifest_sha256: Some("hash".into()),
        old_epoch: "old".into(),
        old_process: astra_platform::lifecycle::capture(std::process::id()).unwrap(),
        config: ManagerConfig {
            state_dir: home.into(),
            ..Default::default()
        },
        instances: Vec::new(),
        frontends: Vec::new(),
        managed_processes: Vec::new(),
        helper: None,
        new_process: None,
        new_epoch: None,
        restored: Vec::new(),
        error: None,
        failed_phase: None,
        service_update: false,
        candidates: Default::default(),
    }
}
#[test]
fn journal_failure_remains_pending_until_explicit_recovery_and_lock_is_exclusive() {
    let temp = tempfile::tempdir().unwrap();
    let mut record = record(temp.path());
    let lease = lock(temp.path()).unwrap();
    assert!(lock(temp.path()).is_err());
    write(&record).unwrap();
    assert!(pending(temp.path()).unwrap());
    record.phase = Phase::Failed;
    record.error = Some("interrupted".into());
    write(&record).unwrap();
    assert!(pending(temp.path()).unwrap());
    assert_eq!(
        read(temp.path()).unwrap().unwrap().error.as_deref(),
        Some("interrupted")
    );
    record.phase = Phase::Cancelled;
    write(&record).unwrap();
    assert!(!pending(temp.path()).unwrap());
    drop(lease);
    assert!(lock(temp.path()).is_ok());
}
#[test]
fn cancellation_and_frontend_checkpoints_are_bound_to_the_transaction() {
    let temp = tempfile::tempdir().unwrap();
    let mut record = record(temp.path());
    write(&record).unwrap();
    let lease = register_frontend(temp.path(), Path::new("astra-tui")).unwrap();
    let windows = frontends(temp.path()).unwrap();
    assert_eq!(windows.len(), 1);
    assert!(!checkpointed(&record, &windows[0].process).unwrap());
    acknowledge(&record).unwrap();
    assert!(checkpointed(&record, &windows[0].process).unwrap());
    record.checkpoint_id = "retry-checkpoint".into();
    assert!(!checkpointed(&record, &windows[0].process).unwrap());
    request_cancel(temp.path(), "another-operation").unwrap();
    assert!(!cancelled(&record).unwrap());
    request_cancel(temp.path(), &record.operation_id).unwrap();
    assert!(cancelled(&record).unwrap());
    drop(lease);
    assert!(frontends(temp.path()).unwrap().is_empty());
}
#[cfg(unix)]
#[test]
fn recovery_record_rejects_symlink_and_wrong_state_directory() {
    let temp = tempfile::tempdir().unwrap();
    let record = record(temp.path());
    write(&record).unwrap();
    let current = directory(temp.path()).join("current.json");
    std::fs::rename(&current, temp.path().join("record.json")).unwrap();
    std::os::unix::fs::symlink(temp.path().join("record.json"), &current).unwrap();
    assert!(read(temp.path()).is_err());
    std::fs::remove_file(&current).unwrap();
    let mut wrong = record;
    wrong.config.state_dir = temp.path().join("another-manager");
    std::fs::write(&current, serde_json::to_vec(&wrong).unwrap()).unwrap();
    assert!(read(temp.path()).is_err());
}

#[test]
fn independent_coordinator_hash_survives_payload_damage_but_rejects_helper_damage() {
    use sha2::{Digest, Sha256};
    let temp = tempfile::tempdir().unwrap();
    let mut record = record(temp.path());
    std::fs::create_dir(&record.coordinator).unwrap();
    let path = record
        .coordinator
        .join(astra_platform::platform::executable("astra-host"));
    std::fs::write(&path, b"fixed-helper").unwrap();
    record.helper_sha256 = format!("{:x}", Sha256::digest(b"fixed-helper"));
    std::fs::write(record.coordinator.join("astra-rule"), b"damaged").unwrap();
    assert_eq!(coordinator(&record).unwrap(), path);
    std::fs::write(&path, b"damaged-helper").unwrap();
    assert!(coordinator(&record).is_err());
}

#[test]
fn interface_readiness_and_failures_remain_bound_to_this_attempt() {
    let temp = tempfile::tempdir().unwrap();
    let mut record = record(temp.path());
    record.phase = Phase::Ready;
    let own = astra_platform::lifecycle::capture(std::process::id()).unwrap();
    let frontend = Frontend {
        process: own,
        executable: record.previous.join("astra-tui"),
    };
    record.frontends.push(frontend.clone());
    write(&record).unwrap();
    assert!(!switched(&record, &frontend).unwrap());
    let executable = record.target.join("astra-tui");
    let token = begin_switch(temp.path(), &executable).unwrap().unwrap();
    assert!(!switched(&record, &frontend).unwrap());
    fail_switch(temp.path(), &token, "candidate failed to paint").unwrap();
    assert!(switched(&record, &frontend).is_err());
    begin_switch(temp.path(), &executable).unwrap();
    finish_switch(temp.path(), &executable).unwrap();
    assert!(switched(&record, &frontend).unwrap());
    record.checkpoint_id = "another-attempt".into();
    assert!(!switched(&record, &frontend).unwrap());
}
