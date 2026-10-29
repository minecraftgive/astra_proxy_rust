//! Bounded, task-local operation progress shared by UI, manager and cores.
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Progress {
    pub timestamp_ms: u64,
    pub phase: String,
    pub detail: String,
    pub current: Option<u64>,
    pub total: Option<u64>,
}
pub type Sender = watch::Sender<Option<Progress>>;
tokio::task_local! { static CHANNEL: Sender; }

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
impl Progress {
    pub fn new(phase: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            timestamp_ms: now_ms(),
            phase: phase.into(),
            detail: detail.into(),
            current: None,
            total: None,
        }
    }
}
pub fn sender() -> Option<Sender> {
    CHANNEL.try_with(Clone::clone).ok()
}
pub fn report(phase: impl Into<String>, detail: impl Into<String>) {
    if let Some(sender) = sender() {
        sender.send_replace(Some(Progress::new(phase, detail)));
    }
}
pub fn counted(phase: impl Into<String>, detail: impl Into<String>, current: u64, total: u64) {
    if let Some(sender) = sender() {
        let mut progress = Progress::new(phase, detail);
        progress.current = Some(current);
        progress.total = Some(total);
        sender.send_replace(Some(progress));
    }
}
pub async fn scope<T>(sender: Sender, future: impl std::future::Future<Output = T>) -> T {
    CHANNEL.scope(sender, future).await
}
