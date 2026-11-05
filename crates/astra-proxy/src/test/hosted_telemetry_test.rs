use super::*;
#[test]
fn cached_reads_and_failed_rounds_do_not_become_new_samples() {
    let mut store = Store::default();
    store.publish(
        Snapshot::default(),
        BTreeMap::from([(1, Some(Instant::now()))]),
        BTreeMap::new(),
    );
    let first = store.metadata("boot", 1, false);
    assert_eq!(first.coverage, Coverage::Complete);
    assert_eq!(
        store.metadata("boot", 2, true).sample_sequence,
        first.sample_sequence
    );
    store.publish(
        Snapshot::default(),
        BTreeMap::from([(1, None)]),
        BTreeMap::new(),
    );
    let failed = store.metadata("boot", 2, true);
    assert_eq!(failed.sample_sequence, first.sample_sequence);
    assert_eq!(failed.coverage, Coverage::Stale);
    assert!(failed.ingress_paused);
}
#[test]
fn partial_generations_and_origins_are_explicit() {
    let mut store = Store::default();
    store.publish(
        Snapshot::default(),
        BTreeMap::from([(1, None), (2, Some(Instant::now()))]),
        BTreeMap::from([(99, 2)]),
    );
    let view = store.metadata("a", 2, false);
    assert_eq!(view.coverage, Coverage::Partial);
    assert_eq!(view.generations[0].sample_age_ms, None);
    assert_eq!(view.connection_generations[&99], 2);
}
#[test]
fn activating_an_unsampled_generation_marks_coverage_partial() {
    let mut store = Store::default();
    store.publish(
        Snapshot::default(),
        BTreeMap::from([(1, Some(Instant::now()))]),
        BTreeMap::new(),
    );
    let view = store.metadata("boot", 2, false);
    assert_eq!(view.coverage, Coverage::Partial);
    assert_eq!(view.active_generation, Some(2));
    assert_eq!(
        view.generations.len(),
        1,
        "missing data is not a fabricated sample"
    );
    assert_eq!(view.generations[0].generation, 1);
    store.publish(
        Snapshot::default(),
        BTreeMap::from([(2, Some(Instant::now()))]),
        BTreeMap::new(),
    );
    let complete = store.metadata("boot", 2, false);
    assert_eq!(complete.coverage, Coverage::Complete);
    assert_eq!(complete.generations.len(), 1);
    assert_eq!(complete.generations[0].generation, 2);
}
#[test]
fn slow_round_does_not_relabel_an_old_worker_observation_as_fresh() {
    let mut store = Store {
        started: Instant::now() - std::time::Duration::from_secs(5),
        ..Default::default()
    };
    store.publish(
        Snapshot::default(),
        BTreeMap::from([(1, Some(Instant::now() - std::time::Duration::from_secs(4)))]),
        BTreeMap::new(),
    );
    let view = store.metadata("boot", 1, false);
    assert_eq!(view.coverage, Coverage::Stale);
    assert!(!view.generations[0].healthy);
    assert!(view.generations[0].sample_age_ms.unwrap() >= 4000);
}
