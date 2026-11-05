use super::*;
use crate::group::test_support::MockProxy;
use meow_common::Metadata;

#[test]
fn picks_first_when_all_alive() {
    let g = FallbackGroup::new("fb", vec![MockProxy::new("a"), MockProxy::new("b")]);
    assert_eq!(g.first_alive().unwrap().name(), "a");
}

#[test]
fn skips_dead_to_next_alive() {
    let a = MockProxy::new("a");
    a.set_alive(false);
    let g = FallbackGroup::new("fb", vec![a, MockProxy::new("b"), MockProxy::new("c")]);
    assert_eq!(g.first_alive().unwrap().name(), "b");
}

#[test]
fn all_dead_returns_first_proxy_as_last_resort() {
    // Upstream behaviour: when every member is dead, still return *something*
    // (the first proxy) so the caller can attempt the dial and surface a
    // real network error rather than a "no proxy" config error.
    let a = MockProxy::new("a");
    let b = MockProxy::new("b");
    a.set_alive(false);
    b.set_alive(false);
    let g = FallbackGroup::new("fb", vec![a, b]);
    assert_eq!(g.first_alive().unwrap().name(), "a");
}

#[test]
fn recovery_promotes_revived_member_back_to_head() {
    let a = MockProxy::new("a");
    a.set_alive(false);
    let a_ref = Arc::clone(&a);
    let g = FallbackGroup::new("fb", vec![a, MockProxy::new("b")]);
    assert_eq!(g.first_alive().unwrap().name(), "b");
    a_ref.set_alive(true);
    assert_eq!(
        g.first_alive().unwrap().name(),
        "a",
        "head proxy regaining health must reclaim primary slot"
    );
}

#[test]
fn member_names_preserve_declaration_order() {
    let g = FallbackGroup::new(
        "fb",
        vec![
            MockProxy::new("a"),
            MockProxy::new("b"),
            MockProxy::new("c"),
        ],
    );
    assert_eq!(g.member_names(), vec!["a", "b", "c"]);
}

/// `member_proxies()` must cover provider-slot members (issue #543
/// item 1) and list them in `member_names()` order.
#[test]
fn member_proxies_include_provider_slots_in_member_names_order() {
    let slot: ProviderSlot = Arc::new(RwLock::new(vec![
        MockProxy::new("p1") as Arc<dyn Proxy>,
        MockProxy::new("p2") as Arc<dyn Proxy>,
    ]));
    let g = FallbackGroup::new_with_providers(
        "fb",
        vec![MockProxy::new("a"), MockProxy::new("b")],
        vec![slot],
    );
    let names: Vec<String> = g
        .member_proxies()
        .expect("groups expose members")
        .iter()
        .map(|p| p.name().to_string())
        .collect();
    assert_eq!(names, g.member_names());
    assert_eq!(names, vec!["a", "b", "p1", "p2"]);
}

#[tokio::test]
async fn dial_tcp_routes_through_first_alive() {
    let a = MockProxy::new("a");
    let b = MockProxy::new("b");
    a.set_alive(false);
    let a_ref = Arc::clone(&a);
    let b_ref = Arc::clone(&b);
    let g = FallbackGroup::new("fb", vec![a, b]);
    assert_eq!(g.usage_generation(), 0, "unused group has no use");
    let _ = g.dial_tcp(&Metadata::default()).await;
    assert_eq!(g.usage_generation(), 1, "dial records group use");
    assert_eq!(a_ref.dials(), 0);
    assert_eq!(b_ref.dials(), 1);
}

