use super::*;
use std::time::{Duration, UNIX_EPOCH};

/// Minimal leaf/group: counts `reset_sessions` calls; a group's members
/// are set after construction so tests can build cycles.
struct CountingProxy {
    resets: std::sync::atomic::AtomicUsize,
    members: std::sync::Mutex<Option<Vec<Arc<dyn Proxy>>>>,
    health: ProxyHealth,
}

impl CountingProxy {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            resets: std::sync::atomic::AtomicUsize::new(0),
            members: std::sync::Mutex::new(None),
            health: ProxyHealth::new(),
        })
    }

    fn resets(&self) -> usize {
        self.resets.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ProxyAdapter for CountingProxy {
    fn name(&self) -> &str {
        "counting"
    }
    fn adapter_type(&self) -> AdapterType {
        AdapterType::Direct
    }
    fn addr(&self) -> &str {
        ""
    }
    fn support_udp(&self) -> bool {
        false
    }
    async fn dial_tcp(&self, _metadata: &Metadata) -> Result<Box<dyn ProxyConn>> {
        unreachable!("never dialed")
    }
    async fn dial_udp(&self, _metadata: &Metadata) -> Result<Box<dyn ProxyPacketConn>> {
        unreachable!("never dialed")
    }
    fn reset_sessions(&self) {
        self.resets.fetch_add(1, Ordering::SeqCst);
    }
    fn health(&self) -> &ProxyHealth {
        &self.health
    }
}

impl Proxy for CountingProxy {
    fn alive(&self) -> bool {
        true
    }
    fn alive_for_url(&self, _url: &str) -> bool {
        true
    }
    fn last_delay(&self) -> u16 {
        0
    }
    fn last_delay_for_url(&self, _url: &str) -> u16 {
        0
    }
    fn delay_history(&self) -> Vec<DelayHistory> {
        Vec::new()
    }
    fn member_proxies(&self) -> Option<Vec<Arc<dyn Proxy>>> {
        self.members.lock().unwrap().clone()
    }
}

#[test]
fn reset_sessions_reachable_visits_each_adapter_once_across_cycles() {
    let leaf_a = CountingProxy::new();
    let leaf_b = CountingProxy::new();
    let group_x = CountingProxy::new();
    let group_y = CountingProxy::new();
    // x → {a, b, y}; y → {a, x}: `a` is shared, x ↔ y is a cycle.
    *group_x.members.lock().unwrap() = Some(vec![
        Arc::clone(&leaf_a) as Arc<dyn Proxy>,
        Arc::clone(&leaf_b) as Arc<dyn Proxy>,
        Arc::clone(&group_y) as Arc<dyn Proxy>,
    ]);
    *group_y.members.lock().unwrap() = Some(vec![
        Arc::clone(&leaf_a) as Arc<dyn Proxy>,
        Arc::clone(&group_x) as Arc<dyn Proxy>,
    ]);

    // `a` is also a root in its own right (a route-table entry).
    let visited = reset_sessions_reachable([
        Arc::clone(&group_x) as Arc<dyn Proxy>,
        Arc::clone(&leaf_a) as Arc<dyn Proxy>,
    ]);

    assert_eq!(visited, 4);
    for proxy in [&leaf_a, &leaf_b, &group_x, &group_y] {
        assert_eq!(proxy.resets(), 1);
    }
    // Break the cycle so the test does not leak the pair.
    group_x.members.lock().unwrap().take();
}

#[test]
fn delay_history_time_serializes_as_rfc3339_string() {
    let entry = DelayHistory {
        time: UNIX_EPOCH + Duration::from_secs(1_751_527_000),
        delay: 76,
    };
    let json = serde_json::to_value(&entry).expect("serialize");
    assert_eq!(
        json,
        serde_json::json!({"time": "2025-07-03T07:16:40Z", "delay": 76}),
    );
}

#[test]
fn delay_history_round_trips_through_json() {
    let entry = DelayHistory {
        time: UNIX_EPOCH + Duration::from_secs(1_751_527_000),
        delay: 321,
    };
    let json = serde_json::to_string(&entry).expect("serialize");
    let back: DelayHistory = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.time, entry.time);
    assert_eq!(back.delay, entry.delay);
}
