use super::*;
use std::sync::atomic::AtomicBool;
use tokio::{sync::oneshot, time::timeout};

struct Fixture {
    host: Arc<Host>,
    _current: Current,
    directory: PathBuf,
    servers: Vec<tokio::task::JoinHandle<()>>,
}
impl Fixture {
    async fn new(prepared: Option<u64>) -> Self {
        let directory = std::env::temp_dir().join(format!("at-{}", uuid::Uuid::new_v4().simple()));
        #[allow(unused_mut)]
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&directory).unwrap();
        let path = directory.join("config.toml");
        let app = App::from_text(&path, "standalone=true\nstandalone_outbound='direct'\nsocks5_listen='127.0.0.1:1080'\n[proxy_groups.direct]\nmembers=['DIRECT']\n".into()).unwrap();
        let current = Arc::new(arc_swap::ArcSwap::from_pointee(app));
        let mut generations = BTreeMap::new();
        for id in 1..=2 {
            // A harness child gives the production reap path a real handle.
            // It exits immediately; no privileged runtime is created.
            let child = tokio::process::Command::new(std::env::current_exe().unwrap())
                .arg("--list")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            generations.insert(
                id,
                Arc::new(Generation {
                    id,
                    directory: directory.clone(),
                    log: directory.join(format!("w{id}.log")),
                    endpoint: directory.join(format!("w{id}.sock")),
                    token: format!("worker-{id}"),
                    child: Mutex::new(WorkerChild::Normal(Box::new(child))),
                    flows: AtomicUsize::new(0),
                    rule_socket: String::new(),
                    snapshot: Default::default(),
                    failed: Default::default(),
                    route_leases: Default::default(),
                }),
            );
        }
        let host = Arc::new(Host {
            consistency: Default::default(),
            boot: "transition-test".into(),
            next_flow: AtomicU64::new(1),
            endpoint: directory.join("host.sock"),
            path,
            current: Arc::downgrade(&current),
            state: Arc::new(Mutex::new(State {
                active: 2,
                prepared,
                paused: true,
                generations,
            })),
            sequence: AtomicU64::new(3),
            transition: Default::default(),
            admissions: Default::default(),
            telemetry: Default::default(),
            traffic_history: Default::default(),
            connection_ids: Default::default(),
            retired: Default::default(),
            tokens: Default::default(),
            mappings: Arc::new(astra_dns::LocalMappingStore::default()),
            tasks: Default::default(),
        });
        Self {
            host,
            _current: current,
            directory,
            servers: vec![],
        }
    }
    async fn worker(
        &mut self,
        id: u64,
        gate: Option<bool>,
    ) -> (oneshot::Receiver<()>, oneshot::Sender<()>, Arc<AtomicBool>) {
        let generation = self.host.state.lock().await.generations[&id].clone();
        let listener = astra_platform::platform::bind_listener(&generation.endpoint).unwrap();
        astra_platform::platform::set_private(
            astra_platform::platform::endpoint_path(&generation.endpoint),
            0o600,
        )
        .unwrap();
        let (started_tx, started) = oneshot::channel();
        let (release, released) = oneshot::channel();
        let enabled = Arc::new(AtomicBool::new(id == 2));
        let worker_enabled = enabled.clone();
        self.servers.push(tokio::spawn(async move {
            let mut started_tx = Some(started_tx);
            let mut released = Some(released);
            while let Ok((mut stream, _)) = listener.accept().await {
                let auth: Auth<WorkerRequest> = receive(&mut stream).await.unwrap();
                assert_eq!(auth.token, generation.token);
                if let WorkerRequest::Active(value) = auth.command {
                    if gate == Some(value) && released.is_some() {
                        let _ = started_tx.take().unwrap().send(());
                        let _ = released.take().unwrap().await;
                    }
                    worker_enabled.store(value, Ordering::Release);
                    let _ = send(&mut stream, true).await;
                } else if matches!(auth.command, WorkerRequest::Snapshot) {
                    let _ = send(
                        &mut stream,
                        WorkerSample {
                            chains: None,
                            snapshot: astra_model::runtime::Snapshot::default(),
                            node_totals: Some([("node".into(), id * 100)].into()),
                            dns: None,
                            dns_resolver: None,
                        },
                    )
                    .await;
                } else if matches!(auth.command, WorkerRequest::Retire) {
                    let _ = send(&mut stream, true).await;
                } else {
                    panic!("unexpected worker request");
                }
            }
        }));
        (started, release, enabled)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for server in &self.servers {
            server.abort();
        }
        for id in 1..=2 {
            astra_platform::platform::remove_socket(&self.directory.join(format!("w{id}.sock")));
        }
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

#[tokio::test]
async fn traffic_history_collects_workers_without_a_ui_and_survives_retirement() {
    let mut fixture = Fixture::new(None).await;
    fixture.worker(1, None).await;
    fixture.worker(2, None).await;
    fixture.host.refresh_telemetry().await;
    fixture.host.refresh_telemetry().await;
    let totals = || {
        fixture
            .host
            .telemetry
            .lock()
            .unwrap()
            .node_traffic
            .clone()
            .unwrap()["node"]
    };
    assert_eq!(totals().session, 300);
    assert_eq!(totals().persist, 300);
    fixture.host.state.lock().await.paused = false;
    fixture.host.reap().await;
    fixture.host.refresh_telemetry().await;
    assert_eq!(totals().session, 300);
}

#[tokio::test]
async fn rollback_handshake_serializes_resume_and_reap() {
    let mut fixture = Fixture::new(None).await;
    let (started, release, _) = fixture.worker(1, Some(true)).await;
    fixture.worker(2, None).await;
    let host = fixture.host.clone();
    let selection = tokio::spawn(async move { host.select(1, false).await });
    timeout(Duration::from_secs(3), started)
        .await
        .unwrap()
        .unwrap();
    let host = fixture.host.clone();
    let mut resume = tokio::spawn(async move { host.command(Command::Resume).await });
    let host = fixture.host.clone();
    let mut reap = tokio::spawn(async move { host.reap().await });
    assert!(timeout(Duration::from_millis(50), &mut resume)
        .await
        .is_err());
    assert!(timeout(Duration::from_millis(50), &mut reap).await.is_err());
    release.send(()).unwrap();
    timeout(Duration::from_secs(3), selection)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    resume.await.unwrap().unwrap();
    reap.await.unwrap();
    let state = fixture.host.state.lock().await;
    assert_eq!(state.active, 1);
    assert!(state.generations.contains_key(&state.active));
    assert!(!state.paused);
}

#[tokio::test]
async fn committed_runtime_cannot_send_a_late_disable_after_rollback() {
    let mut fixture = Fixture::new(Some(1)).await;
    fixture.worker(1, None).await;
    let (started, release, enabled) = fixture.worker(2, Some(false)).await;
    let transition = fixture.host.transition.clone().lock_owned().await;
    let guard = CandidateGuard {
        host: fixture.host.clone(),
        id: 1,
        committed: false,
        transition: Some(transition),
    };
    let published = AtomicBool::new(false);
    guard
        .lock_commit(vec![])
        .await
        .unwrap()
        .commit(|| published.store(true, Ordering::Release));
    assert!(published.load(Ordering::Acquire));
    assert_eq!(fixture.host.state.lock().await.active, 1);
    timeout(Duration::from_secs(3), started)
        .await
        .unwrap()
        .unwrap();
    let host = fixture.host.clone();
    let mut rollback = tokio::spawn(async move { host.select(2, false).await });
    assert!(timeout(Duration::from_millis(50), &mut rollback)
        .await
        .is_err());
    release.send(()).unwrap();
    timeout(Duration::from_secs(3), rollback)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(enabled.load(Ordering::Acquire));
    assert_eq!(fixture.host.state.lock().await.active, 2);
}

#[tokio::test]
async fn failed_activation_and_cancelled_runtime_leave_previous_generation_selected() {
    let fixture = Fixture::new(Some(1)).await;
    // The candidate has no listening worker: readiness cannot be acknowledged.
    assert!(fixture.host.activate(1).await.is_err());
    assert_eq!(fixture.host.state.lock().await.active, 2);
    let transition = fixture.host.transition.clone().lock_owned().await;
    drop(CandidateGuard {
        host: fixture.host.clone(),
        id: 1,
        committed: false,
        transition: Some(transition),
    });
    timeout(Duration::from_secs(3), async {
        loop {
            if fixture.host.state.lock().await.prepared.is_none() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(fixture.host.state.lock().await.active, 2);
    assert!(fixture.host.transition.try_lock().is_ok());
}

#[tokio::test]
async fn cancellation_while_waiting_for_admission_aborts_only_the_candidate() {
    let fixture = Fixture::new(Some(1)).await;
    let transition = fixture.host.transition.clone().lock_owned().await;
    let candidate = CandidateGuard {
        host: fixture.host.clone(),
        id: 1,
        committed: false,
        transition: Some(transition),
    };
    let admission = fixture.host.state.lock().await;
    assert!(
        timeout(Duration::from_millis(25), candidate.lock_commit(vec![]))
            .await
            .is_err()
    );
    assert_eq!(admission.active, 2);
    assert!(fixture.host.transition.try_lock().is_ok());
    drop(admission);
    timeout(Duration::from_secs(3), async {
        loop {
            let state = fixture.host.state.lock().await;
            assert_eq!(state.active, 2);
            if state.prepared.is_none() {
                break;
            }
            drop(state);
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

async fn lost_override_acknowledgement(cancel: bool) {
    let mut fixture = Fixture::new(None).await;
    let generation = {
        let mut state = fixture.host.state.lock().await;
        state.paused = false;
        state.generations[&state.active].clone()
    };
    let listener = astra_platform::platform::bind_listener(&generation.endpoint).unwrap();
    let (started_tx, started) = oneshot::channel();
    fixture.servers.push(tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let auth: Auth<WorkerRequest> = receive(&mut stream).await.unwrap();
        assert!(matches!(auth.command, WorkerRequest::Overrides { .. }));
        let _ = started_tx.send(());
        if cancel {
            std::future::pending::<()>().await;
        }
        // The worker accepted the mutation, but its acknowledgement was lost.
    }));
    let published = Arc::new(AtomicBool::new(false));
    let visible = published.clone();
    let host = fixture.host.clone();
    let mut operation = tokio::spawn(async move {
        host.overrides(Default::default(), None, || {
            visible.store(true, Ordering::Release);
            Ok(())
        })
        .await
    });
    timeout(Duration::from_secs(3), started)
        .await
        .unwrap()
        .unwrap();
    if cancel {
        operation.abort();
        assert!(operation.await.unwrap_err().is_cancelled());
    } else {
        let error = timeout(Duration::from_secs(3), &mut operation)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("outcome unknown"));
    }
    assert!(!published.load(Ordering::Acquire));
    assert!(fixture
        .host
        .task_failure()
        .await
        .unwrap()
        .contains("outcome unknown"));
    assert!(fixture.host.ensure_consistent().is_err());
    assert!(fixture
        .host
        .overrides(Default::default(), None, || panic!(
            "uncertain state accepted another mutation"
        ))
        .await
        .is_err());
}

#[tokio::test]
async fn lost_worker_override_reply_requires_controlled_shutdown() {
    lost_override_acknowledgement(false).await;
}

#[tokio::test]
async fn cancelled_worker_override_requires_controlled_shutdown() {
    lost_override_acknowledgement(true).await;
}

#[tokio::test]
async fn worker_override_commit_with_failed_host_checkpoint_requires_shutdown() {
    let mut fixture = Fixture::new(None).await;
    let generation = {
        let mut state = fixture.host.state.lock().await;
        state.paused = false;
        state.generations[&state.active].clone()
    };
    let listener = astra_platform::platform::bind_listener(&generation.endpoint).unwrap();
    fixture.servers.push(tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let auth: Auth<WorkerRequest> = receive(&mut stream).await.unwrap();
        assert!(matches!(auth.command, WorkerRequest::Overrides { .. }));
        send(&mut stream, Ok::<(), String>(())).await.unwrap();
    }));
    let error = fixture
        .host
        .overrides(Default::default(), None, || {
            anyhow::bail!("injected checkpoint write failure")
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("outcome unknown"));
    assert!(fixture
        .host
        .task_failure()
        .await
        .unwrap()
        .contains("checkpoint"));
}

fn poison<T>(lock: &std::sync::Mutex<T>) {
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = lock.lock().unwrap();
        panic!("injected host lock poison");
    }))
    .is_err());
}

