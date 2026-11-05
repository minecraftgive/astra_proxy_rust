use super::*;

#[test]
fn explicit_admission_limit_survives_context_and_relay_hops() {
    let error = MeowError::io_with("outer", local_resource_limit("pool full"));
    assert!(error.is_local_resource_error());
    assert!(!error.is_errno_backed());
    let nested = MeowError::RelayHopFailed {
        hop: 1,
        source: Box::new(error),
    };
    assert!(nested.is_local_resource_error());
    let io = nested.into_io_error("dialer");
    assert!(MeowError::io_with("adapter", io).is_local_resource_error());
    assert!(!MeowError::Io(io::ErrorKind::WouldBlock.into()).is_local_resource_error());
    #[cfg(unix)]
    assert!(!MeowError::Io(io::Error::from_raw_os_error(libc::EAGAIN)).is_local_resource_error());
}

#[test]
fn admission_limits_keep_context_error_precedence() {
    let limit = || MeowError::io_with("front", local_resource_limit("pool full"));
    let timeout = || MeowError::Io(io::ErrorKind::TimedOut.into());
    let chosen = MeowError::prefer_errno(Some(limit()), timeout()).unwrap();
    assert!(matches!(chosen, MeowError::Io(ref e) if e.kind() == io::ErrorKind::TimedOut));
    let chosen = MeowError::prefer_errno(Some(timeout()), limit()).unwrap();
    assert!(chosen.is_local_resource_error());
    assert!(!chosen.is_errno_backed());

    let chosen = MeowError::prefer_errno_io(
        Some(local_resource_limit("pool full")),
        io::ErrorKind::TimedOut.into(),
    )
    .unwrap();
    assert_eq!(chosen.kind(), io::ErrorKind::TimedOut);
    let chosen = MeowError::prefer_errno_io(
        Some(io::ErrorKind::TimedOut.into()),
        local_resource_limit("pool full"),
    )
    .unwrap();
    assert!(is_local_limit(&chosen));
    assert!(!MeowError::io_errno_backed(&chosen));
    #[cfg(unix)]
    {
        let chosen = MeowError::prefer_errno(
            Some(MeowError::Io(io::Error::from_raw_os_error(libc::EMFILE))),
            limit(),
        )
        .unwrap();
        assert!(matches!(chosen, MeowError::Io(ref e) if e.raw_os_error() == Some(libc::EMFILE)));
    }
}

#[test]
fn capability_errors_are_detected_at_any_hop_depth() {
    assert!(MeowError::NotSupported("udp".into()).is_capability_error());
    assert!(MeowError::UdpNotSupported.is_capability_error());
    let nested = MeowError::RelayHopFailed {
        hop: 2,
        source: Box::new(MeowError::NotSupported("udp".into())),
    };
    assert!(nested.is_capability_error());
    // io-boundary reconstitution: a front's refusal mapped to
    // `ErrorKind::Unsupported` and re-wrapped as `MeowError::Io`.
    let io = MeowError::Io(io::Error::new(
        io::ErrorKind::Unsupported,
        "dialer-proxy udp: refused",
    ));
    assert!(io.is_capability_error());
    assert!(!MeowError::Proxy("boom".into()).is_capability_error());
    assert!(!MeowError::Io(io::Error::other("x")).is_capability_error());
}

#[cfg(unix)]
#[test]
fn local_resource_errnos_are_detected_at_any_hop_depth() {
    let emfile = MeowError::Io(io::Error::from_raw_os_error(libc::EMFILE));
    assert!(emfile.is_local_resource_error());
    let nested = MeowError::RelayHopFailed {
        hop: 0,
        source: Box::new(MeowError::Io(io::Error::from_raw_os_error(libc::ENFILE))),
    };
    assert!(nested.is_local_resource_error());
    // Ordinary dial failures are NOT local resource errors.
    let refused = MeowError::Io(io::Error::from_raw_os_error(libc::ECONNREFUSED));
    assert!(!refused.is_local_resource_error());
    let timeout = MeowError::Io(io::Error::new(io::ErrorKind::TimedOut, "x"));
    assert!(!timeout.is_local_resource_error());
    assert!(!MeowError::Proxy("boom".into()).is_local_resource_error());
}

