//! Sliding-window replay filter over a monotonically increasing packet-ID
//! space — the mechanism SIP022 §3.2.4 requires of every AEAD-2022 relay
//! session (the same construction WireGuard uses for its replay defense).
//!
//! The filter is keyed by *session*: both directions of a Shadowsocks 2022
//! UDP relay session carry their own packet-ID counter, so the inbound side
//! (server filtering client IDs) and the outbound side (client filtering
//! server IDs) each keep one window per session.

/// `u64` limbs backing the bitmap.
const WINDOW_LIMBS: usize = 32;
/// Trailing window width in packet IDs (2048). IDs older than
/// `max_seen - WINDOW_BITS` are rejected outright rather than recorded.
const WINDOW_BITS: u64 = WINDOW_LIMBS as u64 * 64;

/// Bitmap sliding window. Position `i` — counted back from `max_seen`, so
/// position 0 is the newest — maps to packet ID `max_seen - i`; position `i`
/// lives at `bits[i / 64] & (1 << (i % 64))`.
#[derive(Debug, Default)]
pub struct ReplayWindow {
    /// Largest packet ID accepted so far. A fresh window starts at 0 with an
    /// all-zero bitmap, so ID 0 is accepted exactly once like any other.
    max_seen: u64,
    bits: [u64; WINDOW_LIMBS],
}

impl ReplayWindow {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `true` — and records the ID — when `id` may be processed:
    /// it has not been seen before and is not older than the window.
    /// Replays and pre-window IDs return `false`.
    ///
    /// `u64::MAX` is always rejected: accepting it would pin `max_seen` at
    /// the top of the ID space, making every subsequent legitimate ID
    /// pre-window and deadening the session (the reference
    /// `PacketWindowFilter` rejects `id >= limit` with `u64::MAX` passed at
    /// every call site).
    pub fn check_and_set(&mut self, id: u64) -> bool {
        if id == u64::MAX {
            return false;
        }
        if id > self.max_seen {
            self.shift(id - self.max_seen);
            self.max_seen = id;
            self.bits[0] |= 1;
            return true;
        }
        let diff = self.max_seen - id;
        if diff >= WINDOW_BITS {
            return false;
        }
        let limb = &mut self.bits[(diff / 64) as usize];
        let mask = 1u64 << (diff % 64);
        if *limb & mask != 0 {
            return false;
        }
        *limb |= mask;
        true
    }

    /// Advance `max_seen` by `by`, moving every recorded bit `by` positions
    /// toward older offsets (higher limbs). Bits shifted past the window are
    /// forgotten.
    fn shift(&mut self, by: u64) {
        if by >= WINDOW_BITS {
            self.bits = [0; WINDOW_LIMBS];
            return;
        }
        let limbs = (by / 64) as usize;
        let rem = (by % 64) as u32;
        for i in (0..WINDOW_LIMBS).rev() {
            let mut v = if i >= limbs { self.bits[i - limbs] } else { 0 };
            if rem != 0 {
                v <<= rem;
                if i > limbs {
                    v |= self.bits[i - limbs - 1] >> (64 - rem);
                }
            }
            self.bits[i] = v;
        }
    }
}

#[cfg(test)]
#[path = "test/replay_window_test.rs"]
mod tests;
