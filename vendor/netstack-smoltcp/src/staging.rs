//! Application staging only: never replaces a TCP socket or its protocol windows.
use smoltcp::storage::RingBuffer;
use std::{
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
    time::{Duration, Instant},
};
static BYTES: AtomicUsize = AtomicUsize::new(0);
static GROWS: AtomicU64 = AtomicU64::new(0);
static SHRINKS: AtomicU64 = AtomicU64::new(0);
pub fn metrics() -> (usize, u64, u64) {
    (
        BYTES.load(Ordering::Relaxed),
        GROWS.load(Ordering::Relaxed),
        SHRINKS.load(Ordering::Relaxed),
    )
}
/// Optional allocation-only observer; never called on packet reads or writes.
#[derive(Clone, Copy)]
pub enum Event {
    Allocated(usize),
    Released(usize),
    Grown(usize),
    Shrunk(usize),
}
static OBSERVER: std::sync::OnceLock<fn(Event)> = std::sync::OnceLock::new();
pub fn observe(observer: fn(Event)) {
    let _ = OBSERVER.set(observer);
}
fn emit(event: Event) {
    if let Some(observer) = OBSERVER.get() {
        observer(event);
    }
}
pub(crate) struct Staging {
    ring: RingBuffer<'static, u8>,
    min: usize,
    max: usize,
    last: Instant,
}
impl Staging {
    pub fn new(max: usize, adaptive: bool) -> Self {
        let min = if adaptive { max.min(4096) } else { max };
        BYTES.fetch_add(min, Ordering::Relaxed);
        emit(Event::Allocated(min));
        Self {
            ring: RingBuffer::new(vec![0; min]),
            min,
            max,
            last: Instant::now(),
        }
    }
    #[cfg(test)]
    pub fn capacity(&self) -> usize {
        self.ring.capacity()
    }
    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }
    pub fn is_full(&self) -> bool {
        self.ring.is_full()
    }
    pub fn grow_if_full(&mut self, backlog: bool) {
        if backlog && self.ring.is_full() && self.ring.capacity() < self.max {
            self.resize((self.ring.capacity() * 2).min(self.max));
        }
    }
    fn resize(&mut self, size: usize) {
        let old = self.ring.capacity();
        let mut replacement = RingBuffer::new(vec![0; size]);
        let n = self
            .ring
            .dequeue_slice(replacement.enqueue_many(self.ring.len()));
        debug_assert_eq!(n, replacement.len());
        self.ring = replacement;
        if size > old {
            BYTES.fetch_add(size - old, Ordering::Relaxed);
            GROWS.fetch_add(1, Ordering::Relaxed);
            emit(Event::Grown(size - old));
        } else {
            BYTES.fetch_sub(old - size, Ordering::Relaxed);
            SHRINKS.fetch_add(1, Ordering::Relaxed);
            emit(Event::Shrunk(old - size));
        }
    }
    pub fn idle(&mut self, now: Instant) {
        if self.ring.is_empty()
            && self.ring.capacity() > self.min
            && now.duration_since(self.last) >= Duration::from_secs(30)
        {
            self.resize(self.min);
        }
    }
    pub fn enqueue_slice(&mut self, bytes: &[u8]) -> usize {
        self.grow_if_full(!bytes.is_empty());
        let n = self.ring.enqueue_slice(bytes);
        if n > 0 {
            self.last = Instant::now();
        }
        n
    }
    pub fn dequeue_slice(&mut self, bytes: &mut [u8]) -> usize {
        let n = self.ring.dequeue_slice(bytes);
        if n > 0 {
            self.last = Instant::now();
        }
        n
    }
}
impl Drop for Staging {
    fn drop(&mut self) {
        BYTES.fetch_sub(self.ring.capacity(), Ordering::Relaxed);
        emit(Event::Released(self.ring.capacity()));
    }
}
#[cfg(test)]
#[path = "test/staging_test.rs"]
mod tests;
