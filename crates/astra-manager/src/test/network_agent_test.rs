use super::*;
use std::time::Duration;
use tokio::{sync::oneshot, time::timeout};

fn test_agent(root: &Path) -> Arc<AgentService> {
    AgentService::new(ManagerConfig {
        state_dir: root.join("state"),
        config_dir: root.join("config"),
        bin_dir: root.join("bin"),
        ..Default::default()
    })
    .unwrap()
}

fn diagnostic() -> Command {
    Command::NetworkDiagnostics {
        instance: "proxy".into(),
        request: NetToolRequest {
            egress: astra_model::nettool::Egress::System,
            probe: astra_model::nettool::Probe::Tcp {
                host: "127.0.0.1".into(),
                port: 9,
            },
            count: 1,
            interval_ms: "0".into(),
            timeout_ms: "100".into(),
        }
        .into(),
    }
}

fn execute(id: &str, command: Command) -> Request {
    Request::new(
        format!("request-{id}"),
        RequestBody::Execute {
            operation_id: id.into(),
            command,
        },
    )
}

async fn finished_report(agent: &AgentService, id: &str) -> NetworkReport {
    timeout(Duration::from_secs(3), async {
        loop {
            let report = agent.store.network_report(id, None).await.unwrap();
            if report.state.terminal() {
                return report;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn diagnostics_bypass_mutation_lock_while_configuration_commands_remain_serialized() {
    let root = tempfile::tempdir().unwrap();
    let agent = test_agent(root.path());
    let serial = agent.mutation.lock().await;
    let mutation = agent.handle(execute(
        "config-change",
        Command::Start {
            instance: "proxy".into(),
        },
    ));
    tokio::pin!(mutation);
    assert!(timeout(Duration::from_millis(30), &mut mutation)
        .await
        .is_err());
    // Persisting a mutating operation also remains behind its original lock.
    assert!(agent.store.task("config-change").await.is_err());
    let response = timeout(
        Duration::from_secs(2),
        agent.handle(execute("diagnostic", diagnostic())),
    )
    .await
    .unwrap();
    assert!(matches!(
        response.result,
        Ok(ResponseBody::Task(Task {
            state: TaskState::Running,
            ..
        }))
    ));
    assert!(agent.store.task("diagnostic").await.is_ok());
    drop(serial);
    // This fixture has no native actor; failure must occur only after dispatch.
    assert!(timeout(Duration::from_secs(2), &mut mutation)
        .await
        .unwrap()
        .result
        .is_err());
    assert!(matches!(
        agent.store.task("config-change").await.unwrap().state,
        TaskState::Failed
    ));
    assert_eq!(
        finished_report(&agent, "diagnostic").await.state,
        NetworkState::Failed
    );
}

#[tokio::test]
async fn duplicate_diagnostic_returns_original_task_and_input_changes_conflict() {
    let root = tempfile::tempdir().unwrap();
    let agent = test_agent(root.path());
    assert!(agent
        .handle(execute("once", diagnostic()))
        .await
        .result
        .is_ok());
    let original = finished_report(&agent, "once").await;
    let task = agent.store.task("once").await.unwrap();
    let retry = agent
        .handle(execute("once", diagnostic()))
        .await
        .result
        .unwrap();
    let ResponseBody::Task(retry) = retry else {
        panic!("Expected original task")
    };
    assert_eq!(
        serde_json::to_value(retry).unwrap(),
        serde_json::to_value(task).unwrap()
    );
    let after = agent.store.network_report("once", None).await.unwrap();
    assert_eq!(
        serde_json::to_value(original).unwrap(),
        serde_json::to_value(after).unwrap()
    );
    let mut changed = diagnostic();
    if let Command::NetworkDiagnostics { request, .. } = &mut changed {
        request.count = 2;
    }
    assert!(matches!(
        agent.handle(execute("once", changed)).await.result,
        Err(ApiError {
            code: ErrorCode::Conflict,
            ..
        })
    ));
}

#[tokio::test]
async fn cancel_task_waits_for_cleanup_and_durable_terminal_report_outside_mutation_lock() {
    let root = tempfile::tempdir().unwrap();
    let agent = test_agent(root.path());
    let (started, running) = oneshot::channel();
    let (observed, cancellation_observed) = oneshot::channel();
    let (cleanup, release_cleanup) = oneshot::channel();
    agent
        .diagnostics
        .submit(
            "running".into(),
            "fixture".into(),
            move |output, mut cancel| async move {
                output
                    .send(astra_model::nettool::Event::Attempt(
                        astra_model::nettool::Attempt {
                            seq: 1,
                            rtt_ms: Some(1.0),
                            detail: "evidence-must-not-be-copied-into-cancel-operation".into(),
                            error: None,
                        },
                    ))
                    .await
                    .unwrap();
                started.send(()).unwrap();
                crate::network::cancelled(&mut cancel).await;
                observed.send(()).unwrap();
                release_cleanup.await.unwrap();
                Ok(())
            },
        )
        .await
        .unwrap();
    running.await.unwrap();
    let serial = agent.mutation.lock().await;
    let cancel_agent = agent.clone();
    let mut cancellation = tokio::spawn(async move {
        cancel_agent
            .handle(execute(
                "cancel",
                Command::CancelTask {
                    operation_id: "running".into(),
                },
            ))
            .await
    });
    timeout(Duration::from_secs(2), cancellation_observed)
        .await
        .unwrap()
        .unwrap();
    assert!(timeout(Duration::from_millis(30), &mut cancellation)
        .await
        .is_err());
    assert!(!agent
        .store
        .network_report("running", None)
        .await
        .unwrap()
        .state
        .terminal());
    cleanup.send(()).unwrap();
    let response = timeout(Duration::from_secs(2), cancellation)
        .await
        .unwrap()
        .unwrap();
    let Ok(ResponseBody::NetworkReport(report)) = response.result else {
        panic!("Expected terminal report")
    };
    assert_eq!(report.state, NetworkState::Cancelled);
    assert!(report.finished_ms.is_some());
    assert!(report
        .items
        .iter()
        .any(|item| item.data["detail"] == "evidence-must-not-be-copied-into-cancel-operation"));
    let db = rusqlite::Connection::open_with_flags(
        agent.config.state_dir.join("management.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let receipt: String = db
        .query_row(
            "SELECT response FROM operations WHERE id='cancel'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!receipt.contains("evidence-must-not-be-copied-into-cancel-operation"));
    assert!(matches!(
        serde_json::from_str::<ResponseBody>(&receipt).unwrap(),
        ResponseBody::Cancellation(Cancellation::AlreadyFinished(_))
    ));
    assert!(
        receipt.len() < 2048,
        "cancellation receipts must stay compact"
    );
    assert!(matches!(
        agent.store.task("running").await.unwrap().state,
        TaskState::Failed
    ));
    drop(serial);
}

#[tokio::test]
async fn remote_diagnostic_rejects_a_different_agent_before_admission() {
    let root = tempfile::tempdir().unwrap();
    let agent = test_agent(root.path());
    let mut request = execute("wrong-agent", diagnostic());
    request.agent_id = Some("another-agent".into());
    assert!(matches!(
        agent.handle_remote(request).await.result,
        Err(ApiError {
            code: ErrorCode::NotFound,
            ..
        })
    ));
    assert!(agent.store.task("wrong-agent").await.is_err());
}
