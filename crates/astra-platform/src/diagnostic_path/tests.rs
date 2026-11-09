use super::*;

#[tokio::test]
async fn no_physical_choice_and_loopback_never_become_automatic_routes() {
    assert!(PhysicalGuard::new("", false).await.is_err());
    assert!(PhysicalGuard::new("missing-interface-astra", false)
        .await
        .is_err());
    #[cfg(unix)]
    assert!(PhysicalGuard::new(
        if cfg!(target_os = "macos") {
            "lo0"
        } else {
            "lo"
        },
        false
    )
    .await
    .is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn bound_udp_is_unconnected_and_invalidated_guard_cannot_claim_route() {
    use std::ffi::CString;
    let name = if cfg!(target_os = "macos") {
        "lo0"
    } else {
        "lo"
    };
    let index = unsafe { libc::if_nametoindex(CString::new(name).unwrap().as_ptr()) };
    assert_ne!(index, 0);
    // Only a private test fixture bypasses physical admission to exercise native
    // source/device binding without sending traffic to an external interface.
    let guard = PhysicalGuard {
        interface: Interface {
            name: name.into(),
            index,
            addresses: vec!["127.0.0.1".parse().unwrap()],
            local_addresses: vec!["127.0.0.1".parse().unwrap()],
            fingerprint: "fixture".into(),
        },
        source: "127.0.0.1".parse().unwrap(),
        verified: Arc::new(Mutex::new(Verification {
            route: None,
            binding: true,
            sampled_at: now(),
        })),
    };
    let socket = match guard.bind_udp() {
        Ok(socket) => socket,
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return,
        Err(error) => panic!("{error}"),
    };
    assert_eq!(socket.local_addr().unwrap().ip(), guard.local_ip());
    assert!(socket.peer_addr().is_err());
    assert!(guard
        .recheck("192.0.2.1:3478".parse().unwrap())
        .await
        .is_err());
    assert!(!guard.evidence().route_verified);
    assert!(!guard.evidence().binding_verified);
    assert!(guard.destination("[::1]:3478".parse().unwrap()).is_err());
}

#[test]
fn local_echo_is_correlated_and_capability_is_runtime_permission_based() {
    for target in ["127.0.0.1", "::1"] {
        let target: IpAddr = target.parse().unwrap();
        let capability = probes::echo_capability(target.is_ipv6());
        if !capability.supported {
            assert!(capability.reason.is_some());
            continue;
        }
        let result = probes::echo(None, target, 71, std::time::Duration::from_secs(1));
        if let Err(error) = &result {
            if matches!(
                error.kind(),
                io::ErrorKind::PermissionDenied
                    | io::ErrorKind::NetworkUnreachable
                    | io::ErrorKind::AddrNotAvailable
            ) {
                continue;
            }
        }
        let reply = result.unwrap();
        #[cfg(windows)]
        if target.is_ipv4()
            && std::env::var("ASTRA_TEST_ICMP_V4_BLOCKED").as_deref() == Ok("1")
            && reply.outcome == astra_model::network_path::PathOutcome::NoResponse
        {
            // Only after independent system-ping confirmation by the test
            // operator; never turns a timeout into a successful echo result.
            assert!(reply.from.is_none() && reply.rtt_ms.is_none());
            eprintln!("Windows IPv4 echo reply not verified: independently confirmed system policy/no-response fixture");
            continue;
        }
        assert_eq!(reply.outcome, astra_model::network_path::PathOutcome::Reply);
        assert_eq!(reply.from, Some(target));
    }
}
