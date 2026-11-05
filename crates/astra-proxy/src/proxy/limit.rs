//! A process-wide quota survives configuration generations and supports resizing.
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::Notify;
pub struct Limit {
    active: AtomicUsize,
    maximum: AtomicUsize,
    changed: Notify,
}
pub struct Permit(Arc<Limit>);
impl Limit {
    pub fn new(maximum: usize) -> Self {
        Self {
            active: AtomicUsize::new(0),
            maximum: AtomicUsize::new(maximum),
            changed: Notify::new(),
        }
    }
    pub fn resize(&self, maximum: usize) {
        self.maximum.store(maximum, Ordering::Release);
        self.changed.notify_waiters();
    }
    pub fn try_acquire_owned(self: Arc<Self>) -> Result<Permit, ()> {
        let mut count = self.active.load(Ordering::Acquire);
        loop {
            if count >= self.maximum.load(Ordering::Acquire) {
                return Err(());
            }
            match self.active.compare_exchange_weak(
                count,
                count + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(Permit(self)),
                Err(actual) => count = actual,
            }
        }
    }
    pub async fn acquire_owned(self: Arc<Self>) -> Result<Permit, ()> {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Ok(permit) = self.clone().try_acquire_owned() {
                return Ok(permit);
            }
            notified.await;
        }
    }
    pub async fn idle(&self) {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.active.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::AcqRel);
        self.0.changed.notify_waiters();
    }
}
#[cfg(test)]
#[path = "../test/proxy_limit_test.rs"]
mod tests;
