use super::append_worker_monitor;
use astra_model::monitor::{InternalSnapshot, Metric, MetricKind};

#[test]
fn worker_generations_never_share_counter_names_or_accept_foreign_epochs() {
    let mut host = InternalSnapshot {
        epoch: "boot".into(),
        metrics: vec![],
    };
    let worker = |epoch: &str| InternalSnapshot {
        epoch: epoch.into(),
        metrics: vec![Metric {
            sampled_at_ms: None,
            name: "routing.started".into(),
            value: "9007199254740993".into(),
            description: "路由请求".into(),
            kind: MetricKind::Counter,
            unit: "count".into(),
        }],
    };
    assert!(append_worker_monitor(&mut host, 1, worker("boot:1")));
    assert!(append_worker_monitor(&mut host, 2, worker("boot:2")));
    assert!(!append_worker_monitor(&mut host, 3, worker("other-boot:3")));
    assert!(!append_worker_monitor(&mut host, 4, worker("boot:3")));
    assert_eq!(host.metrics.len(), 2);
    assert_eq!(host.metrics[0].name, "workers.1.routing.started");
    assert_eq!(host.metrics[1].name, "workers.2.routing.started");
    assert_eq!(host.metrics[0].value, "9007199254740993");
    assert_eq!(host.epoch, "boot");
}
