//! Per-frontend wake callback; models never depend on a global rendering loop.
use std::sync::Arc;
#[derive(Clone)]
pub struct Waker(Arc<dyn Fn() + Send + Sync>);
impl Waker {
    pub fn new(callback: impl Fn() + Send + Sync + 'static) -> Self {
        Self(Arc::new(callback))
    }
    pub fn wake(&self) {
        (self.0)();
    }
}
impl Default for Waker {
    fn default() -> Self {
        Self::new(|| {})
    }
}
