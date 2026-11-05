use super::*;
use crate::dialer::ProxyRegistry;
use crate::group::test_support::MockProxy;
use meow_common::MeowError;
use smol_str::SmolStr;
use std::collections::HashMap;

fn meta(host: &str, port: u16) -> Metadata {
    Metadata {
        host: host.into(),
        dst_port: port,
        ..Default::default()
    }
}

/// Publish `dialer` in a registry of its own and hand back the target that
/// resolves it — the late-binding shape `meow-config` builds at load time.
/// The registry is returned alongside because `DialerTarget` only holds it
/// weakly: the caller must keep the handle alive for the target to resolve
/// (issue #533).
fn front(dialer: Arc<MockProxy>) -> (DialerTarget, ProxyRegistry) {
    let name = SmolStr::from(dialer.name());
    let registry = ProxyRegistry::default();
    let mut proxies: HashMap<SmolStr, Arc<dyn Proxy>> = HashMap::new();
    proxies.insert(name.clone(), dialer);
    registry.publish(Arc::new(proxies));
    (DialerTarget::new(name, &registry), registry)
}

#[tokio::test]
async fn dial_tcp_routes_through_dialer_first() {
    let dialer = MockProxy::new("fast");
    let inner = MockProxy::new("daniel");
    let (target, _registry) = front(Arc::clone(&dialer));
    let adapter = DialerProxyAdapter::new(Arc::clone(&inner) as _, target);

    // dialer.dial_tcp errors at relay hop 0 → confirms the chain dials the
    // front proxy first. The inner adapter is only reached via connect_over,
    // so its dial_tcp counter stays at zero.
    match adapter.dial_tcp(&meta("example.com", 443)).await {
        Err(MeowError::RelayHopFailed { hop: 0, .. }) => {}
        other => panic!("expected relay hop-0 failure, got {:?}", other.err()),
    }
    assert_eq!(dialer.dials(), 1, "front dialer must be dialled");
    assert_eq!(
        inner.dials(),
        0,
        "inner is reached via connect_over, not dial_tcp"
    );
}

/// A front hop that never resolves (registry not published, or the name
/// gone after a rebuild) must fail the dial. Falling back to a direct dial
/// would leak past a chain the user configured for policy reasons.
#[tokio::test]
async fn unresolved_front_hop_fails_loudly() {
    let inner = MockProxy::new("daniel");
    let adapter = DialerProxyAdapter::new(
        Arc::clone(&inner) as _,
        DialerTarget::new("ghost", &ProxyRegistry::default()),
    );

    match adapter.dial_tcp(&meta("example.com", 443)).await {
        Err(MeowError::Proxy(msg)) => {
            assert!(
                msg.contains("ghost"),
                "the error must name the missing dialer: {msg}"
            );
            // The temporary registry dropped at the end of the `new`
            // call — this is the dead-cell arm, not the live-registry
            // "name absent" arm (issue #533 review).
            assert!(
                msg.contains("generation dropped"),
                "expected the dead-cell wording, got: {msg}"
            );
        }
        other => panic!("expected a Proxy error, got {:?}", other.err()),
    }
    assert_eq!(inner.dials(), 0, "the inner outbound must not be dialled");
}

#[tokio::test]
async fn udp_is_unsupported() {
    let (target, _registry) = front(MockProxy::new_udp("fast"));
    let adapter = DialerProxyAdapter::new(MockProxy::new("inner"), target);
    assert!(!adapter.support_udp());
    match adapter.dial_udp(&meta("example.com", 53)).await {
        Err(MeowError::UdpNotSupported) => {}
        other => panic!("expected UdpNotSupported, got {:?}", other.err()),
    }
}

#[test]
fn identity_delegates_to_inner() {
    let inner = MockProxy::new("daniel");
    let (target, _registry) = front(MockProxy::new("fast"));
    let adapter = DialerProxyAdapter::new(inner, target);
    assert_eq!(adapter.name(), "daniel");
    assert_eq!(adapter.dialer_name(), "fast");
    assert_eq!(adapter.adapter_type(), AdapterType::Direct); // MockProxy's type
}

/// Issue #695: `member_proxies` exposes the inner outbound's members,
/// never the inner outbound itself, so the reachability walk only
/// reaches its pools through the wrapper's own forwarding. The front
/// hop is not reset through the wrapper: it is a route-table entry the
/// walk reaches on its own.
#[test]
fn reset_sessions_reaches_the_inner_outbound() {
    let inner = MockProxy::new("daniel");
    let fast = MockProxy::new("fast");
    let (target, _registry) = front(Arc::clone(&fast));
    let adapter: Arc<dyn Proxy> =
        Arc::new(DialerProxyAdapter::new(Arc::clone(&inner) as _, target));
    assert_eq!(meow_common::reset_sessions_reachable([adapter]), 1);
    assert_eq!(inner.resets(), 1);
    assert_eq!(fast.resets(), 0);
}
