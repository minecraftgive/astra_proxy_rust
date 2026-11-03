//! Ownership and error reporting for tasks which must live as long as a service.
use std::{fmt::Display, future::Future};
use tokio::task::JoinHandle;

pub struct Task {
    name: String,
    handle: JoinHandle<Result<(), String>>,
}
impl Task {
    pub fn spawn(
        name: impl Into<String>,
        future: impl Future<Output = ()> + Send + 'static,
    ) -> Self {
        Self::spawn_result(name, async move {
            future.await;
            Ok::<(), String>(())
        })
    }
    pub fn spawn_result<E: Display>(
        name: impl Into<String>,
        future: impl Future<Output = Result<(), E>> + Send + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            handle: tokio::spawn(async move { future.await.map_err(|error| format!("{error:#}")) }),
        }
    }
    pub fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }
    pub fn abort(&self) {
        self.handle.abort();
    }
    /// Every completion is unexpected until the owner begins shutdown.
    /// Cancel-safe: callers can select this against the normal shutdown path.
    pub async fn failure(&mut self) -> String {
        match (&mut self.handle).await {
            Ok(Ok(())) => format!("{} stopped unexpectedly", self.name),
            Ok(Err(error)) => format!("{} failed: {error}", self.name),
            Err(error) => format!("{} task failed: {error}", self.name),
        }
    }
    pub async fn check(&mut self) -> Option<String> {
        if self.is_finished() {
            Some(self.failure().await)
        } else {
            None
        }
    }
}
impl Drop for Task {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Errors which do not invalidate the listener. Other errors must reach its supervisor.
pub fn transient_io(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::OutOfMemory
    ) || matches!(error.raw_os_error(), Some(23 | 24 | 10024 | 10055))
}

#[cfg(test)]
mod tests {
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
}
