use super::*;
#[test]
fn replacement_expiry_and_capacity_keep_indices_and_bytes_exact() {
    let mut cache = Cache::default();
    let now = Instant::now();
    for i in 0..1000 {
        cache.insert(vec![1], vec![2; i + 1], 10, 2, now);
        assert_eq!(cache.bytes, i + 2);
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.expiry.len(), 1);
        assert_eq!(cache.order.len(), 1);
    }
    cache.insert(vec![2], vec![3], 1, 2, now);
    cache.insert(vec![3], vec![4], 10, 2, now);
    assert!(cache.get(&[1], now).is_none(), "oldest insertion evicted");
    assert!(cache.get(&[2], now + Duration::from_secs(2)).is_none());
    assert_eq!(cache.bytes, 2);
    cache.expire(now + Duration::from_secs(11));
    assert_eq!(
        (cache.bytes, cache.order.len(), cache.expiry.len()),
        (0, 0, 0)
    );
}
#[test]
fn twenty_load_idle_cycles_release_empty_capacity() {
    let mut cache = Cache::default();
    for _ in 0..20 {
        let now = Instant::now();
        for i in 0u32..4096 {
            cache.insert(i.to_be_bytes().to_vec(), vec![0; 64], 1, 4096, now);
        }
        cache.maintain(now + Duration::from_secs(2));
        cache.maintain(now + Duration::from_secs(33));
        assert_eq!(cache.entries.capacity(), 0);
        assert_eq!(cache.bytes, 0);
        assert!(cache.expiry.is_empty() && cache.order.is_empty());
    }
}
#[test]
fn budget_and_zero_capacity_are_hard_bounds() {
    let now = Instant::now();
    let mut cache = Cache::default();
    cache.insert(vec![1], vec![2; BYTE_LIMIT], 60, 4, now);
    assert!(cache.entries.is_empty());
    cache.insert(vec![1], vec![2], 60, 0, now);
    assert!(cache.entries.is_empty());
}

#[tokio::test]
async fn maintenance_expires_idle_entries_without_retaining_the_owner() {
    let state = std::sync::Arc::new(std::sync::Mutex::new(Cache::default()));
    let maintenance = crate::maintenance::Maintenance::default();
    state
        .lock()
        .unwrap()
        .insert(vec![1], vec![2], 1, 4, Instant::now());
    maintenance.start(&state);
    maintenance.start(&state);
    tokio::time::sleep(Duration::from_millis(5200)).await;
    assert!(state.lock().unwrap().entries.is_empty());
    assert_eq!(std::sync::Arc::strong_count(&state), 1);
    let weak = std::sync::Arc::downgrade(&state);
    drop(state);
    drop(maintenance);
    tokio::task::yield_now().await;
    assert!(weak.upgrade().is_none());
}

#[test]
fn a_load_burst_restarts_the_low_occupancy_grace_period() {
    let mut cache = Cache::default();
    let now = Instant::now();
    for i in 0..64 {
        cache.insert(vec![i], vec![0], 1, 64, now);
    }
    cache.maintain(now + Duration::from_secs(2));
    for i in 0..64 {
        cache.insert(vec![i], vec![0], 1, 64, now + Duration::from_secs(20));
    }
    cache.maintain(now + Duration::from_secs(22));
    cache.maintain(now + Duration::from_secs(33));
    assert!(
        cache.entries.capacity() >= 64,
        "recent activity resets the shrink grace period"
    );
    cache.maintain(now + Duration::from_secs(53));
    assert_eq!(cache.entries.capacity(), 0);
}
