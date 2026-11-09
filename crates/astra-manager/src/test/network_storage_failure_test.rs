use super::*;
use std::time::Duration;

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

fn execute(id: &str) -> Request {
    Request::new(
        format!("request-{id}"),
        RequestBody::Execute {
            operation_id: id.into(),
            command: diagnostic(),
        },
    )
}

fn assert_unknown(response: Response) {
    let Err(ApiError { code, message, .. }) = response.result else {
        panic!("Expected an explicit unknown outcome, not a stale running task")
    };
    assert!(matches!(code, ErrorCode::OutcomeUnknown));
    assert!(message.contains("fixture terminal storage failure"));
}

#[tokio::test]
async fn terminal_storage_failure_is_queryable_and_blocks_new_admission() {
    let root = tempfile::tempdir().unwrap();
    let config = ManagerConfig {
        state_dir: root.path().join("state"),
        config_dir: root.path().join("config"),
        bin_dir: root.path().join("bin"),
        ..Default::default()
    };
    let agent = AgentService::new(config.clone()).unwrap();
    let db = rusqlite::Connection::open(config.state_dir.join("management.sqlite")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER fail_terminal_storage BEFORE UPDATE ON network_reports
         WHEN NEW.id='storage-failure' AND json_extract(NEW.body,'$.state')='complete'
         BEGIN SELECT RAISE(ABORT,'fixture terminal storage failure'); END;",
    )
    .unwrap();

    // Run the real scheduler against an actual SQLite failure. The local runner
    // completes successfully, so only persisting the terminal outcome fails.
    let fingerprint = store::revision(&serde_json::to_string(&diagnostic()).unwrap());
    agent
        .diagnostics
        .submit(
            "storage-failure".into(),
            fingerprint,
            |output, _| async move {
                output
                    .send(astra_model::nettool::Event::Attempt(
                        astra_model::nettool::Attempt {
                            seq: 1,
                            rtt_ms: Some(1.0),
                            detail: "local fixture evidence".into(),
                            error: None,
                        },
                    ))
                    .await?;
                Ok(())
            },
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while agent
            .diagnostics
            .outcome_failure("storage-failure")
            .is_none()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    assert_unknown(
        agent
            .handle(Request::new(
                "query-task",
                RequestBody::Query(Query::Task {
                    operation_id: "storage-failure".into(),
                }),
            ))
            .await,
    );
    assert_unknown(
        agent
            .handle(Request::new(
                "query-report",
                RequestBody::Query(Query::NetworkReport {
                    operation_id: "storage-failure".into(),
                    cursor: None,
                }),
            ))
            .await,
    );
    assert_unknown(agent.handle(execute("storage-failure")).await);

    let response = agent
        .handle(Request::new(
            "query-tasks",
            RequestBody::Query(Query::Tasks { limit: Some(10) }),
        ))
        .await;
    let Ok(ResponseBody::Tasks(tasks)) = response.result else {
        panic!("Expected compatible task list")
    };
    let task = tasks
        .iter()
        .find(|task| task.id == "storage-failure")
        .unwrap();
    assert!(matches!(task.state, TaskState::OutcomeUnknown));
    assert!(task.message.contains("fixture terminal storage failure"));

    let rejected = agent.handle(execute("new-job")).await;
    assert!(matches!(
        rejected.result,
        Err(ApiError {
            code: ErrorCode::Conflict,
            ..
        })
    ));
    assert!(agent.store.task("new-job").await.is_err());

    // Persistence failure cannot fabricate a terminal on-disk result. Recovery
    // on restart must mark this interrupted and preserve the original evidence.
    assert!(matches!(
        agent.store.task("storage-failure").await.unwrap().state,
        TaskState::Running
    ));
    db.execute_batch("DROP TRIGGER fail_terminal_storage")
        .unwrap();
    drop(agent);
    let restarted = AgentService::new(config).unwrap();
    let report = restarted
        .store
        .network_report("storage-failure", None)
        .await
        .unwrap();
    assert_eq!(report.state, NetworkState::Interrupted);
    assert!(report
        .items
        .iter()
        .any(|item| item.data["detail"] == "local fixture evidence"));
    let replay = restarted.handle(execute("storage-failure")).await;
    assert!(matches!(
        replay.result,
        Ok(ResponseBody::Task(Task {
            state: TaskState::OutcomeUnknown,
            ..
        }))
    ));
    let cancel = restarted
        .handle(Request::new(
            "cancel-interrupted",
            RequestBody::Execute {
                operation_id: "cancel-after-restart".into(),
                command: Command::CancelTask {
                    operation_id: "storage-failure".into(),
                },
            },
        ))
        .await;
    assert!(matches!(
        cancel.result,
        Ok(ResponseBody::Cancellation(Cancellation::AlreadyFinished(_)))
    ));
}
