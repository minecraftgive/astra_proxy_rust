//! One weakly-owned timer per store, never one timer per query or entry.
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
pub(crate) trait Maintain: Send + 'static {
    fn maintain(&mut self, now: Instant);
}
#[derive(Default)]
pub(crate) struct Maintenance(OnceLock<tokio::task::JoinHandle<()>>);
impl Maintenance {
    pub fn start<T: Maintain>(&self, state: &Arc<Mutex<T>>) {
        if self.0.get().is_some() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        self.0.get_or_init(|| {
            let weak = Arc::downgrade(state);
            runtime.spawn(async move {
                let _active =
                    astra_observe::gauge!("dns.maintenance.active", "DNS 清理任务", "count")
                        .enter();
                loop {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    let Some(state) = weak.upgrade() else { break };
                    if let Ok(mut state) = state.lock() {
                        state.maintain(Instant::now());
                    };
                }
            })
        });
    }
}
impl Drop for Maintenance {
    fn drop(&mut self) {
        if let Some(task) = self.0.get() {
            task.abort();
        }
    }
}
#[derive(Default)]
pub(crate) struct Underused(Option<Instant>);
impl Underused {
    pub fn ready(&mut self, len: usize, capacity: usize, now: Instant) -> bool {
        if capacity == 0 || len.saturating_mul(4) >= capacity {
            self.0 = None;
            return false;
        }
        let since = self.0.get_or_insert(now);
        if now.duration_since(*since) < Duration::from_secs(30) {
            return false;
        }
        self.0 = None;
        true
    }
}
