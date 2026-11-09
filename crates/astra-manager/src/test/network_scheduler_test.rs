use super::*;
use std::sync::atomic::AtomicUsize;

fn scheduler() -> (tempfile::TempDir, Arc<Scheduler>) {
    let root = tempfile::tempdir().unwrap();
    let db = crate::database::Database::open(root.path().join("jobs.sqlite"), 10000).unwrap();
    let scheduler = Scheduler::new(db, root.path().to_path_buf());
    (root, scheduler)
}

#[tokio::test]
async fn nat_owns_both_slots_and_keeps_only_four_waiting_jobs() {
    let (_root, scheduler) = scheduler();
    let (started, mut ready) = watch::channel(false);
    scheduler
        .submit_mode(
            "nat".into(),
            "nat-input".into(),
            true,
            move |_, mut stop| async move {
                started.send_replace(true);
                cancelled(&mut stop).await;
                tokio::time::sleep(Duration::from_millis(10)).await; // owned socket cleanup
                Ok(())
            },
        )
        .await
        .unwrap();
    ready.wait_for(|ready| *ready).await.unwrap();
    let ordinary = Arc::new(AtomicUsize::new(0));
    for n in 0..4 {
        let starts = ordinary.clone();
        scheduler
            .submit(
                format!("waiting-{n}"),
                format!("request-{n}"),
                move |_, mut stop| async move {
                    starts.fetch_add(1, Ordering::SeqCst);
                    cancelled(&mut stop).await;
                    Ok(())
                },
            )
            .await
            .unwrap();
    }
    let rejected = scheduler
        .submit("overflow".into(), "extra".into(), |_, _| async {
            panic!("queue overflow ran")
        })
        .await
        .unwrap_err();
    assert!(rejected.to_string().contains("queue full"));
    assert!(matches!(crate::error::code(&rejected), ErrorCode::Conflict));
    tokio::task::yield_now().await;
    assert_eq!(ordinary.load(Ordering::SeqCst), 0);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), scheduler.legacy_permit())
            .await
            .is_err()
    );
    let report = scheduler.cancel("nat").await.unwrap().unwrap();
    assert_eq!(report.state, NetworkState::Cancelled);
    until(|| ordinary.load(Ordering::SeqCst) == 2).await;
    scheduler.pause().await.unwrap();
}

#[tokio::test]
async fn queued_nat_waits_for_existing_probes_and_does_not_get_bypassed() {
    let (_root, scheduler) = scheduler();
    let old = scheduler.legacy_permit().await.unwrap();
    let (started, mut ready) = watch::channel(false);
    scheduler
        .submit_mode(
            "nat".into(),
            "input".into(),
            true,
            move |_, mut stop| async move {
                started.send_replace(true);
                cancelled(&mut stop).await;
                Ok(())
            },
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!*ready.borrow());
    assert!(
        tokio::time::timeout(Duration::from_millis(20), scheduler.legacy_permit())
            .await
            .is_err()
    );
    drop(old);
    ready.wait_for(|ready| *ready).await.unwrap();
    scheduler.cancel("nat").await.unwrap();
}

