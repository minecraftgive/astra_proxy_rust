use super::*;
use tokio::time::Instant;

/// Stand-in for a socket-level failure (EMFILE/ENFILE/ENOMEM/ENOBUFS…):
/// any kind outside the per-connection set engages the delay.
fn socket_error() -> io::Error {
    io::Error::other("socket-level failure")
}

/// Errnos from the Linux `accept(2)` pending-error set that either
/// have no `ErrorKind` name (decode to `Uncategorized`) or decode to
/// a wider kind than intended (`EOPNOTSUPP` → `Unsupported` also
/// covers `ENOSYS`). `ENONET` exists only on Linux-kernel targets.
#[cfg(unix)]
const PENDING_ERRNOS: &[i32] = &[
    libc::EPROTO,
    libc::ENOPROTOOPT,
    libc::EHOSTDOWN,
    libc::EOPNOTSUPP,
    #[cfg(any(target_os = "linux", target_os = "android"))]
    libc::ENONET,
];

#[tokio::test(start_paused = true)]
async fn delay_doubles_then_caps() {
    // Pin the numeric contract, not just the shape: the documented
    // 10 ms → 1 s curve is the behavior callers rely on.
    assert_eq!(INITIAL_DELAY, Duration::from_millis(10));
    assert_eq!(MAX_DELAY, Duration::from_secs(1));

    let mut backoff = ErrorBackoff::new();
    let start = Instant::now();

    backoff.failed(&socket_error()).await;
    assert_eq!(start.elapsed(), INITIAL_DELAY);
    backoff.failed(&socket_error()).await;
    // Cumulative: the second failure waited INITIAL_DELAY * 2 on top of
    // the first INITIAL_DELAY.
    assert_eq!(start.elapsed(), INITIAL_DELAY + INITIAL_DELAY * 2);

    // Drive to the cap, then confirm it stays there.
    for _ in 0..16 {
        backoff.failed(&socket_error()).await;
    }
    let at_cap = start.elapsed();
    backoff.failed(&socket_error()).await;
    assert_eq!(start.elapsed() - at_cap, MAX_DELAY);
}

#[tokio::test(start_paused = true)]
async fn success_resets_to_initial() {
    let mut backoff = ErrorBackoff::new();
    for _ in 0..16 {
        backoff.failed(&socket_error()).await;
    }
    backoff.succeeded();
    let start = Instant::now();
    backoff.failed(&socket_error()).await;
    assert_eq!(start.elapsed(), INITIAL_DELAY);
}

/// An aborted pending connection consumed a backlog slot — the accept is
/// making progress and must not be delayed (RST-after-handshake scan
/// floods would otherwise throttle healthy accepts toward 1/s). The
/// Linux `accept(2)` pending-network-error set is progress too.
#[tokio::test(start_paused = true)]
async fn per_connection_errors_never_sleep() {
    let mut backoff = ErrorBackoff::new();
    let start = Instant::now();
    for kind in [
        io::ErrorKind::ConnectionAborted,
        io::ErrorKind::ConnectionReset,
        io::ErrorKind::ConnectionRefused,
        io::ErrorKind::Interrupted,
        io::ErrorKind::NetworkUnreachable,
        io::ErrorKind::HostUnreachable,
        // unix-only exemption: the same `ErrorKind` is a persistent
        // socket-level failure on Windows (`WSAENETDOWN`).
        #[cfg(unix)]
        io::ErrorKind::NetworkDown,
    ] {
        backoff.failed(&io::Error::from(kind)).await;
    }
    // The pending-error errnos with no `ErrorKind` name (decode to
    // `Uncategorized`, so `from` alone cannot pin them).
    #[cfg(unix)]
    for errno in PENDING_ERRNOS {
        backoff.failed(&io::Error::from_raw_os_error(*errno)).await;
    }
    #[cfg(windows)]
    for errno in [10052, 10040, 10064] {
        backoff.failed(&io::Error::from_raw_os_error(errno)).await;
    }
    assert_eq!(start.elapsed(), Duration::ZERO);
    // And they must not grow the delay: the next socket-level error
    // still waits only INITIAL_DELAY.
    backoff.failed(&socket_error()).await;
    assert_eq!(start.elapsed(), INITIAL_DELAY);
}

/// Progress errors must not reset an already-raised delay either: an
/// embryonic dequeue interleaved in an EMFILE storm does not prove the
/// fd pressure cleared — only a successful accept/recv does.
#[tokio::test(start_paused = true)]
async fn per_connection_errors_do_not_reset() {
    let mut backoff = ErrorBackoff::new();
    for _ in 0..4 {
        backoff.failed(&socket_error()).await;
    }
    assert!(
        !backoff
            .failed(&io::Error::from(io::ErrorKind::ConnectionAborted))
            .await
    );
    let start = Instant::now();
    assert!(backoff.failed(&socket_error()).await);
    assert_eq!(start.elapsed(), INITIAL_DELAY * 16);
}

/// Deliberate non-exemptions still sleep: a persistent deny-all
/// (`EPERM`/`WSAEACCES`) must not spin; a kind-only `Unsupported`
/// (the `ENOSYS` class — absent syscall under seccomp/sandboxed
/// runtimes) is persistent, not the pending `EOPNOTSUPP`; and on
/// Windows `WSAENETDOWN` reports a dead network subsystem, not a
/// consumed queue entry.
#[tokio::test(start_paused = true)]
async fn persistent_kinds_still_sleep() {
    let mut backoff = ErrorBackoff::new();
    let start = Instant::now();
    // 10 ms, delay → 20 ms.
    assert!(
        backoff
            .failed(&io::Error::from(io::ErrorKind::PermissionDenied))
            .await
    );
    // 20 ms, delay → 40 ms.
    assert!(
        backoff
            .failed(&io::Error::from(io::ErrorKind::Unsupported))
            .await
    );
    #[cfg(windows)]
    {
        // 40 ms, delay → 80 ms.
        assert!(
            backoff
                .failed(&io::Error::from(io::ErrorKind::NetworkDown))
                .await
        );
        assert_eq!(start.elapsed(), INITIAL_DELAY * 7);
    }
    #[cfg(not(windows))]
    assert_eq!(start.elapsed(), INITIAL_DELAY * 3);
}
