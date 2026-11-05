use super::*;

#[test]
fn explicit_pool_pressure_never_escalates() {
    let tracker = DialFailureTracker::new();
    let error = MeowError::RelayHopFailed {
        hop: 1,
        source: Box::new(MeowError::io_with(
            "adapter",
            meow_common::error::local_resource_limit("bounded pool full"),
        )),
    };
    for _ in 0..100 {
        assert!(!tracker.on_failure(&error));
    }
    for _ in 1..DIAL_FAILURE_THRESHOLD {
        assert!(!tracker.on_failure(&proxy_err("timed out")));
    }
    assert!(tracker.on_failure(&proxy_err("timed out")));
}

fn proxy_err(msg: &str) -> MeowError {
    MeowError::Proxy(msg.into())
}

#[test]
fn five_failures_within_window_escalate() {
    let tracker = DialFailureTracker::new();
    for i in 1..DIAL_FAILURE_THRESHOLD {
        assert!(
            !tracker.on_failure(&proxy_err("dial timed out")),
            "failure {i} is below the threshold"
        );
    }
    assert!(
        tracker.on_failure(&proxy_err("dial timed out")),
        "the 5th failure within the window escalates"
    );
    // Escalation resets the streak, so a fresh window starts over.
    assert!(!tracker.on_failure(&proxy_err("dial timed out")));
}

#[test]
fn connection_refused_escalates_immediately() {
    let tracker = DialFailureTracker::new();
    assert!(tracker.on_failure(&proxy_err("connection refused")));
}

#[test]
fn connection_refused_leaves_the_streak_intact() {
    // mihomo's refused fast path does not touch failedTimes.
    let tracker = DialFailureTracker::new();
    for _ in 1..DIAL_FAILURE_THRESHOLD {
        tracker.on_failure(&proxy_err("dial timed out"));
    }
    assert!(tracker.on_failure(&proxy_err("connection refused")));
    assert!(
        tracker.on_failure(&proxy_err("dial timed out")),
        "the pre-refused streak still counts toward the threshold"
    );
}

#[test]
fn success_resets_the_streak() {
    let tracker = DialFailureTracker::new();
    for _ in 1..DIAL_FAILURE_THRESHOLD {
        tracker.on_failure(&proxy_err("dial timed out"));
    }
    tracker.on_success();
    for i in 1..DIAL_FAILURE_THRESHOLD {
        assert!(
            !tracker.on_failure(&proxy_err("dial timed out")),
            "failure {i} after a success does not escalate"
        );
    }
    assert!(tracker.on_failure(&proxy_err("dial timed out")));
}

#[test]
fn not_supported_failures_never_escalate() {
    let tracker = DialFailureTracker::new();
    for _ in 0..100 {
        assert!(!tracker.on_failure(&MeowError::NotSupported("udp".into())));
    }
}

#[test]
fn udp_unsupported_failures_never_escalate() {
    let tracker = DialFailureTracker::new();
    for _ in 0..100 {
        assert!(!tracker.on_failure(&MeowError::UdpNotSupported));
    }
}

#[test]
fn capability_errors_inside_relay_hops_never_escalate() {
    // A refusal from hop N of a chained front is the same permanent
    // property as a top-level NotSupported (issue #663).
    let tracker = DialFailureTracker::new();
    let err = MeowError::RelayHopFailed {
        hop: 2,
        source: Box::new(MeowError::NotSupported("udp".into())),
    };
    for _ in 0..100 {
        assert!(!tracker.on_failure(&err));
    }
    // io-boundary reconstitution reaches the tracker as
    // `MeowError::Io` with `Unsupported` kind.
    let io = MeowError::Io(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "dialer-proxy udp: refused",
    ));
    for _ in 0..100 {
        assert!(!tracker.on_failure(&io));
    }
}

#[cfg(unix)]
#[test]
fn local_resource_errors_never_escalate() {
    // EMFILE & friends mean this process is out of fds — dead-marking
    // a healthy member for local pressure blackholes the group
    // (issue #668).
    let tracker = DialFailureTracker::new();
    for errno in [libc::EMFILE, libc::ENFILE, libc::ENOBUFS, libc::ENOMEM] {
        let err = MeowError::Io(std::io::Error::from_raw_os_error(errno));
        for _ in 0..100 {
            assert!(
                !tracker.on_failure(&err),
                "errno {errno} must not count toward dead-marking"
            );
        }
    }
    // The same errno inside a relay-hop wrapper is equally local.
    let nested = MeowError::RelayHopFailed {
        hop: 1,
        source: Box::new(MeowError::Io(std::io::Error::from_raw_os_error(
            libc::EMFILE,
        ))),
    };
    for _ in 0..100 {
        assert!(!tracker.on_failure(&nested));
    }
    // ...while an unrelated errno still counts toward the threshold.
    let timed_out = MeowError::Io(std::io::Error::from_raw_os_error(libc::ETIMEDOUT));
    for _ in 1..DIAL_FAILURE_THRESHOLD {
        assert!(!tracker.on_failure(&timed_out));
    }
    assert!(tracker.on_failure(&timed_out));
}

#[test]
fn window_expiry_drops_the_streak() {
    let tracker = DialFailureTracker::new();
    for _ in 1..DIAL_FAILURE_THRESHOLD {
        tracker.on_failure(&proxy_err("dial timed out"));
    }
    // Age the window without sleeping in the test.
    tracker.state.lock().first_at =
        Some(Instant::now() - DIAL_FAILURE_WINDOW - Duration::from_millis(50));
    assert!(
        !tracker.on_failure(&proxy_err("dial timed out")),
        "a streak older than the window is dropped"
    );
    // A fresh window needs a full new streak to escalate.
    for _ in 1..DIAL_FAILURE_THRESHOLD {
        assert!(!tracker.on_failure(&proxy_err("dial timed out")));
    }
    assert!(tracker.on_failure(&proxy_err("dial timed out")));
}