async fn until(mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !predicate() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn two_running_four_queued_busy_dedup_and_cancel_without_starting() {
    let (_root, scheduler) = scheduler();
    let starts = Arc::new(AtomicUsize::new(0));
    let exits = Arc::new(AtomicUsize::new(0));
    for n in 0..6 {
        let (starts, exits) = (starts.clone(), exits.clone());
        let task = scheduler
            .submit(
                n.to_string(),
                format!("input-{n}"),
                move |_, mut stop| async move {
                    starts.fetch_add(1, Ordering::SeqCst);
                    cancelled(&mut stop).await;
                    // Model the child wait: cancellation must not complete at signal delivery.
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    exits.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            )
            .await
            .unwrap();
        assert!(matches!(task.state, TaskState::Running));
    }
    until(|| starts.load(Ordering::SeqCst) == 2).await;
    let full = scheduler
        .submit("overflow".into(), "new".into(), |_, _| async {
            panic!("must not execute")
        })
        .await;
    let full = full.unwrap_err();
    assert!(full.to_string().contains("busy"));
    assert!(matches!(crate::error::code(&full), ErrorCode::Conflict));
    assert!(scheduler.store.task("overflow").await.is_err());
    let repeated = scheduler
        .submit("0".into(), "input-0".into(), |_, _| async {
            panic!("must not replay")
        })
        .await
        .unwrap();
    assert!(matches!(repeated.state, TaskState::Running));
    assert!(scheduler
        .submit("0".into(), "different".into(), |_, _| async { Ok(()) })
        .await
        .is_err());
    let cancelled = scheduler.cancel("5").await.unwrap().unwrap();
    assert_eq!(cancelled.state, NetworkState::Cancelled);
    assert_eq!(starts.load(Ordering::SeqCst), 2);
    let guard = scheduler.pause().await.unwrap();
    assert_eq!(starts.load(Ordering::SeqCst), 2);
    assert_eq!(exits.load(Ordering::SeqCst), 2);
    assert!(scheduler.jobs.lock().await.is_empty());
    assert!(scheduler
        .submit("paused".into(), "new".into(), |_, _| async { Ok(()) })
        .await
        .is_err());
    // A completed identity is returned even while maintenance blocks new work.
    let terminal = scheduler
        .submit("0".into(), "input-0".into(), |_, _| async {
            panic!("must not replay")
        })
        .await
        .unwrap();
    assert!(terminal.finished_ms.is_some());
    drop(guard);
}

#[tokio::test]
async fn cancelling_waits_for_worker_cleanup_and_retains_evidence() {
    let (_root, scheduler) = scheduler();
    let (started, mut ready) = watch::channel(false);
    let (cleanup, mut finish) = watch::channel(false);
    scheduler
        .submit(
            "cancel".into(),
            "same".into(),
            move |output, mut stop| async move {
                output
                    .send(CoreEvent::Attempt(astra_model::nettool::Attempt {
                        seq: 1,
                        rtt_ms: Some(1.0),
                        detail: "local fixture".into(),
                        error: None,
                    }))
                    .await?;
                started.send_replace(true);
                cancelled(&mut stop).await;
                let _ = finish.wait_for(|done| *done).await;
                Ok(())
            },
        )
        .await
        .unwrap();
    ready.wait_for(|ready| *ready).await.unwrap();
    let owner = scheduler.clone();
    let mut cancel = tokio::spawn(async move { owner.cancel("cancel").await });
    assert!(tokio::time::timeout(Duration::from_millis(20), &mut cancel)
        .await
        .is_err());
    assert!(!scheduler
        .store
        .network_report("cancel", None)
        .await
        .unwrap()
        .state
        .terminal());
    cleanup.send_replace(true);
    let report = cancel.await.unwrap().unwrap().unwrap();
    assert_eq!(report.state, NetworkState::Cancelled);
    assert_eq!(report.items.len(), 1);
}

#[tokio::test]
async fn deadline_includes_queue_time_and_never_starts_expired_work() {
    let (_root, scheduler) = scheduler();
    scheduler
        .store
        .begin_network("deadline", "same")
        .await
        .unwrap();
    let (stop, cancel) = watch::channel(false);
    scheduler
        .work(
            "deadline",
            stop,
            cancel,
            tokio::time::Instant::now(),
            WorkAdmission {
                units: 1,
                admission: Admission::reserve(scheduler.running.clone(), 1)
                    .await
                    .unwrap(),
                queued: None,
            },
            |_, _| async { panic!("expired work started") },
        )
        .await
        .unwrap();
    let report = scheduler
        .store
        .network_report("deadline", None)
        .await
        .unwrap();
    assert_eq!(report.state, NetworkState::Partial);
    assert!(report.items.is_empty());
}

#[tokio::test]
async fn failed_cleanup_is_not_reported_as_cancelled() {
    let (_root, scheduler) = scheduler();
    let (started, mut ready) = watch::channel(false);
    scheduler
        .submit(
            "failure".into(),
            "same".into(),
            move |_, mut stop| async move {
                started.send_replace(true);
                cancelled(&mut stop).await;
                anyhow::bail!("fixture reaping failed")
            },
        )
        .await
        .unwrap();
    ready.wait_for(|v| *v).await.unwrap();
    let report = scheduler.cancel("failure").await.unwrap().unwrap();
    assert_eq!(report.state, NetworkState::Failed);
    assert!(report.message.contains("reaping failed"));
}

#[tokio::test]
async fn unconfirmed_cleanup_blocks_later_maintenance_even_after_worker_exit() {
    let (_root, scheduler) = scheduler();
    let (started, mut ready) = watch::channel(false);
    scheduler
        .submit(
            "unsafe".into(),
            "same".into(),
            move |_, mut stop| async move {
                started.send_replace(true);
                cancelled(&mut stop).await;
                Err(CleanupUnconfirmed("fixture cannot confirm process exit".into()).into())
            },
        )
        .await
        .unwrap();
    ready.wait_for(|v| *v).await.unwrap();
    let error = scheduler.cancel("unsafe").await.unwrap_err();
    assert!(matches!(
        crate::error::code(&error),
        ErrorCode::OutcomeUnknown
    ));
    assert!(error.chain().any(|cause| cause.is::<CleanupUnconfirmed>()));
    assert!(scheduler.jobs.lock().await.is_empty());
    assert_eq!(
        scheduler
            .store
            .network_report("unsafe", None)
            .await
            .unwrap()
            .state,
        NetworkState::Failed
    );
    assert!(scheduler.pause().await.is_err());
    assert!(scheduler
        .submit("later".into(), "new".into(), |_, _| async { Ok(()) })
        .await
        .is_err());
}

#[tokio::test]
async fn overlapping_maintenance_waits_for_cleanup_and_prior_guard() {
    let (_root, scheduler) = scheduler();
    let (started, mut ready) = watch::channel(false);
    let (cleanup, mut release) = watch::channel(false);
    scheduler
        .submit(
            "maintenance".into(),
            "same".into(),
            move |_, mut stop| async move {
                started.send_replace(true);
                cancelled(&mut stop).await;
                let _ = release.wait_for(|v| *v).await;
                Ok(())
            },
        )
        .await
        .unwrap();
    ready.wait_for(|v| *v).await.unwrap();
    let owner = scheduler.clone();
    let first = tokio::spawn(async move { owner.pause().await });
    until(|| scheduler.paused.load(Ordering::SeqCst)).await;
    let owner = scheduler.clone();
    let mut second = tokio::spawn(async move { owner.pause().await });
    assert!(tokio::time::timeout(Duration::from_millis(20), &mut second)
        .await
        .is_err());
    cleanup.send_replace(true);
    let first_guard = first.await.unwrap().unwrap();
    assert!(tokio::time::timeout(Duration::from_millis(20), &mut second)
        .await
        .is_err());
    drop(first_guard);
    let second_guard = second.await.unwrap().unwrap();
    assert!(scheduler.paused.load(Ordering::SeqCst));
    drop(second_guard);
    assert!(!scheduler.paused.load(Ordering::SeqCst));
}

#[test]
fn unsupported_paths_do_not_claim_resolver_or_tun_safety() {
    let mut request: NetworkDiagnosticsRequest = NetToolRequest {
        egress: astra_model::nettool::Egress::System,
        probe: astra_model::nettool::Probe::Tcp {
            host: "127.0.0.1".into(),
            port: 80,
        },
        count: 1,
        timeout_ms: "100".into(),
        interval_ms: "0".into(),
    }
    .into();
    validate(&request).unwrap();
    request.egress = astra_model::nettool::Egress::Interface("eth0".into());
    validate(&request).unwrap();
    request.probe = astra_model::nettool::Probe::Tcp {
        host: "example.test".into(),
        port: 80,
    };
    // Resolver applicability is checked against the Agent snapshot before sockets open.
    validate(&request).unwrap();
    request.resolver = Some("udp://127.0.0.1".into());
    validate(&request).unwrap();
    request.egress = astra_model::nettool::Egress::System;
    request.probe = astra_model::nettool::Probe::Icmp {
        host: "127.0.0.1".into(),
    };
    validate(&request).unwrap(); // Actual ICMP execution must establish TUN/path evidence.
}

#[test]
fn diagnostic_endpoint_validation_rejects_url_credentials_without_echoing_them() {
    let mut request: NetworkDiagnosticsRequest = NetToolRequest {
        egress: astra_model::nettool::Egress::System,
        probe: astra_model::nettool::Probe::Dns {
            name: "example.test".into(),
            record: "A".into(),
            server: "https://resolver.invalid/private?token=private-query".into(),
        },
        count: 1,
        interval_ms: "0".into(),
        timeout_ms: "100".into(),
    }
    .into();
    // DoH query parameters remain usable, but are hidden by the new core's
    // report labels. The runner requires its new capability for DoH.
    validate(&request).unwrap();
    for value in [
        "https://user:private-password@resolver.invalid/dns-query",
        "tls://user:private-password@resolver.invalid:853",
        "udp://127.0.0.1?token=private-query",
    ] {
        request.probe = astra_model::nettool::Probe::Dns {
            name: "example.test".into(),
            record: "A".into(),
            server: value.into(),
        };
        let error = validate(&request).unwrap_err().to_string();
        assert!(!error.contains("private-password") && !error.contains("private-query"));
    }
    request.probe = astra_model::nettool::Probe::Tcp {
        host: "example.test".into(),
        port: 443,
    };
    request.resolver = Some("https://user:private-password@resolver.invalid/dns-query".into());
    assert!(!validate(&request)
        .unwrap_err()
        .to_string()
        .contains("private-password"));
}

#[tokio::test(flavor = "current_thread")]
async fn nat_reservation_exists_before_its_worker_is_first_polled() {
    let (_root, scheduler) = scheduler();
    let old = scheduler.legacy_permit().await.unwrap();
    let (nat_started, mut nat_ready) = watch::channel(false);
    scheduler
        .submit_mode(
            "reserved-nat".into(),
            "nat".into(),
            true,
            move |_, mut cancel| async move {
                nat_started.send_replace(true);
                cancelled(&mut cancel).await;
                Ok(())
            },
        )
        .await
        .unwrap();
    // No yield/sleep: the old implementation's spawned NAT worker had not yet
    // entered acquire_many, and a later one-unit legacy waiter took the spare slot.
    let mut later = Box::pin(scheduler.legacy_permit());
    assert!(matches!(
        std::future::poll_fn(|cx| Poll::Ready(later.as_mut().poll(cx))).await,
        Poll::Pending
    ));
    drop(old);
    nat_ready.wait_for(|ready| *ready).await.unwrap();
    assert!(tokio::time::timeout(Duration::from_millis(20), &mut later)
        .await
        .is_err());
    scheduler.cancel("reserved-nat").await.unwrap();
    drop(later.await.unwrap());
}

#[tokio::test]
async fn maintenance_waits_for_legacy_cleanup_and_prevents_new_legacy_work() {
    let (_root, scheduler) = scheduler();
    let legacy = scheduler.legacy_permit().await.unwrap();
    let owner = scheduler.clone();
    let mut pause = tokio::spawn(async move { owner.pause().await });
    until(|| scheduler.paused.load(Ordering::SeqCst)).await;
    assert!(scheduler.legacy_permit().await.is_err());
    assert!(tokio::time::timeout(Duration::from_millis(20), &mut pause)
        .await
        .is_err());
    drop(legacy);
    let paused = pause.await.unwrap().unwrap();
    assert!(scheduler.legacy_permit().await.is_err());
    drop(paused);
    assert!(scheduler.legacy_permit().await.is_ok());
}

#[tokio::test]
async fn expired_queued_reservation_never_starts_and_releases_fifo_position() {
    let (_root, scheduler) = scheduler();
    let old = scheduler
        .running
        .clone()
        .acquire_many_owned(2)
        .await
        .unwrap();
    scheduler
        .store
        .begin_network("queued-deadline", "same")
        .await
        .unwrap();
    let reservation = Admission::reserve(scheduler.running.clone(), 2)
        .await
        .unwrap();
    assert!(reservation.waiting());
    let (stop, cancel) = watch::channel(false);
    scheduler
        .work(
            "queued-deadline",
            stop,
            cancel,
            tokio::time::Instant::now() + Duration::from_millis(20),
            WorkAdmission {
                units: 2,
                admission: reservation,
                queued: None,
            },
            |_, _| async { panic!("expired queued probe started") },
        )
        .await
        .unwrap();
    assert_eq!(
        scheduler
            .store
            .network_report("queued-deadline", None)
            .await
            .unwrap()
            .state,
        NetworkState::Partial
    );
    drop(old);
    assert!(scheduler.legacy_permit().await.is_ok());
}

#[test]
fn trace_rejects_excessive_per_probe_timeout_before_admission() {
    let mut request: NetworkDiagnosticsRequest = astra_model::nettool::Request {
        config: String::new(),
        egress: astra_model::nettool::Egress::System,
        probe: astra_model::nettool::Probe::Trace {
            host: "192.0.2.1".into(),
            max_hops: 30,
            probes_per_hop: 3,
        },
        count: 1,
        interval_ms: 0,
        timeout_ms: 1000,
    }
    .into();
    validate(&request).unwrap();
    request.timeout_ms = "1001".into();
    assert!(validate(&request).is_err());
}

#[tokio::test]
async fn legacy_cleanup_failure_during_maintenance_wait_is_rechecked_before_replacement() {
    let (_root, scheduler) = scheduler();
    let legacy = scheduler.legacy_permit().await.unwrap();
    let owner = scheduler.clone();
    let pause = tokio::spawn(async move { owner.pause().await });
    until(|| scheduler.paused.load(Ordering::SeqCst)).await;
    let result: Result<()> =
        Err(CleanupUnconfirmed("legacy fixture cannot confirm exit".into()).into());
    scheduler.record_legacy_cleanup(&result);
    drop(legacy);
    assert!(pause.await.unwrap().is_err());
    assert!(scheduler.legacy_permit().await.is_err());
}

#[tokio::test]
async fn closed_scheduler_or_queue_is_internal_not_capacity_conflict() {
    let (_root, scheduler) = scheduler();
    scheduler.waiting.close();
    let occupied = scheduler
        .running
        .clone()
        .acquire_many_owned(2)
        .await
        .unwrap();
    let error = scheduler
        .submit("closed-queue".into(), "input".into(), |_, _| async {
            panic!("closed queue must not start work")
        })
        .await
        .unwrap_err();
    assert!(matches!(crate::error::code(&error), ErrorCode::Internal));
    assert!(scheduler.store.task("closed-queue").await.is_err());
    drop(occupied);
    scheduler.running.close();
    let error = scheduler
        .submit("closed-running".into(), "input".into(), |_, _| async {
            panic!("closed scheduler must not start work")
        })
        .await
        .unwrap_err();
    assert!(matches!(crate::error::code(&error), ErrorCode::Internal));
    assert!(scheduler.store.task("closed-running").await.is_err());
}

#[tokio::test]
async fn corrupt_maintenance_record_is_internal_not_busy() {
    let (root, scheduler) = scheduler();
    let directory = astra_management::maintenance::directory(root.path());
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("current.json"), "not valid JSON").unwrap();
    let error = scheduler
        .submit("corrupt-record".into(), "input".into(), |_, _| async {
            panic!("corrupt maintenance state must not start work")
        })
        .await
        .unwrap_err();
    assert!(matches!(crate::error::code(&error), ErrorCode::Internal));
    assert!(scheduler.store.task("corrupt-record").await.is_err());
}

#[tokio::test]
async fn missing_worker_completion_does_not_acknowledge_cancellation() {
    let (_root, scheduler) = scheduler();
    let (cancel, _receiver) = watch::channel(false);
    let (done, completion) = watch::channel(None);
    drop(done);
    scheduler.jobs.lock().await.insert(
        "lost-completion".into(),
        Job {
            cancel,
            done: completion,
            handle: tokio::spawn(async {}),
        },
    );
    let error = scheduler.cancel("lost-completion").await.unwrap_err();
    assert!(matches!(
        crate::error::code(&error),
        ErrorCode::OutcomeUnknown
    ));
}

#[tokio::test]
async fn cancellation_terminal_write_failure_is_unknown_and_never_replayed() {
    let (root, scheduler) = scheduler();
    let db = rusqlite::Connection::open(root.path().join("jobs.sqlite")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER fail_cancel BEFORE UPDATE ON network_reports
        WHEN NEW.id='cancel-write' AND json_extract(NEW.body,'$.state')='cancelled'
        BEGIN SELECT RAISE(ABORT,'fixture write failed'); END;",
    )
    .unwrap();
    let (started, mut ready) = watch::channel(false);
    scheduler
        .submit(
            "cancel-write".into(),
            "input".into(),
            move |_, mut stop| async move {
                started.send_replace(true);
                cancelled(&mut stop).await;
                Ok(())
            },
        )
        .await
        .unwrap();
    ready.wait_for(|started| *started).await.unwrap();
    let error = scheduler.cancel("cancel-write").await.unwrap_err();
    assert!(matches!(
        crate::error::code(&error),
        ErrorCode::OutcomeUnknown
    ));
    assert!(matches!(
        scheduler.store.task("cancel-write").await.unwrap().state,
        TaskState::Running
    ));
    let error = scheduler
        .submit("cancel-write".into(), "input".into(), |_, _| async {
            panic!("lost cancellation outcome must not replay the probe")
        })
        .await
        .unwrap_err();
    assert!(matches!(
        crate::error::code(&error),
        ErrorCode::OutcomeUnknown
    ));
    let error = scheduler
        .submit("after-failure".into(), "new".into(), |_, _| async {
            panic!("unrecovered storage failure must block admission")
        })
        .await
        .unwrap_err();
    assert!(matches!(crate::error::code(&error), ErrorCode::Conflict));
}

#[test]
fn validation_distinguishes_bad_parameters_from_unsupported_transport() {
    let mut request: NetworkDiagnosticsRequest = NetToolRequest {
        egress: astra_model::nettool::Egress::System,
        probe: astra_model::nettool::Probe::Dns {
            name: "example.test".into(),
            record: "A".into(),
            server: "udp://127.0.0.1".into(),
        },
        count: 1,
        interval_ms: "0".into(),
        timeout_ms: "invalid".into(),
    }
    .into();
    assert!(matches!(
        crate::error::code(&validate(&request).unwrap_err()),
        ErrorCode::InvalidRequest
    ));
    request.timeout_ms = "100".into();
    if let astra_model::nettool::Probe::Dns { server, .. } = &mut request.probe {
        *server = "ftp://127.0.0.1".into();
    }
    let error = validate(&request)
        .unwrap_err()
        .context("different caller wording");
    assert!(matches!(crate::error::code(&error), ErrorCode::Unsupported));
}

#[tokio::test]
async fn cleanup_latch_survives_a_simultaneous_report_write_failure() {
    let (root, scheduler) = scheduler();
    let db = rusqlite::Connection::open(root.path().join("jobs.sqlite")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER fail_cleanup_report BEFORE UPDATE ON network_reports
        WHEN NEW.id='cleanup-write' AND json_extract(NEW.body,'$.state')='failed'
        BEGIN SELECT RAISE(ABORT,'fixture cleanup report write failed'); END;",
    )
    .unwrap();
    let (started, mut ready) = watch::channel(false);
    scheduler
        .submit(
            "cleanup-write".into(),
            "input".into(),
            move |_, mut stop| async move {
                started.send_replace(true);
                cancelled(&mut stop).await;
                Err(crate::error::classified(
                    ErrorCode::Internal,
                    CleanupUnconfirmed("fixture child exit unconfirmed".into()).into(),
                ))
            },
        )
        .await
        .unwrap();
    ready.wait_for(|started| *started).await.unwrap();
    let error = scheduler.cancel("cleanup-write").await.unwrap_err();
    assert!(matches!(
        crate::error::code(&error),
        ErrorCode::OutcomeUnknown
    ));
    assert!(scheduler.cleanup_failure.lock().unwrap().is_some());
    assert!(scheduler.outcome_failure("cleanup-write").is_some());
    assert!(scheduler.pause().await.is_err());
}
