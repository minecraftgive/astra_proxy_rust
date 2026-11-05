use super::*;
use sha2::{Digest, Sha256};
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let id = UpdateRequest::new(UpdateIntent::Stage {
            package: PathBuf::new(),
        })
        .operation_id;
        let path = std::env::temp_dir().join(format!("astra-update-operation-{id}"));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn manager(&self) -> Manager {
        Manager::load(self.0.join("state"), self.0.join("bundle")).unwrap()
    }
    fn package(&self) -> PathBuf {
        let directory = self.0.join("package");
        fs::create_dir_all(&directory).unwrap();
        let mut files = Vec::new();
        let mut components = std::collections::BTreeMap::new();
        for name in ["astra-host", "astra-tui", "astra-proxy", "astra-rule"] {
            let file = astra_platform::platform::executable(name);
            fs::write(directory.join(&file), name.as_bytes()).unwrap();
            files.push(astra_update::ReleaseFile {
                path: file,
                size: name.len() as u64,
                sha256: format!("{:x}", Sha256::digest(name.as_bytes())),
            });
            components.insert(name.into(), "test".into());
        }
        let manifest = astra_update::ReleaseManifest {
            schema_version: 1,
            role: Default::default(),
            release_id: "test-release".into(),
            target: astra_update::TARGET.into(),
            host_protocol: astra_update::ProtocolRange { min: 1, max: 1 },
            components,
            files,
            profile: None,
            mode: None,
            cargo_profile: None,
        };
        fs::write(
            directory.join(astra_update::MANIFEST_FILE),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        directory
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn decoded(reply: Reply) -> UpdateStatus {
    serde_json::from_str(&reply.text).unwrap()
}
fn record(request: UpdateRequest, before: PathBuf, target: PathBuf) -> UpdateOperation {
    UpdateOperation {
        request,
        phase: UpdatePhase::Resuming,
        before_directory: before,
        target_directory: Some(target),
        release_id: None,
        runtime_committed: true,
        next_launch_saved: false,
        pending_record_saved: false,
        participants: vec![],
        uncertain_preparations: vec![],
        error: None,
        updated_at_ms: 1,
    }
}
#[tokio::test]
async fn stage_is_idempotent_even_after_source_disappears_and_does_not_select_it() {
    let fixture = Fixture::new();
    let mut manager = fixture.manager();
    let package = fixture.package();
    let request = UpdateRequest::new(UpdateIntent::Stage {
        package: package.clone(),
    });
    let first = decoded(manager.update_transaction(request.clone()).await.unwrap());
    assert_eq!(first.operation.unwrap().phase, UpdatePhase::Staged);
    assert!(first.next_launch.is_none());
    fs::remove_dir_all(&package).unwrap();
    let repeated = decoded(manager.update_transaction(request.clone()).await.unwrap());
    assert_eq!(repeated.operation.unwrap().phase, UpdatePhase::Staged);
    assert!(!manager.home.join("updates/current.json").exists());
    let conflict = UpdateRequest {
        operation_id: request.operation_id,
        intent: UpdateIntent::Activate {
            release_id: "another".into(),
        },
    };
    assert!(manager
        .update_transaction(conflict)
        .await
        .unwrap_err()
        .to_string()
        .contains("bound to another"));
}
#[tokio::test]
async fn persistence_failure_recovers_only_remaining_steps_with_fixed_rollback_target() {
    let fixture = Fixture::new();
    let mut manager = fixture.manager();
    let original = manager.bin.clone();
    let target = fixture.0.join("previous-bundle");
    let request = UpdateRequest::new(UpdateIntent::Rollback {
        target_directory: target.clone(),
    });
    manager.bin = target.clone(); // The runtime commit already happened before the injected disk failure.
    manager.updates.previous_bin = Some(original.clone());
    manager.updates.status.operation =
        Some(record(request.clone(), original.clone(), target.clone()));
    manager.updates.tracking = true;
    manager.persist_update_operation().unwrap();
    let obstruction = manager.home.join("updates/current.json");
    fs::create_dir(&obstruction).unwrap();
    let result = manager.complete_committed_update().await;
    assert!(result.is_err());
    let partial = decoded(manager.finish_update_attempt(result).unwrap());
    assert!(partial.operation.as_ref().unwrap().runtime_committed);
    assert_eq!(partial.operation.unwrap().phase, UpdatePhase::NeedsRecovery);
    fs::remove_dir(&obstruction).unwrap();
    let restored = decoded(manager.recover_update(&request.operation_id).await.unwrap());
    let operation = restored.operation.unwrap();
    assert_eq!(operation.phase, UpdatePhase::Complete);
    assert!(operation.next_launch_saved && operation.pending_record_saved);
    assert_eq!(manager.bin, target);
    assert_eq!(manager.updates.previous_bin, Some(original)); // No second rollback/activation occurred.
    let repeated = decoded(manager.update_transaction(request).await.unwrap());
    assert_eq!(repeated.operation.unwrap().target_directory, Some(target));
}
#[tokio::test]
async fn status_is_read_only_and_restart_exposes_unfinished_operation() {
    let fixture = Fixture::new();
    let mut manager = fixture.manager();
    assert!(!manager.home.join("updates").exists());
    decoded(manager.update_status().await.unwrap());
    assert!(
        !manager.home.join("updates").exists(),
        "status must not create release-store locks or recover journals"
    );
    let request = UpdateRequest::new(UpdateIntent::Rollback {
        target_directory: fixture.0.join("old"),
    });
    manager.updates.status.operation = Some(record(
        request.clone(),
        manager.bin.clone(),
        fixture.0.join("old"),
    ));
    manager.persist_update_operation().unwrap();
    let loaded = Updates::load(&manager.home, &manager.bin).unwrap();
    assert_eq!(
        loaded.status.operation.unwrap().phase,
        UpdatePhase::Uncertain
    );
    assert!(operation_path(&manager.home, "../wrong").is_err());
}

#[tokio::test]
async fn failed_resume_recovery_does_not_reactivate_or_reverse_a_committed_rollback() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    let fixture = Fixture::new();
    let mut manager = fixture.manager();
    let endpoint = fixture.0.join("fake-host.sock");
    let listener = UnixListener::bind(&endpoint).unwrap();
    let resume_count = Arc::new(AtomicUsize::new(0));
    let mutation_count = Arc::new(AtomicUsize::new(0));
    let (resumes, mutations) = (resume_count.clone(), mutation_count.clone());
    let host = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let command: HostCommand = astra_ipc::host::receive_message(&mut stream).await.unwrap();
            let reply: std::result::Result<serde_json::Value, String> = match command {
                HostCommand::Status => {
                    Ok(json!({"active":2,"generations":[],"boot_id":"stable-host",
                    "paused":resumes.load(Ordering::SeqCst)<2,"busy":false}))
                }
                HostCommand::Resume => {
                    if resumes.fetch_add(1, Ordering::SeqCst) == 0 {
                        Err("injected ingress resume failure".into())
                    } else {
                        Ok(json!(null))
                    }
                }
                _ => {
                    mutations.fetch_add(1, Ordering::SeqCst);
                    Err("activation must never replay during recovery".into())
                }
            };
            astra_ipc::host::send_message(&mut stream, reply)
                .await
                .unwrap();
        }
    });
    let target = fixture.0.join("already-active-previous-release");
    let request = UpdateRequest::new(UpdateIntent::Rollback {
        target_directory: target.clone(),
    });
    let mut operation = record(request.clone(), manager.bin.clone(), target.clone());
    // All hosts committed, but the durable commit acknowledgement was lost.
    operation.runtime_committed = false;
    operation.phase = UpdatePhase::Uncertain;
    operation
        .participants
        .push(crate::protocol::updates::UpdateParticipant {
            endpoint,
            host_boot_id: "stable-host".into(),
            previous_generation: 1,
            candidate_generation: 2,
            resume_needed: true,
        });
    manager.updates.status.operation = Some(operation);
    manager.persist_update_operation().unwrap();
    let first = decoded(manager.recover_update(&request.operation_id).await.unwrap());
    assert!(first.operation.as_ref().unwrap().runtime_committed);
    assert_eq!(first.operation.unwrap().phase, UpdatePhase::NeedsRecovery);
    assert_eq!(mutation_count.load(Ordering::SeqCst), 0);
    let second = decoded(manager.recover_update(&request.operation_id).await.unwrap());
    assert_eq!(second.operation.unwrap().phase, UpdatePhase::Complete);
    assert_eq!(manager.bin, target);
    manager.recover_update(&request.operation_id).await.unwrap();
    assert_eq!(resume_count.load(Ordering::SeqCst), 2);
    assert_eq!(mutation_count.load(Ordering::SeqCst), 0);
    host.abort();
    let _ = host.await;
}

#[tokio::test]
async fn operation_can_be_queried_by_identity_after_another_window_completes_an_update() {
    let fixture = Fixture::new();
    let mut manager = fixture.manager();
    let package = fixture.package();
    let first = UpdateRequest::new(UpdateIntent::Stage {
        package: package.clone(),
    });
    manager.update_transaction(first.clone()).await.unwrap();
    let next = UpdateRequest::new(UpdateIntent::Stage { package });
    manager.update_transaction(next.clone()).await.unwrap();
    let queried = decoded(
        Manager::lookup_update_operation(
            &manager.home,
            manager.cached_update_status(),
            &first.operation_id,
        )
        .unwrap(),
    );
    assert_eq!(queried.operation.unwrap().request, first);
    assert_eq!(manager.updates.status.operation.unwrap().request, next);
}
