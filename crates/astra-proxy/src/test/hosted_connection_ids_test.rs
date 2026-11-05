use super::*;
#[test]
fn live_ids_are_stable_and_worker_ids_are_not_truncated() {
    let mut ids = ConnectionIds::with_seed(123);
    let large = u32::MAX as u64 + 2;
    assert!(ids.reconcile("a", [(1, large), (2, large)]));
    let first = ids.id("a", 1, large).unwrap();
    let second = ids.id("a", 2, large).unwrap();
    assert_ne!(first, second);
    assert!(ids.reconcile("a", [(2, large), (1, large), (3, 1)]));
    assert_eq!(ids.id("a", 1, large), Some(first));
    assert_eq!(ids.resolve(second).unwrap().worker, large);
}
#[test]
fn vanished_connections_and_previous_boot_ids_cannot_address_replacements() {
    let mut ids = ConnectionIds::with_seed(123);
    ids.reconcile("a", [(1, 1)]);
    let previous = ids.id("a", 1, 1).unwrap();
    ids.reconcile("a", []);
    assert!(ids.resolve(previous).is_none());
    ids.reconcile("a", [(1, 1)]);
    let replacement = ids.id("a", 1, 1).unwrap();
    assert_ne!(replacement, previous);
    ids.reconcile("b", [(1, 1)]);
    assert!(ids.resolve(replacement).is_none());
    assert_eq!(ids.resolve(ids.id("b", 1, 1).unwrap()).unwrap().boot, "b");
    let mut restarted = ConnectionIds::with_seed(456);
    restarted.reconcile("b", [(1, 1)]);
    assert!(restarted.resolve(previous).is_none());
}
#[test]
fn ids_skip_zero_and_collisions_and_stop_before_sequence_wrap() {
    let mut ids = ConnectionIds::with_seed(1u64.rotate_left(27) as u128);
    let colliding = 2u64.rotate_left(27) ^ ids.mask;
    ids.by_id.insert(
        colliding,
        Target {
            boot: "a".into(),
            generation: 1,
            worker: 1,
        },
    );
    assert_eq!(ids.allocate(), Some(3u64.rotate_left(27) ^ ids.mask));
    ids.next = Some(u64::MAX);
    assert!(ids.allocate().is_some());
    assert!(ids.allocate().is_none());
}
#[test]
fn mapping_memory_is_bounded_and_pruned() {
    let mut ids = ConnectionIds::with_seed(123);
    assert!(!ids.reconcile(
        "a",
        (0..MAX_CONNECTIONS as u64 + 1).map(|worker| (1, worker))
    ));
    assert_eq!(ids.by_id.len(), MAX_CONNECTIONS);
    assert_eq!(ids.by_target.len(), MAX_CONNECTIONS);
    assert!(ids.reconcile("a", [(2, 1)]));
    assert_eq!(ids.by_id.len(), 1);
    assert_eq!(ids.by_target.len(), 1);
}
