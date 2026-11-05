//! Exponential backoff for loops that retry a fallible syscall.
//!
//! Accept/recv loops (`TcpListener::accept`, UDP `recv_from`) must not tear
//! down on a transient `EMFILE`/`ECONNABORTED`, but an unconditional `continue`
//! spins hot when the failure is persistent — fd exhaustion makes `accept`
//! return `Err` immediately forever, burning a worker and flooding the log.
//! [`ErrorBackoff`] delays retries exponentially (10 ms → doubling → 1 s cap)
//! and resets on the first success, so a recovered listener pays nothing and a
//! wedged one retries at most once per second.
//!
//! Per-connection errors are exempt: tokio surfaces `accept()` failures for
//! pending connections that were reset before the accept (`ECONNABORTED`),
//! the pending-network-error set Linux passes up through `accept(2)`
//! (`ENETUNREACH`, `EHOSTUNREACH`, `ENETDOWN`, `EOPNOTSUPP`, `EPROTO`,
//! `ENOPROTOOPT`, `EHOSTDOWN`, `ENONET` — the man page says treat them like
//! `EAGAIN`), and ICMP async errors on UDP recv (`ECONNREFUSED` on connected
//! sockets, `WSAECONNRESET`/`WSAEHOSTUNREACH`/`WSAENETUNREACH`/`WSAENETRESET`/
//! `WSAEHOSTDOWN`/`WSAEMSGSIZE` on Windows). Each such error consumed a
//! queue entry — the call made forward progress — so sleeping would only
//! add latency under an abortive connect or ICMP flood while the socket
//! itself stays healthy. Only socket-level failures (`EMFILE`, `ENFILE`,
//! `ENOMEM`, `ENOBUFS`, …) engage the delay.

use std::io;
use std::time::Duration;

/// Delay after the first consecutive failure. Small enough that a
/// single transient error costs ~10 ms of accept throughput.
const INITIAL_DELAY: Duration = Duration::from_millis(10);
/// Ceiling on consecutive-failure delay: one retry per second, which also
/// rate-limits the per-error log line under a persistent failure.
const MAX_DELAY: Duration = Duration::from_secs(1);

/// `true` when `e` is a per-connection/per-packet error rather than a
/// socket-level failure. Each dequeues a pending queue entry — forward
/// progress — so the retry loop must not sleep on them; `Interrupted` is
/// exempt on the weaker grounds that retrying a signal interruption is free
/// (mio does not auto-retry `EINTR`). Notable non-exemptions that sleep:
/// `EPERM`/`EACCES` (`PermissionDenied`) — a persistent deny-all
/// netfilter/LSM rule must not spin; Darwin's `EPROTOTYPE` —
/// its reports straddle per-conn teardown and persistent AppProxy
/// interception; and the man page's secondary kernel list
/// (`ENOSR`/`ESOCKTNOSUPPORT`/`EPROTONOSUPPORT`/`ETIMEDOUT`) — rare and
/// unverifiable as progress, so they take the safe direction.
fn is_per_connection(e: &io::Error) -> bool {
    match e.kind() {
        io::ErrorKind::ConnectionAborted
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::ConnectionRefused
        | io::ErrorKind::Interrupted
        // Part of the `accept(2)` pending set on unix *and* the async-ICMP
        // family Windows reports on unconnected UDP `recvfrom`
        // (`WSAENETUNREACH`/`WSAEHOSTUNREACH`) — per-packet progress on
        // every supported platform.
        | io::ErrorKind::NetworkUnreachable
        | io::ErrorKind::HostUnreachable => true,
        // `ENETDOWN` is the only errno that decodes to `NetworkDown`, and
        // on unix it is only ever a pending accept error — while Windows
        // `WSAENETDOWN` means the network subsystem itself failed, so the
        // kind must sleep there. `EOPNOTSUPP` is handled in the errno arm
        // instead of here: its `Unsupported` kind also covers `ENOSYS`,
        // a persistent failure (seccomp/sandboxed runtimes) that must
        // keep sleeping.
        #[cfg(unix)]
        io::ErrorKind::NetworkDown => true,
        _ => is_pending_errno(e),
    }
}

/// `accept(2)` pending-error errnos that Rust maps to `Uncategorized`
/// (`EPROTO`/`ENOPROTOOPT`/`EHOSTDOWN`/`ENONET`) or to a wider-than-
/// intended `ErrorKind` (`EOPNOTSUPP` → `Unsupported`, which also covers
/// the persistent `ENOSYS`). Linux-only `ENONET` needs its own `cfg` —
/// the libc constant does not exist on other unix targets.
#[cfg(unix)]
fn is_pending_errno(e: &io::Error) -> bool {
    match e.raw_os_error() {
        Some(libc::EPROTO | libc::ENOPROTOOPT | libc::EHOSTDOWN | libc::EOPNOTSUPP) => true,
        #[cfg(any(target_os = "linux", target_os = "android"))]
        Some(libc::ENONET) => true,
        _ => false,
    }
}

/// ICMP async errors Windows reports on an unconnected UDP `recvfrom`,
/// decoded to `Uncategorized`: `WSAENETRESET` (10052) and
/// `WSAEHOSTDOWN` (10064) mark a pending per-datagram error, and
/// `WSAEMSGSIZE` (10040) means the datagram was consumed but truncated.
#[cfg(windows)]
fn is_pending_errno(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(10052 | 10040 | 10064))
}

#[cfg(not(any(unix, windows)))]
fn is_pending_errno(_: &io::Error) -> bool {
    false
}

/// Exponential backoff state for a retry loop. Call [`Self::failed`] after
/// each error and [`Self::succeeded`] after each success.
#[derive(Debug)]
pub struct ErrorBackoff {
    delay: Duration,
}

impl ErrorBackoff {
    pub const fn new() -> Self {
        Self {
            delay: INITIAL_DELAY,
        }
    }

    /// Sleep for the current delay, then double it (capped at `MAX_DELAY` —
    /// 1 s); returns `true` when the delay actually engaged. Call this when
    /// the retried operation failed. Per-connection errors (`ECONNABORTED`,
    /// `ECONNREFUSED`, `ENETUNREACH`, …) return `false` immediately without
    /// touching `delay`: they are queue progress, not socket failure —
    /// progress neither proves the socket healed (no reset) nor deepens the
    /// penalty (no growth). The return value lets a call site log loudly
    /// only for socket-level failures, keeping per-packet noise at `debug!`.
    pub async fn failed(&mut self, err: &io::Error) -> bool {
        if is_per_connection(err) {
            return false;
        }
        tokio::time::sleep(self.delay).await;
        self.delay = (self.delay * 2).min(MAX_DELAY);
        true
    }

    /// Reset to `INITIAL_DELAY`. Call this when the retried operation
    /// succeeded.
    pub fn succeeded(&mut self) {
        self.delay = INITIAL_DELAY;
    }
}

impl Default for ErrorBackoff {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[path = "test/backoff_test.rs"]
mod tests;