#[test]
fn into_io_error_preserves_the_capability_class() {
    let e = MeowError::RelayHopFailed {
        hop: 1,
        source: Box::new(MeowError::UdpNotSupported),
    };
    let io_err = e.into_io_error("dialer-proxy");
    assert_eq!(io_err.kind(), io::ErrorKind::Unsupported);
    assert!(io_err.to_string().contains("dialer-proxy"));
}

#[cfg(unix)]
#[test]
fn into_io_error_passes_errno_through_hop_wrappers() {
    let e = MeowError::RelayHopFailed {
        hop: 1,
        source: Box::new(MeowError::Io(io::Error::from_raw_os_error(libc::EMFILE))),
    };
    let io_err = e.into_io_error("dialer-proxy");
    assert_eq!(io_err.raw_os_error(), Some(libc::EMFILE));
}

#[test]
fn into_io_error_keeps_context_on_non_io_chains() {
    let e = MeowError::RelayHopFailed {
        hop: 3,
        source: Box::new(MeowError::Proxy("down".into())),
    };
    let io_err = e.into_io_error("dialer-proxy");
    assert_eq!(io_err.kind(), io::ErrorKind::Other);
    let msg = io_err.to_string();
    assert!(msg.contains("dialer-proxy"), "{msg}");
    assert!(msg.contains("hop 3"), "{msg}");
}

#[test]
fn synthesized_out_of_memory_counts_as_local_resource() {
    // `TryReserveError`-style failures arrive errno-less — the kind
    // arm still classifies them as local exhaustion on every platform.
    let e = MeowError::Io(io::Error::new(io::ErrorKind::OutOfMemory, "reserve"));
    assert!(e.is_local_resource_error());
}

#[test]
fn into_io_error_round_trips_capability_through_hop_wrappers() {
    // `RelayHopFailed{Io(Unsupported)}` — the io-boundary shape a
    // front's refusal takes when `dial_udp_conn` already mapped it —
    // must re-emit `Unsupported`, not collapse to `Other`.
    let e = MeowError::RelayHopFailed {
        hop: 2,
        source: Box::new(MeowError::Io(io::Error::new(
            io::ErrorKind::Unsupported,
            "front refused udp",
        ))),
    };
    assert_eq!(
        e.into_io_error("dialer-proxy").kind(),
        io::ErrorKind::Unsupported
    );
}

#[cfg(unix)]
#[test]
fn classification_survives_deep_relay_chains() {
    // Bounded in production by MAX_DIALER_CHAIN_DEPTH (16); exercise
    // depth 3 to pin the recursive walk.
    let mut e = MeowError::Io(io::Error::from_raw_os_error(libc::ENOBUFS));
    for hop in [2, 1, 0] {
        e = MeowError::RelayHopFailed {
            hop,
            source: Box::new(e),
        };
    }
    assert!(e.is_local_resource_error());
    assert_eq!(
        e.into_io_error("dialer-proxy").raw_os_error(),
        Some(libc::ENOBUFS)
    );

    let mut e = MeowError::NotSupported("udp".into());
    for hop in [2, 1, 0] {
        e = MeowError::RelayHopFailed {
            hop,
            source: Box::new(e),
        };
    }
    assert!(e.is_capability_error());
}

#[test]
fn io_with_keeps_errno_backed_errors_as_io() {
    let e = io::Error::from_raw_os_error(1); // EPERM — errno present, not
                                             // in the local-resource set.
    let wrapped = MeowError::io_with("ctx", e);
    match wrapped {
        MeowError::Io(e) => assert_eq!(e.raw_os_error(), Some(1)),
        other => panic!("expected Io, got {other}"),
    }
}

#[test]
fn io_with_keeps_the_capability_kind_as_io() {
    // Synthesized `Unsupported` (no errno) is still a capability
    // refusal — flattening to `Proxy` would let it dead-mark members.
    let e = io::Error::new(io::ErrorKind::Unsupported, "front: no udp");
    let wrapped = MeowError::io_with("ctx", e);
    match wrapped {
        MeowError::Io(e) => {
            assert_eq!(e.kind(), io::ErrorKind::Unsupported);
            assert!(MeowError::Io(e).is_capability_error());
        }
        other => panic!("expected Io, got {other}"),
    }
}

