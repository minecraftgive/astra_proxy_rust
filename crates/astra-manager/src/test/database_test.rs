use super::*;
use std::time::Duration;
#[tokio::test]
async fn short_contention_recovers_without_blocking_the_async_runtime() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("management.sqlite");
    let database = Database::open(path.clone(), 100).unwrap();
    let blocker = rusqlite::Connection::open(path).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let (started, worker_started) = tokio::sync::oneshot::channel();
    let write = database.write(move |store| {
        let _ = started.send(());
        store.begin("wait-once", "input")
    });
    tokio::pin!(write);
    tokio::select! {
        result=&mut write=>panic!("write bypassed external lock: {result:?}"),
        _=worker_started=>{},
    }
    assert!(tokio::time::timeout(Duration::from_millis(50), &mut write)
        .await
        .is_err());
    blocker.execute_batch("ROLLBACK").unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(2), write)
        .await
        .unwrap()
        .unwrap()
        .is_none());
    assert!(matches!(
        database.begin("wait-once", "input").await.unwrap(),
        Some(ResponseBody::Task(_))
    ));
}
#[tokio::test]
async fn worker_panic_is_fatal_and_pending_operation_is_never_replayed() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("management.sqlite");
    let database = Database::open(path.clone(), 100).unwrap();
    database
        .begin("accepted-before-panic", "input")
        .await
        .unwrap();
    let error = database
        .call::<()>(|_| panic!("injected SQLite worker failure"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("worker panicked"));
    assert!(matches!(crate::error::code(&error), ErrorCode::Internal));
    let closed = tokio::time::timeout(Duration::from_secs(1), database.closed())
        .await
        .unwrap();
    assert!(closed.to_string().contains("not restarted"));
    assert!(database
        .begin("must-not-run", "input")
        .await
        .unwrap_err()
        .to_string()
        .contains("worker panicked"));
    drop(database);
    // Only an explicit new management startup may open another worker.
    let restarted = Database::open(path, 100).unwrap();
    assert!(matches!(
        restarted
            .begin("accepted-before-panic", "input")
            .await
            .unwrap(),
        Some(ResponseBody::Task(Task {
            state: TaskState::OutcomeUnknown,
            ..
        }))
    ));
    assert!(restarted.task("must-not-run").await.is_err());
}

#[tokio::test]
async fn committed_write_without_receipt_is_unknown_but_later_rejected_writes_are_internal() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("management.sqlite");
    let database = Database::open(path.clone(), 100).unwrap();
    let error = database
        .write::<()>(|store| {
            store.begin_network("committed-before-receipt-loss", "input")?;
            panic!("injected failure after commit and before sending the receipt");
        })
        .await
        .unwrap_err();
    assert!(matches!(
        crate::error::code(&error),
        ErrorCode::OutcomeUnknown
    ));
    assert!(matches!(
        crate::error::code(
            &database
                .begin_network("never-queued", "input")
                .await
                .unwrap_err()
        ),
        ErrorCode::Internal
    ));
    assert!(matches!(
        crate::error::code(
            &database
                .task("committed-before-receipt-loss")
                .await
                .unwrap_err()
        ),
        ErrorCode::Internal
    ));
    drop(database);

    let restarted = Database::open(path, 100).unwrap();
    let task = restarted
        .network_existing("committed-before-receipt-loss", "input")
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(task.state, TaskState::OutcomeUnknown));
    let report = restarted.network_report(&task.id, None).await.unwrap();
    assert_eq!(report.state, NetworkState::Interrupted);
    assert!(matches!(
        restarted.begin(&task.id, "input").await.unwrap(),
        Some(ResponseBody::Task(Task {
            state: TaskState::OutcomeUnknown,
            ..
        }))
    ));
    assert!(restarted
        .network_existing("never-queued", "input")
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn delivered_worker_errors_keep_their_types_and_ignore_message_keywords() {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::open(directory.path().join("management.sqlite"), 100).unwrap();
    let error = database
        .write::<()>(|_| anyhow::bail!("Conflict: not found; outcome unknown; not replayed"))
        .await
        .unwrap_err();
    assert!(matches!(crate::error::code(&error), ErrorCode::Internal));
    assert!(error
        .chain()
        .any(|cause| cause.is::<crate::error::AgentError>()));
    database.begin_network("conflict", "first").await.unwrap();
    let error = database
        .network_existing("conflict", "second")
        .await
        .unwrap_err()
        .context("database worker delivered a normal error result");
    assert!(matches!(crate::error::code(&error), ErrorCode::Conflict));
    assert!(matches!(
        crate::error::code(&database.task("absent").await.unwrap_err()),
        ErrorCode::NotFound
    ));
    assert!(
        database.tasks(10).await.is_ok(),
        "ordinary errors must not stop the worker"
    );
}
