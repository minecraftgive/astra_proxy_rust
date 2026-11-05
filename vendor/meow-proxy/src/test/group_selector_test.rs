use super::*;
use crate::group::test_support::MockProxy;
use meow_common::Metadata;

/// `member_proxies()` must cover provider-slot members (issue #543
/// item 1) and list them in `proxy_names()` order.
#[test]
fn member_proxies_include_provider_slots_in_proxy_names_order() {
    let slot: ProviderSlot = Arc::new(RwLock::new(vec![
        MockProxy::new("p1") as Arc<dyn Proxy>,
        MockProxy::new("p2") as Arc<dyn Proxy>,
    ]));
    let g = SelectorGroup::new_with_providers(
        "sel",
        vec![MockProxy::new("a"), MockProxy::new("b")],
        vec![slot],
    );
    let names: Vec<String> = g
        .member_proxies()
        .expect("groups expose members")
        .iter()
        .map(|p| p.name().to_string())
        .collect();
    assert_eq!(names, g.proxy_names());
    assert_eq!(names, vec!["a", "b", "p1", "p2"]);
}

#[test]
fn unselected_falls_back_to_first_static() {
    let a = MockProxy::new("a");
    let b = MockProxy::new("b");
    let g = SelectorGroup::new("sel", vec![a, b]);
    assert_eq!(g.selected_proxy().unwrap().name(), "a");
    assert_eq!(g.proxy_names(), vec!["a", "b"]);
}

#[test]
fn select_changes_target_and_returns_true_only_when_present() {
    let a = MockProxy::new("a");
    let b = MockProxy::new("b");
    let g = SelectorGroup::new("sel", vec![a, b]);
    assert!(g.select("b"));
    assert_eq!(g.selected_proxy().unwrap().name(), "b");
    assert!(!g.select("nope"));
    // Unknown name must not clobber the previous choice.
    assert_eq!(g.selected_proxy().unwrap().name(), "b");
}

/// `unwrap_proxy` must surface the *selected* member, not the first —
/// a peek that returned `static_proxies[0]` would misclassify a
/// selector pinned to PASS as a live target (issue #533).
#[test]
fn unwrap_peek_returns_the_selected_member() {
    let g = SelectorGroup::new("sel", vec![MockProxy::new("a"), MockProxy::new("b")]);
    let meta = Metadata::default();
    assert_eq!(g.unwrap_proxy(&meta, false).unwrap().name(), "a");
    assert!(g.select("b"));
    assert_eq!(
        g.unwrap_proxy(&meta, false).unwrap().name(),
        "b",
        "peek must reflect the selection, not the first member"
    );
    assert_eq!(g.unwrap_proxy(&meta, true).unwrap().name(), "b");
}

#[test]
fn selection_falls_back_when_named_proxy_disappears() {
    // Simulates a provider refresh that removed the previously-selected
    // node. Manually poke the selection to a missing name (the public
    // `select` would reject it; this mirrors the post-refresh state).
    let a = MockProxy::new("a");
    let g = SelectorGroup::new("sel", vec![a]);
    *g.selected.write() = Some("ghost".into());
    assert_eq!(g.selected_proxy().unwrap().name(), "a");
}

#[test]
fn store_primes_initial_selection_and_persists_changes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sel.json");
    let store = crate::group::selector_store::SelectorStore::open(path.clone());
    store.set("sel", "b");

    let a = MockProxy::new("a");
    let b = MockProxy::new("b");
    let g = SelectorGroup::new("sel", vec![a, b]).with_store(Arc::clone(&store));
    assert_eq!(
        g.selected_proxy().unwrap().name(),
        "b",
        "primed selection should load from store"
    );

    assert!(g.select("a"));
    drop(store);
    let store2 = crate::group::selector_store::SelectorStore::open(path);
    assert_eq!(store2.get("sel").as_deref(), Some("a"));
}

#[test]
fn support_udp_reflects_selected_target() {
    let tcp = MockProxy::new("tcp-only");
    let udp = MockProxy::new_udp("udp-able");
    let g = SelectorGroup::new("sel", vec![tcp, udp]);
    assert!(!g.support_udp(), "default selection is tcp-only");
    assert!(g.select("udp-able"));
    assert!(g.support_udp(), "switching selection updates udp support");
}

/// Issue #681: `record_dial_failure` escalation and probe sweeps write
/// `health().set_alive` / `record_delay` — a pure delegation made the
/// group member's own dead-mark unreadable, wedging any parent group
/// that picked it.
#[test]
fn own_health_bit_makes_dead_marks_observable() {
    let leaf = MockProxy::new("leaf");
    let g = SelectorGroup::new("sel", vec![leaf]);
    assert!(g.alive() && g.alive_for_url("https://x"));

    g.health().set_alive(false);
    assert!(!g.alive(), "escalation mark must stop selection");
    assert!(!g.alive_for_url("https://x"));

    // The delegated half is preserved: a leaf dead-marked by its own
    // tracker/sweep still propagates through the selector.
    g.health().set_alive(true);
    assert!(g.alive());
    g.selected_proxy().unwrap().health().set_alive(false);
    assert!(!g.alive(), "dead selected member still reports dead");
}

/// Probe sweeps revive via `record_delay`, not `set_alive(true)` — the
/// write must clear the same bit escalation sets.
#[test]
fn record_delay_revives_a_dead_marked_selector() {
    let g = SelectorGroup::new("sel", vec![MockProxy::new("leaf")]);
    g.health().set_alive(false);
    assert!(!g.alive());
    g.health().record_delay(42);
    assert!(g.alive(), "a successful probe must clear the dead-mark");
    g.health().record_delay(0);
    assert!(!g.alive(), "a failed probe dead-marks the same way");
}

#[tokio::test]
async fn dial_tcp_routes_to_selected_member() {
    let a = MockProxy::new("a");
    let b = MockProxy::new("b");
    let a_ref = Arc::clone(&a);
    let b_ref = Arc::clone(&b);
    let g = SelectorGroup::new("sel", vec![a, b]);
    assert!(g.select("b"));
    let _ = g.dial_tcp(&Metadata::default()).await; // mock returns Err
    assert_eq!(a_ref.dials(), 0);
    assert_eq!(b_ref.dials(), 1);
}
