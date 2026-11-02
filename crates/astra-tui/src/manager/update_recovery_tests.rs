//! Host-semantic fault injection: SelectExisting requires Pause, Abort requires Prepared.
use super::*;
use std::sync::{Arc, Mutex};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let id = UpdateRequest::new(UpdateIntent::Stage {
            package: PathBuf::new(),
        })
        .operation_id;
        let root = std::env::temp_dir().join(format!("astra-rollback-recovery-{id}"));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }
    fn manager(&self) -> Manager {
        Manager::load(self.0.join("state"), self.0.join("bundle")).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct HostState {
    previous: u64,
    candidate: u64,
    active: u64,
    paused: bool,
    prepared: bool,
    fail_select_once: bool,
    candidate_flows: u64,
    commands: Vec<&'static str>,
}
struct FakeHost {
    endpoint: PathBuf,
    state: Arc<Mutex<HostState>>,
    task: tokio::task::JoinHandle<()>,
}
impl FakeHost {
    async fn start(
        root: &Path,
        name: &str,
        previous: u64,
        active_candidate: bool,
        fail_select_once: bool,
    ) -> Self {
        let endpoint = root.join(format!("{name}.sock"));
        let listener = UnixListener::bind(&endpoint).unwrap();
        let state = Arc::new(Mutex::new(HostState {
            previous,
            candidate: previous + 1,
            active: previous + u64::from(active_candidate),
            paused: false,
            prepared: !active_candidate,
            fail_select_once,
            candidate_flows: if active_candidate { 7 } else { 0 },
            commands: vec![],
        }));
        let observed = state.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let command: HostCommand = hosted::receive(&mut stream).await.unwrap();
                let response: std::result::Result<serde_json::Value, String> = {
                    let mut state = observed.lock().unwrap();
                    match command {
                        HostCommand::Status => Ok(
                            json!({"active":state.active,"paused":state.paused,"busy":state.prepared||state.paused,"boot_id":"same-boot",
                            "generations":[
                                {"generation":state.previous,"directory":"before","pid":1,"active":state.active==state.previous,"prepared":false,"flows":3,"rule_socket":"","failed":false,"log":""},
                                {"generation":state.candidate,"directory":"target","pid":2,"active":state.active==state.candidate,"prepared":state.prepared,"flows":state.candidate_flows,"rule_socket":"","failed":false,"log":""}]}),
                        ),
                        HostCommand::Pause => {
                            state.commands.push("pause");
                            state.paused = true;
                            Ok(json!(true))
                        }
                        HostCommand::SelectExisting { generation } => {
                            state.commands.push("select");
                            if !state.paused {
                                Err("admission must be paused".into())
                            } else if state.fail_select_once {
                                state.fail_select_once = false;
                                Err("injected rollback selection failure".into())
                            } else if generation != state.previous {
                                Err("recovery changed its fixed target".into())
                            } else {
                                state.active = generation;
                                Ok(json!(true))
                            }
                        }
                        HostCommand::Abort { generation } => {
                            state.commands.push("abort");
                            if generation != state.candidate || !state.prepared {
                                Err("can only abort a prepared generation".into())
                            } else {
                                state.prepared = false;
                                Ok(json!(true))
                            }
                        }
                        HostCommand::Resume => {
                            state.commands.push("resume");
                            state.paused = false;
                            Ok(json!(true))
                        }
                        _ => {
                            state.commands.push("unexpected");
                            Err("recovery must not prepare or activate a generation".into())
                        }
                    }
                };
                hosted::send(&mut stream, response).await.unwrap();
            }
        });
        Self {
            endpoint,
            state,
            task,
        }
    }
    fn participant(&self) -> crate::protocol::updates::UpdateParticipant {
        let state = self.state.lock().unwrap();
        crate::protocol::updates::UpdateParticipant {
            endpoint: self.endpoint.clone(),
            host_boot_id: "same-boot".into(),
            previous_generation: state.previous,
            candidate_generation: state.candidate,
            resume_needed: false,
        }
    }
    fn candidate(&self) -> Candidate {
        let state = self.state.lock().unwrap();
        Candidate {
            endpoint: self.endpoint.clone(),
            generation: state.candidate,
            previous: state.previous,
            host_boot_id: "same-boot".into(),
        }
    }
    async fn close(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}
