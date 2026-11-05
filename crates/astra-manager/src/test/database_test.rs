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
    let write = database.call(move |store| {
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
