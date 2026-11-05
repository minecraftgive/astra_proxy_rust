use super::*;
use crate::group::test_support::MockProxy;
use meow_common::Metadata;

/// `member_proxies()` must cover provider-slot members (issue #543
/// item 1) and list them in `member_names()` order.
#[test]
fn member_proxies_include_provider_slots_in_member_names_order() {
    let slot: ProviderSlot = Arc::new(RwLock::new(vec![
        MockProxy::new("p1") as Arc<dyn Proxy>,
        MockProxy::new("p2") as Arc<dyn Proxy>,
    ]));
    let g = UrlTestGroup::new_with_providers(
        "auto",
        vec![MockProxy::new("a"), MockProxy::new("b")],
        150,
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

/// `unwrap_proxy(meta, touch)` — upstream `Unwrap`: the peek must not
/// record usage (lazy groups stay asleep on match probes) but must
/// still return the same member the next dial would pick.
#[test]
fn unwrap_peek_returns_pick_without_touching_usage() {
    let g = UrlTestGroup::new("auto", vec![MockProxy::new("a"), MockProxy::new("b")], 150);
    let meta = Metadata::default();
    let peeked = g.unwrap_proxy(&meta, false).expect("peek yields a member");
    assert_eq!(
        peeked.name(),
        g.pick_for_dial().unwrap().name(),
        "peek must agree with the next pick"
    );
    assert_eq!(g.usage_generation(), 0, "touch=false must not record usage");
    let _ = g.unwrap_proxy(&meta, true);
    assert_eq!(g.usage_generation(), 1, "touch=true records the use");
}

fn pick(g: &UrlTestGroup) -> String {
    g.pick_for_dial().unwrap().name().to_string()
}

#[test]
fn first_pick_chooses_lowest_delay_among_alive() {
    let a = MockProxy::new("a");
    let b = MockProxy::new("b");
    let c = MockProxy::new("c");
    a.set_delay(120);
    b.set_delay(30);
    c.set_delay(60);
    let g = UrlTestGroup::new("ut", vec![a, b, c], 0);
    assert_eq!(pick(&g), "b");
}

#[test]
fn tolerance_keeps_current_pick_until_strictly_better() {
    // tolerance = 50: once an incumbent exists, a challenger must beat
    // it by MORE than 50 ms before the selection flips.
    let a = MockProxy::new("a");
    let b = MockProxy::new("b");
    a.set_delay(100);
    b.set_delay(80);
    let a_ref = Arc::clone(&a);
    let g = UrlTestGroup::new("ut", vec![a, b], 50);
    // First pick has no incumbent → goes to the lowest-delay member.
    assert_eq!(pick(&g), "b");

    // a comes in just inside the tolerance band — must stick with b.
    a_ref.set_delay(40);
    assert_eq!(pick(&g), "b", "tolerance prevents flapping");

    // a improves enough to clear the band → must promote.
    a_ref.set_delay(20);
    assert_eq!(pick(&g), "a");
}

#[test]
fn dead_current_forces_repick_even_inside_tolerance() {
    let a = MockProxy::new("a");
    let b = MockProxy::new("b");
    a.set_delay(50);
    b.set_delay(80);
    let a_ref = Arc::clone(&a);
    let g = UrlTestGroup::new("ut", vec![a, b], 100);
    assert_eq!(pick(&g), "a");
    a_ref.set_alive(false);
    assert_eq!(pick(&g), "b", "current died -> must promote next best");
}

#[test]
fn no_alive_members_returns_first_proxy_as_fallback() {
    let a = MockProxy::new("a");
    let b = MockProxy::new("b");
    a.set_alive(false);
    b.set_alive(false);
    let g = UrlTestGroup::new("ut", vec![a, b], 0);
    assert_eq!(
        g.pick_for_dial().unwrap().name(),
        "a",
        "graceful degradation: surface a real network error from a, \
             not a 'no proxy' config error"
    );
}

#[test]
fn zero_delay_is_treated_as_unknown_not_best() {
    // last_delay == 0 means "never probed / dead"; the picker must NOT
    // consider it the lowest delay.
    let a = MockProxy::new("a");
    let b = MockProxy::new("b");
    // a has no recorded delay (last_delay=0), b has 100.
    b.set_delay(100);
    let g = UrlTestGroup::new("ut", vec![a, b], 0);
    assert_eq!(pick(&g), "b", "0-delay proxy must not win");
}

#[tokio::test]
async fn dial_tcp_routes_through_pick() {
    let a = MockProxy::new("a");
    let b = MockProxy::new("b");
    a.set_delay(100);
    b.set_delay(20);
    let a_ref = Arc::clone(&a);
    let b_ref = Arc::clone(&b);
    let g = UrlTestGroup::new("ut", vec![a, b], 0);
    assert_eq!(g.usage_generation(), 0, "unused group has no use");
    let _ = g.dial_tcp(&Metadata::default()).await;
    assert_eq!(g.usage_generation(), 1, "dial records group use");
    assert_eq!(a_ref.dials(), 0);
    assert_eq!(b_ref.dials(), 1);
}

#[tokio::test]
async fn internal_dial_does_not_count_as_use() {
    // Housekeeping traffic (provider fetches, probes chained through
    // `dialer-proxy`) must not bump the usage generation — otherwise a
    // `lazy` group is probed every interval forever, degrading lazy to
    // eager.
    let a = MockProxy::new("a");
    a.set_delay(20);
    let g = UrlTestGroup::new("ut", vec![a], 0);

    let meta = Metadata {
        conn_type: meow_common::ConnType::Inner,
        internal: true,
        ..Metadata::default()
    };
    let _ = g.dial_tcp(&meta).await;
    assert_eq!(
        g.usage_generation(),
        0,
        "internal dial must not record group use"
    );

    // The legacy `ConnType::Tunnel` probe marker is still skipped.
    let meta = Metadata {
        conn_type: meow_common::ConnType::Tunnel,
        ..Metadata::default()
    };
    let _ = g.dial_tcp(&meta).await;
    assert_eq!(
        g.usage_generation(),
        0,
        "tunnel-typed probe dial must not record group use"
    );

    // Real user traffic still counts.
    let _ = g.dial_tcp(&Metadata::default()).await;
    assert_eq!(g.usage_generation(), 1, "user dial records group use");
}

#[tokio::test]
async fn user_pin_overrides_fastest_and_can_be_cleared() {
    let a = MockProxy::new("a");
    let b = MockProxy::new("b");
    a.set_delay(100);
    b.set_delay(10);
    let g = UrlTestGroup::new("ut", vec![a, b], 0);

    ProxySelection::set(&g, "a").await.unwrap();
    assert_eq!(pick(&g), "a");
    assert_eq!(ProxySelection::fixed(&g).as_deref(), Some("a"));

    ProxySelection::force_set(&g, None);
    assert_eq!(pick(&g), "b");
    assert_eq!(ProxySelection::fixed(&g).as_deref(), Some(""));
}

#[tokio::test]
async fn dead_url_test_pin_falls_back_without_forgetting_pin() {
    let a = MockProxy::new("a");
    let b = MockProxy::new("b");
    a.set_alive(false);
    b.set_delay(20);
    let g = UrlTestGroup::new("ut", vec![a, b], 0);

    ProxySelection::set(&g, "a").await.unwrap();
    assert_eq!(pick(&g), "b");
    assert_eq!(ProxySelection::fixed(&g).as_deref(), Some("a"));
}

#[tokio::test]
async fn repeated_dial_failures_mark_member_dead() {
    // mihomo GroupBase.onDialFailed escalation, applied to the fastest
    // member: five failures within the window mark it dead so routing
    // moves to the next candidate until a probe revives it.
    let a = MockProxy::new_failing("a", AdapterType::Shadowsocks, "dial timed out");
    let b = MockProxy::new_failing("b", AdapterType::Shadowsocks, "dial timed out");
    a.set_delay(10);
    b.set_delay(20);
    let a_ref = Arc::clone(&a);
    let b_ref = Arc::clone(&b);
    let g = UrlTestGroup::new("ut", vec![a, b], 0);

    for i in 1..5 {
        let _ = g.dial_tcp(&Metadata::default()).await;
        assert!(
            a_ref.alive(),
            "failure {i} is below the escalation threshold"
        );
    }
    let _ = g.dial_tcp(&Metadata::default()).await;
    // The first pick (a) has now failed five times in a row and is dead;
    // the next dial must be routed to b.
    assert!(
        !a_ref.alive(),
        "the repeatedly failing member is marked dead"
    );
    let _ = g.dial_tcp(&Metadata::default()).await;
    assert_eq!(a_ref.dials(), 5, "the dead member no longer receives dials");
    assert_eq!(b_ref.dials(), 1, "routing moved past the dead member");
}

#[tokio::test]
async fn connection_refused_marks_member_dead_immediately() {
    let a = MockProxy::new_failing("a", AdapterType::Shadowsocks, "connection refused");
    a.set_delay(10);
    let a_ref = Arc::clone(&a);
    let b = MockProxy::new("b");
    b.set_delay(20);
    let g = UrlTestGroup::new("ut", vec![a, b], 0);

    let _ = g.dial_tcp(&Metadata::default()).await;
    assert!(!a_ref.alive(), "refused escalates on the first failure");
    assert_eq!(pick(&g), "b", "the refused member is no longer selected");
}
/// Issue #681 wedge repro at the pick level: `url-test: [selector,
/// spare]` where the selector's pick always fails. The dead-mark must
/// evict the selector from `pick_for_dial`'s alive-member scan.
#[tokio::test]
async fn dead_marked_selector_member_is_skipped() {
    let leaf = MockProxy::new("leaf"); // dial always errors
    leaf.set_delay(50); // make sel the incumbent fastest
    let sel: Arc<dyn Proxy> = Arc::new(crate::group::selector::SelectorGroup::new(
        "sel",
        vec![leaf],
    ));
    let spare = MockProxy::new("spare");
    spare.set_delay(100);
    let spare_ref = Arc::clone(&spare);
    let g = UrlTestGroup::new("ut", vec![sel, spare], 0);

    for _ in 0..5 {
        let _ = g.dial_tcp(&Metadata::default()).await;
    }
    assert_eq!(spare_ref.dials(), 0, "fastest member serves all dials");
    let _ = g.dial_tcp(&Metadata::default()).await;
    assert_eq!(
        spare_ref.dials(),
        1,
        "dead-marked selector member must be skipped"
    );
}

/// Issue #681: escalation/probe writes land on the group's own health
/// bit — it must be observable next to the delegated fastest member.
#[test]
fn own_health_bit_makes_dead_marks_observable() {
    let g = UrlTestGroup::new("ut", vec![MockProxy::new("a")], 0);
    assert!(g.alive());
    g.health().set_alive(false);
    assert!(!g.alive() && !g.alive_for_url("https://x"));
    g.health().record_delay(50);
    assert!(g.alive());
}