#[tokio::test]
async fn health_probe_dials_do_not_count_as_use() {
    // Lazy health checks dial members with `ConnType::Tunnel`.  If those
    // dials incremented the usage generation, a parent group probing a
    // lazy child would keep the child's own probe loop awake forever.
    let g = FallbackGroup::new("fb", vec![MockProxy::new("a")]);
    let probe_meta = Metadata {
        conn_type: meow_common::ConnType::Tunnel,
        ..Default::default()
    };
    let _ = g.dial_tcp(&probe_meta).await;
    assert_eq!(
        g.usage_generation(),
        0,
        "probe dials must not mark the group as used"
    );
    // The `internal` flag (provider fetches, probes chained through
    // `dialer-proxy`) is the same housekeeping signal — also skipped
    // (issue #555).
    let internal_meta = Metadata {
        conn_type: meow_common::ConnType::Inner,
        internal: true,
        ..Default::default()
    };
    let _ = g.dial_tcp(&internal_meta).await;
    assert_eq!(
        g.usage_generation(),
        0,
        "internal dials must not mark the group as used"
    );
    let _ = g.dial_tcp(&Metadata::default()).await;
    assert_eq!(g.usage_generation(), 1, "real traffic still marks use");
}

/// Issue #533: `unwrap_proxy(meta, false)` is the match-time peek —
/// it must not record usage; `touch=true` (the real dial path) does.
#[test]
fn unwrap_peek_does_not_touch_usage() {
    let g = FallbackGroup::new("fb", vec![MockProxy::new("a"), MockProxy::new("b")]);
    let meta = Metadata::default();
    let peeked = g
        .unwrap_proxy(&meta, false)
        .expect("peek yields the first alive member");
    assert_eq!(peeked.name(), "a", "peek must return the pick target");
    assert_eq!(
        g.usage_generation(),
        0,
        "peek must not mark the group as used"
    );
    let _ = g.unwrap_proxy(&meta, true);
    assert_eq!(g.usage_generation(), 1, "real unwrap records the use");
}

/// Issue #681 wedge repro: `fallback: [selector, spare]` where the
/// selector's pick always fails. Escalation used to dead-mark the
/// selector's own (unread) health bit, so `alive()` stayed delegated-
/// true and every dial re-picked the broken member forever.
#[tokio::test]
async fn dead_marked_selector_member_is_skipped() {
    let leaf = MockProxy::new("leaf"); // dial always errors
    let sel: Arc<dyn Proxy> = Arc::new(crate::group::selector::SelectorGroup::new(
        "sel",
        vec![leaf],
    ));
    let spare = MockProxy::new("spare");
    let spare_ref = Arc::clone(&spare);
    let g = FallbackGroup::new("fb", vec![sel, spare]);

    // Five dial failures inside the window escalate (each `dial_tcp`
    // fails instantly, so the streak stays inside 5s).
    for _ in 0..5 {
        let _ = g.dial_tcp(&Metadata::default()).await;
    }
    assert_eq!(
        spare_ref.dials(),
        0,
        "before escalation the first member is picked"
    );
    let _ = g.dial_tcp(&Metadata::default()).await;
    assert_eq!(
        spare_ref.dials(),
        1,
        "dead-marked selector member must be skipped"
    );
}

#[test]
fn support_udp_reflects_first_alive() {
    let a = MockProxy::new("a"); // tcp-only
    let a_ref = Arc::clone(&a);
    let g = FallbackGroup::new("fb", vec![a, MockProxy::new_udp("b")]);
    assert!(!g.support_udp(), "a is alive and tcp-only");
    a_ref.set_alive(false);
    assert!(g.support_udp(), "fallback to udp-capable b");
}

#[tokio::test]
async fn user_pin_overrides_order_and_can_be_cleared() {
    let g = FallbackGroup::new("fb", vec![MockProxy::new("a"), MockProxy::new("b")]);
    ProxySelection::set(&g, "b").await.unwrap();
    assert_eq!(g.first_alive().unwrap().name(), "b");
    assert_eq!(ProxySelection::fixed(&g).as_deref(), Some("b"));

    ProxySelection::force_set(&g, None);
    assert_eq!(g.first_alive().unwrap().name(), "a");
    assert_eq!(ProxySelection::fixed(&g).as_deref(), Some(""));
}

#[tokio::test]
async fn dead_fallback_pin_is_forgotten_in_memory() {
    let a = MockProxy::new("a");
    let b = MockProxy::new("b");
    let b_ref = Arc::clone(&b);
    let g = FallbackGroup::new("fb", vec![a, b]);
    ProxySelection::set(&g, "b").await.unwrap();
    b_ref.set_alive(false);
    assert_eq!(g.first_alive().unwrap().name(), "a");
    assert_eq!(ProxySelection::fixed(&g).as_deref(), Some(""));
}