#[test]
fn io_with_stringifies_context_only_errors() {
    // Synthesized errors without errno or a capability kind keep the
    // historical `Proxy("{ctx}: {e}")` shape.
    let e = io::Error::other("handshake torn");
    let wrapped = MeowError::io_with("ctx", e);
    match wrapped {
        MeowError::Proxy(msg) => assert_eq!(msg, "ctx: handshake torn"),
        other => panic!("expected Proxy, got {other}"),
    }
}

#[test]
fn prefer_errno_keeps_errno_backed_over_context_only() {
    let emfile = || MeowError::RelayHopFailed {
        hop: 0,
        source: Box::new(MeowError::Io(io::Error::from_raw_os_error(1))),
    };
    let ctx = || MeowError::Proxy("session config".into());
    // errno prev survives a context-only next…
    let acc = MeowError::prefer_errno(Some(emfile()), ctx());
    assert!(acc.as_ref().is_some_and(MeowError::is_errno_backed));
    // …and errno next replaces a context-only prev.
    let acc = MeowError::prefer_errno(Some(ctx()), emfile());
    assert!(acc.as_ref().is_some_and(MeowError::is_errno_backed));
    // Two context-only errors: last wins.
    let acc = MeowError::prefer_errno(Some(ctx()), MeowError::Proxy("later".into()));
    assert!(matches!(acc, Some(MeowError::Proxy(ref s)) if s == "later"));
    // An `Io` without errno does not claim precedence.
    let acc = MeowError::prefer_errno(Some(MeowError::Io(io::Error::other("synth"))), ctx());
    assert!(matches!(acc, Some(MeowError::Proxy(_))));
}

#[test]
fn prefer_errno_io_keeps_errno_backed_over_context_only() {
    let errno = || io::Error::from_raw_os_error(1);
    let context = || io::Error::other("candidate failed");

    // errno prev survives a context-only next; errno next always
    // replaces; errno-less Io claims no precedence; OOM-kind io
    // counts as payload-bearing.
    let prev = MeowError::prefer_errno_io(Some(errno()), context());
    assert_eq!(prev.as_ref().and_then(io::Error::raw_os_error), Some(1));
    let prev = MeowError::prefer_errno_io(Some(context()), errno());
    assert_eq!(prev.as_ref().and_then(io::Error::raw_os_error), Some(1));
    let prev = MeowError::prefer_errno_io(None, context());
    assert_eq!(prev.as_ref().and_then(io::Error::raw_os_error), None);
    let oom = || io::Error::new(io::ErrorKind::OutOfMemory, "reserve");
    let prev = MeowError::prefer_errno_io(Some(oom()), context());
    assert_eq!(
        prev.as_ref().map(io::Error::kind),
        Some(io::ErrorKind::OutOfMemory)
    );
}

#[test]
fn io_with_passes_errno_less_out_of_memory_verbatim() {
    // Matches `is_local_resource_io_error`'s kind arm: a synthesized
    // OOM io error must not be flattened to `Proxy` at adapter
    // boundaries — `Io` is what the exemption reads.
    let e = MeowError::io_with("ctx", io::Error::new(io::ErrorKind::OutOfMemory, "reserve"));
    match e {
        MeowError::Io(ref inner) => assert_eq!(inner.kind(), io::ErrorKind::OutOfMemory),
        ref other => panic!("expected Io passthrough, got {other:?}"),
    }
    assert!(e.is_local_resource_error());
}

#[cfg(windows)]
#[test]
fn windows_resource_errnos_are_detected() {
    // WSAEMFILE / WSAENOBUFS / ERROR_NOT_ENOUGH_MEMORY /
    // ERROR_OUTOFMEMORY — socket and allocation failures surface as
    // raw os errors on Windows.
    for code in [10024, 10055, 8, 14] {
        let e = MeowError::Io(io::Error::from_raw_os_error(code));
        assert!(e.is_local_resource_error(), "errno {code}");
    }
    let refused = MeowError::Io(io::Error::from_raw_os_error(10061)); // WSAECONNREFUSED
    assert!(!refused.is_local_resource_error());
}
