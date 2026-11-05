use super::*;
#[tokio::test]
async fn reports_return_error_and_panic_with_task_identity() {
    let mut returned = Task::spawn("DNS UDP", async {});
    assert_eq!(returned.failure().await, "DNS UDP stopped unexpectedly");
    let mut failed = Task::spawn_result("server TCP", async { Err::<(), _>("socket closed") });
    assert_eq!(failed.failure().await, "server TCP failed: socket closed");
    let mut panicked = Task::spawn("manager", async { panic!("injected failure") });
    let message = panicked.failure().await;
    assert!(message.contains("manager") && message.contains("injected failure"));
}
#[tokio::test]
async fn cancelled_wait_keeps_ownership_and_drop_cancels_task() {
    let (done, receiver) = tokio::sync::oneshot::channel::<()>();
    let mut task = Task::spawn("worker", async move {
        let _done = done;
        std::future::pending::<()>().await;
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(10), task.failure())
            .await
            .is_err()
    );
    assert!(!task.is_finished());
    drop(task);
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(1), receiver)
            .await
            .unwrap()
            .is_err()
    );
}
