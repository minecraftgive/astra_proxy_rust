//! Reuse pool for snell `CommandConnectV2` sessions.
//!
//! Port of opensnell `components/snell/pool.go`. The Surge `snell-server`
//! v4 and v5 releases (4.0.0 through 5.0.1, like v6) serve any number of
//! sessions on a reuse-mode TCP connection, so a connection goes back to the
//! pool after every clean session. Idle entries age out after 15 s.
//!
//! Lifecycle of a pooled session:
//!
//! 1. `Pool::get` either pops the most-recently-returned idle conn (LIFO,
//!    warmest cache) or asks the factory to dial a fresh one.
//! 2. The caller writes the snell `CommandConnectV2` header and relays data.
//! 3. `PooledConn` returns the stream only after both peers have completed
//!    their zero-chunk half-close handshake. Incomplete sessions are dropped.

use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use meow_common::atomic::{AtomicU, Uint};
use meow_transport::Stream as TransportStream;

use super::protocol::Snell;

/// Type-erased snell stream used inside the pool. The underlying byte
/// stream may be a plain TCP connection or an obfs-wrapped one — the pool
/// doesn't care.
pub type PoolStream = Snell<Box<dyn TransportStream>>;

const DEFAULT_MAX_SIZE: usize = 10;
const DEFAULT_MAX_AGE: Duration = Duration::from_secs(15);
struct PooledEntry {
    conn: PoolStream,
    expires_at: Instant,
}

/// Bounded LIFO pool of warm snell streams.
pub struct Pool {
    max_size: usize,
    max_age: Duration,
    items: Mutex<Vec<PooledEntry>>,
    /// Bumped by [`Pool::reset`]; a conn checked out (or dialled) under an
    /// older generation is never re-pooled. Compared for equality only, so
    /// the mips32 `u32` wrap is harmless.
    generation: AtomicU,
}

impl Pool {
    pub fn new() -> Self {
        Self::with_limits(DEFAULT_MAX_SIZE, DEFAULT_MAX_AGE)
    }

    /// A pool with custom caps: at most `max_size` idle entries, each
    /// discarded after `max_age` idle.
    pub fn with_limits(max_size: usize, max_age: Duration) -> Self {
        Self {
            max_size,
            max_age,
            items: Mutex::new(Vec::new()),
            generation: AtomicU::new(0),
        }
    }

    /// Current generation. Capture it *before* taking an idle conn or
    /// dialling a fresh one and hand it back to [`Pool::put`], so a conn
    /// that straddles a [`Pool::reset`] is dropped instead of re-pooled.
    pub fn generation(&self) -> Uint {
        self.generation.load(Ordering::SeqCst)
    }

    /// Drop every idle conn and invalidate every checked-out one, so the
    /// next dial opens a fresh socket. Called when the outbound-interface
    /// binding changes (issue #695). Returns the number of idle conns
    /// closed; checked-out conns are exclusive to their session and close
    /// when it ends (the caller cancels those relays).
    pub fn reset(&self) -> usize {
        let mut items = self
            .items
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.generation.fetch_add(1, Ordering::SeqCst);
        let closed = items.len();
        items.clear();
        closed
    }

    /// Try to take a still-fresh idle entry off the pool. Returns `None` if
    /// the pool is empty or every entry has expired.
    pub fn take_idle(&self) -> Option<PoolStream> {
        let now = Instant::now();
        let mut items = self
            .items
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while let Some(entry) = items.pop() {
            if now < entry.expires_at {
                return Some(entry.conn);
            }
            // Expired — drop on the floor; the underlying TCP will close
            // when the Snell wrapper is dropped.
        }
        None
    }

    /// Number of idle entries currently parked (expired entries included —
    /// they are lazily discarded by [`Pool::take_idle`]). Exposed so callers
    /// (and integration tests) can observe when a completed session has
    /// replenished the pool.
    pub fn idle_count(&self) -> usize {
        self.items
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// Re-insert a conn that has just finished a session. Drops the conn if
    /// the pool is full or `generation` (captured when the conn was taken
    /// or dialled) predates a [`Pool::reset`].
    pub fn put(&self, conn: PoolStream, generation: Uint) {
        let mut items = self
            .items
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Checked under the lock `reset` bumps under: a put can never slip
        // a stale conn in after the reset's clear.
        if items.len() >= self.max_size || generation != self.generation() {
            return;
        }
        items.push(PooledEntry {
            conn,
            expires_at: Instant::now() + self.max_age,
        });
    }
}

impl Default for Pool {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[path = "../test/snell_pool_test.rs"]
mod tests;
