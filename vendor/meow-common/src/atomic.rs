//! Platform-adaptive atomic type aliases.
//!
//! On targets with 64-bit atomics (x86_64, i686 Windows, aarch64, etc.)
//! these resolve to `AtomicU64`/`AtomicI64`. On targets lacking them
//! (e.g. MIPS32) they fall back to `AtomicU32`/`AtomicI32`.
//!
//! Trade-offs on 32-bit fallback targets:
//! - Traffic/byte counters wrap at 4 GiB (`u32::MAX`). REST/metrics totals
//!   are best-effort there; rates are unaffected.
//! - Millisecond clocks stored in these types wrap every ~49.7 days.
//!   Comparisons MUST be done in the truncated domain with `wrapping_sub`
//!   (see `UdpSession::idle_for`), never widened to `u64` first.

#[cfg(target_has_atomic = "64")]
pub type AtomicU = std::sync::atomic::AtomicU64;
#[cfg(not(target_has_atomic = "64"))]
pub type AtomicU = std::sync::atomic::AtomicU32;

#[cfg(target_has_atomic = "64")]
pub type AtomicI = std::sync::atomic::AtomicI64;
#[cfg(not(target_has_atomic = "64"))]
pub type AtomicI = std::sync::atomic::AtomicI32;

#[cfg(target_has_atomic = "64")]
pub type Uint = u64;
#[cfg(not(target_has_atomic = "64"))]
pub type Uint = u32;

#[cfg(target_has_atomic = "64")]
pub type Int = i64;
#[cfg(not(target_has_atomic = "64"))]
pub type Int = i32;

/// Atomically increments `counter` and returns the NEW value widened to the
/// wire's `u64` domain — `None` once the space is exhausted, leaving the
/// counter untouched. `Uint::MAX` is never returned: it is the replay
/// window's always-reject sentinel, so a terminal counter must not emit it.
///
/// SIP022 packet-ID allocators use this for pre-incremented IDs (the first
/// datagram carries 1). On 64-bit targets `AtomicU` is u64 and exhaustion is
/// unreachable; on 32-bit fallback targets it is u32 and `None` is the
/// end-of-session signal (the caller re-keys or re-dials) rather than
/// wrapping onto IDs the peer's window has already spent.
#[inline]
pub fn checked_increment(counter: &AtomicU) -> Option<u64> {
    let mut current = counter.load(std::sync::atomic::Ordering::Relaxed);
    loop {
        let next = current.checked_add(1).filter(|&n| n != Uint::MAX)?;
        // Keep compatibility with Rust 1.89; `try_update` requires Rust 1.95.
        match counter.compare_exchange_weak(
            current,
            next,
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
        ) {
            Ok(_) => {
                #[allow(
                    clippy::useless_conversion,
                    reason = "identity on 64-bit; u32→u64 widening on mips32"
                )]
                return Some(u64::from(next));
            }
            Err(actual) => current = actual,
        }
    }
}

#[cfg(test)]
#[path = "test/atomic_test.rs"]
mod tests;
