use super::*;

fn nodes(bytes: u64) -> BTreeMap<String, u64> {
    [("tokyo".into(), bytes)].into()
}

#[test]
fn windows_expire_without_losing_persistent_or_session_totals() {
    let mut history = History::default();
    let now = 100_000;
    for (age, total) in [
        (MONTH + 1, 1),
        (8 * 1440, 3),
        (2 * 1440, 7),
        (120, 15),
        (0, 31),
    ] {
        history.observe(1, &nodes(total), now - age);
    }
    assert_eq!(
        history.summary(now)["tokyo"],
        NodeTraffic {
            session: 31,
            hour: 16,
            day: 24,
            week: 28,
            month: 30,
            persist: 31,
        }
    );
    assert_eq!(history.summary(now + 60)["tokyo"].hour, 0);
    assert_eq!(history.summary(now + MONTH)["tokyo"].month, 0);
    assert_eq!(history.summary(now + MONTH)["tokyo"].persist, 31);
}

#[test]
fn duplicate_samples_draining_generations_and_node_switches_are_not_double_counted() {
    let mut history = History::default();
    history.observe(1, &nodes(100), 100_000);
    history.observe(1, &nodes(100), 100_000);
    history.observe(1, &nodes(90), 100_000);
    history.observe(2, &nodes(30), 100_000);
    history.observe(1, &nodes(120), 100_000);
    history.observe(2, &[("osaka".into(), 70)].into(), 100_000);
    let totals = history.summary(100_000);
    assert_eq!(totals["tokyo"].session, 150);
    assert_eq!(totals["osaka"].session, 70);
    history.forget_generation(1);
    assert_eq!(history.summary(100_000), totals);
}

#[test]
fn restart_restores_history_but_begins_a_new_session() {
    let path = std::env::temp_dir()
        .join(format!("astra-traffic-{}", uuid::Uuid::new_v4()))
        .join("history.json");
    let mut history = History::open(Ok(path.clone()));
    history.observe(1, &nodes(100), 100_000);
    history.save();
    assert!(history.error.is_none(), "{:?}", history.error);
    let mut history = History::open(Ok(path.clone()));
    history.observe(1, &nodes(20), 100_001);
    let total = history.summary(100_001)["tokyo"];
    assert_eq!(total.session, 20);
    assert_eq!(total.persist, 120);
    assert_eq!(total.hour, 120);
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn unreadable_history_is_preserved() {
    let path = std::env::temp_dir()
        .join(format!("astra-traffic-{}", uuid::Uuid::new_v4()))
        .join("history.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "broken history").unwrap();
    let mut history = History::open(Ok(path.clone()));
    assert!(history.error.is_some());
    history.observe(1, &nodes(10), 100_000);
    history.save();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "broken history");
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn worker_node_counters_preserve_legacy_json_snapshot_compatibility() {
    use super::super::WorkerSample;
    use astra_model::runtime::Snapshot;
    let legacy = Snapshot {
        upload: 42,
        ..Default::default()
    };
    let sample: WorkerSample =
        serde_json::from_value(serde_json::to_value(&legacy).unwrap()).unwrap();
    assert!(sample.node_totals.is_none());
    let enhanced = WorkerSample {
        snapshot: legacy,
        node_totals: Some(nodes(42)),
        dns: None,
        dns_resolver: None,
    };
    let old_reader: Snapshot =
        serde_json::from_value(serde_json::to_value(enhanced).unwrap()).unwrap();
    assert_eq!(old_reader.upload, 42);
}

#[tokio::test]
async fn traffic_node_follows_standalone_and_selected_groups() {
    use super::super::{selected_traffic_node, worker_sample, App};
    use astra_model::runtime::Mode;
    let mut app = App::from_text(std::path::Path::new("traffic-node.toml"),
        "standalone=true\nstandalone_outbound='main'\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\nkind='socks5'\ntag='a'\nserver='127.0.0.1:1234'\n[[outbounds]]\nkind='socks5'\ntag='b'\nserver='127.0.0.1:1235'\n[proxy_groups.main]\nmembers=['b']\n[proxy_groups.PROXY]\nmembers=['a']\n".into()).unwrap();
    let mut sample = worker_sample(&app);
    assert_eq!(sample.node_totals.as_ref().unwrap().len(), 2);
    assert_eq!(
        selected_traffic_node(&app, &sample.snapshot).as_deref(),
        Some("b")
    );
    app.config.standalone = false;
    assert_eq!(
        selected_traffic_node(&app, &sample.snapshot).as_deref(),
        Some("a")
    );
    sample.snapshot.mode = Mode::Direct;
    assert_eq!(selected_traffic_node(&app, &sample.snapshot), None);
}
