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
#[path = "test/supervision_test.rs"]
mod tests;