fn operation(manager: &Manager, hosts: &[&FakeHost]) -> UpdateOperation {
    UpdateOperation {
        request: UpdateRequest::new(UpdateIntent::Activate {
            release_id: "target".into(),
        }),
        phase: UpdatePhase::Uncertain,
        before_directory: manager.bin.clone(),
        target_directory: Some(PathBuf::from("target")),
        release_id: Some("target".into()),
        runtime_committed: false,
        next_launch_saved: false,
        pending_record_saved: false,
        participants: hosts.iter().map(|host| host.participant()).collect(),
        uncertain_preparations: vec![],
        error: None,
        updated_at_ms: 1,
    }
}

#[tokio::test]
async fn rollback_only_aborts_prepared_candidates_and_preserves_active_sessions() {
    let fixture = Fixture::new();
    let active = FakeHost::start(&fixture.0, "active", 1, true, false).await;
    let prepared = FakeHost::start(&fixture.0, "prepared", 10, false, false).await;
    restore_previous(&active.candidate()).await.unwrap();
    restore_previous(&prepared.candidate()).await.unwrap();
    {
        let state = active.state.lock().unwrap();
        assert_eq!(state.commands, vec!["pause", "select"]);
        assert_eq!(state.active, state.previous);
        assert_eq!(
            state.candidate_flows, 7,
            "retired candidate sessions must drain naturally"
        );
    }
    assert_eq!(prepared.state.lock().unwrap().commands, vec!["abort"]);
    active.close().await;
    prepared.close().await;
}

#[tokio::test]
async fn mixed_partial_rollback_recovers_with_pause_fixed_selection_and_resume() {
    let fixture = Fixture::new();
    let mut manager = fixture.manager();
    let active = FakeHost::start(&fixture.0, "active", 1, true, true).await;
    let prepared = FakeHost::start(&fixture.0, "prepared", 10, false, false).await;
    let operation = operation(&manager, &[&active, &prepared]);
    let id = operation.request.operation_id.clone();
    manager.updates.status.operation = Some(operation);
    manager.persist_update_operation().unwrap();
    let first: UpdateStatus =
        serde_json::from_str(&manager.recover_update(&id).await.unwrap().text).unwrap();
    assert_eq!(first.operation.unwrap().phase, UpdatePhase::NeedsRecovery);
    assert!(
        active.state.lock().unwrap().paused,
        "keep old generation pinned until rollback can finish"
    );
    let recorded: UpdateOperation = read_optional(&operation_path(&manager.home, &id).unwrap())
        .unwrap()
        .unwrap();
    assert!(recorded.participants[0].resume_needed);
    assert_eq!(
        active.state.lock().unwrap().commands,
        vec!["pause", "select"]
    );
    let second: UpdateStatus =
        serde_json::from_str(&manager.recover_update(&id).await.unwrap().text).unwrap();
    assert_eq!(second.operation.unwrap().phase, UpdatePhase::Failed); // Restored, original operation not applied.
    {
        let state = active.state.lock().unwrap();
        assert_eq!(
            state.commands,
            vec!["pause", "select", "pause", "select", "resume"]
        );
        assert_eq!(state.active, state.previous);
        assert!(!state.paused);
        assert_eq!(state.candidate_flows, 7);
    }
    assert_eq!(prepared.state.lock().unwrap().commands, vec!["abort"]);
    manager.recover_update(&id).await.unwrap();
    assert_eq!(
        active.state.lock().unwrap().commands.len(),
        5,
        "terminal recovery must be idempotent"
    );
    active.close().await;
    prepared.close().await;
}

#[tokio::test]
async fn failed_recovery_checkpoint_does_not_pause_or_switch_hosts() {
    let fixture = Fixture::new();
    let mut manager = fixture.manager();
    let active = FakeHost::start(&fixture.0, "active", 1, true, false).await;
    let prepared = FakeHost::start(&fixture.0, "prepared", 10, false, false).await;
    let operation = operation(&manager, &[&active, &prepared]);
    let id = operation.request.operation_id.clone();
    manager.updates.status.operation = Some(operation);
    manager.persist_update_operation().unwrap();
    let path = operation_path(&manager.home, &id).unwrap();
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    let first: UpdateStatus =
        serde_json::from_str(&manager.recover_update(&id).await.unwrap().text).unwrap();
    assert_eq!(first.operation.unwrap().phase, UpdatePhase::NeedsRecovery);
    assert!(active.state.lock().unwrap().commands.is_empty());
    assert!(prepared.state.lock().unwrap().commands.is_empty());
    fs::remove_dir(path).unwrap();
    manager.recover_update(&id).await.unwrap();
    assert_eq!(
        active.state.lock().unwrap().commands,
        vec!["pause", "select", "resume"]
    );
    active.close().await;
    prepared.close().await;
}