#[tokio::test]
async fn repeated_dial_failures_mark_member_dead() {
    // mihomo GroupBase.onDialFailed: five failures within the window
    // escalate; the escalation marks the failed member dead so routing
    // skips it until the next probe revives it.
    let a = MockProxy::new_failing("a", AdapterType::Shadowsocks, "dial timed out");
    let b = MockProxy::new_failing("b", AdapterType::Shadowsocks, "dial timed out");
    let a_ref = Arc::clone(&a);
    let b_ref = Arc::clone(&b);
    let g = FallbackGroup::new("fb", vec![a, b]);

    for i in 1..5 {
        let _ = g.dial_tcp(&Metadata::default()).await;
        assert!(
            a_ref.alive(),
            "failure {i} is below the escalation threshold"
        );
    }
    let _ = g.dial_tcp(&Metadata::default()).await;
    assert!(
        !a_ref.alive(),
        "the repeatedly failing member is marked dead"
    );
    assert!(b_ref.alive(), "the untouched member stays alive");

    // The dead member is skipped: the next dial reaches b instead.
    let _ = g.dial_tcp(&Metadata::default()).await;
    assert_eq!(b_ref.dials(), 1, "routing moved past the dead member");
    assert_eq!(a_ref.dials(), 5);
}

#[tokio::test]
async fn internal_dial_failures_still_mark_member_dead() {
    // `touch_user_traffic` skips internal dials, but
    // `record_dial_failure` deliberately does NOT (#555): a failed
    // housekeeping dial is real evidence the member is down — the
    // asymmetry must hold or housekeeping could mask a dead member.
    let a = MockProxy::new_failing("a", AdapterType::Shadowsocks, "dial timed out");
    let a_ref = Arc::clone(&a);
    let g = FallbackGroup::new("fb", vec![a, MockProxy::new("b")]);
    let internal_meta = Metadata {
        internal: true,
        ..Default::default()
    };

    for _ in 0..5 {
        let _ = g.dial_tcp(&internal_meta).await;
    }
    assert!(
        !a_ref.alive(),
        "internal dial failures must still escalate to dead"
    );
    // …and the usage gate stays untouched by the same dials.
    assert_eq!(g.usage_generation(), 0);
}

#[tokio::test]
async fn connection_refused_marks_member_dead_immediately() {
    // mihomo escalates "connection refused" without waiting for the
    // failure streak.
    let a = MockProxy::new_failing("a", AdapterType::Shadowsocks, "connection refused");
    let a_ref = Arc::clone(&a);
    let g = FallbackGroup::new("fb", vec![a, MockProxy::new("b")]);

    let _ = g.dial_tcp(&Metadata::default()).await;
    assert!(!a_ref.alive(), "refused escalates on the first failure");
}

#[tokio::test]
async fn direct_member_failures_are_exempt_from_escalation() {
    // mihomo exempts Direct/Reject-family members: their dial errors
    // describe the target, not the member.
    let a = MockProxy::new_failing("d", AdapterType::Direct, "connection refused");
    let a_ref = Arc::clone(&a);
    let g = FallbackGroup::new("fb", vec![a]);

    for _ in 0..10 {
        let _ = g.dial_tcp(&Metadata::default()).await;
    }
    assert!(
        a_ref.alive(),
        "direct members are exempt from failure tracking"
    );
}

#[tokio::test]
async fn udp_unsupported_member_failures_are_exempt_from_escalation() {
    // mihomo ignores ErrNotSupport failures; meow surfaces them as
    // MeowError::UdpNotSupported (relay chains, dialer proxies). They
    // describe a capability, not health, so UDP traffic must never mark
    // an otherwise healthy member dead.
    let a = MockProxy::new_udp_unsupported("r", AdapterType::Relay);
    let a_ref = Arc::clone(&a);
    let g = FallbackGroup::new("fb", vec![a]);

    for _ in 0..10 {
        let _ = g.dial_udp(&Metadata::default()).await;
    }
    assert!(
        a_ref.alive(),
        "structural UdpNotSupported errors are exempt from failure tracking"
    );
}