#[tokio::test]
async fn poisoned_route_ownership_rejects_commit_before_publishing() {
    let fixture = Fixture::new(Some(1)).await;
    let generation = fixture.host.state.lock().await.generations[&1].clone();
    poison(&generation.route_leases);
    let transition = fixture.host.transition.clone().lock_owned().await;
    let candidate = CandidateGuard {
        host: fixture.host.clone(),
        id: 1,
        committed: false,
        transition: Some(transition),
    };
    let error = candidate
        .lock_commit(vec![])
        .await
        .err()
        .expect("poisoned ownership accepted");
    assert!(error.to_string().contains("route ownership"));
    assert_eq!(fixture.host.state.lock().await.active, 2);
    assert!(fixture
        .host
        .task_failure()
        .await
        .unwrap()
        .contains("route ownership"));
}

#[tokio::test]
async fn poisoned_host_cleanup_does_not_panic_again_or_trust_partial_telemetry() {
    let fixture = Fixture::new(Some(1)).await;
    poison(&fixture.host.telemetry);
    let mut snapshot = astra_model::runtime::Snapshot::default();
    fixture.host.merge_snapshot(&mut snapshot);
    assert!(snapshot.truncated);
    assert!(fixture
        .host
        .task_failure()
        .await
        .unwrap()
        .contains("poisoned"));
    poison(&fixture.host.tokens);
    let preparation = Preparation {
        state: fixture.host.state.clone(),
        id: 1,
        committed: false,
        tokens: fixture.host.tokens.clone(),
        token: Some("candidate".into()),
        pid: None,
    };
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(preparation))).is_ok());
    poison(&fixture.host.tasks);
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(fixture))).is_ok());
}
