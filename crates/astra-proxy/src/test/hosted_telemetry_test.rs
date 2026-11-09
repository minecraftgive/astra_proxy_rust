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

#[test]
fn dns_history_merges_generations_deduplicates_and_survives_retirement() {
    use astra_model::runtime::{DnsQuery, DnsQueryLog, DNS_QUERY_LIMIT};
    let mut store = Store::default();
    let log = |resolver: &str, start: u64| DnsQueryLog {
        records: (start..start + 80)
            .map(|id| DnsQuery {
                resolver: resolver.into(),
                id,
                timestamp_ms: id,
                ..Default::default()
            })
            .collect(),
        truncated: false,
    };
    store.merge_dns(vec![log("old", 1)], Some(true));
    store.merge_dns(vec![log("old", 1)], Some(true));
    assert_eq!(
        store.metadata("boot", 1, false).dns.unwrap().records.len(),
        80
    );
    store.merge_dns(vec![log("new", 60)], Some(true));
    let merged = store.metadata("boot", 2, false).dns.unwrap();
    assert_eq!(merged.records.len(), DNS_QUERY_LIMIT);
    assert!(merged.truncated);
    assert_eq!(merged.records[0].id, 139);
    assert!(merged.records.iter().any(|row| row.resolver == "old"));
    store.merge_dns(vec![], Some(true));
    assert_eq!(
        store.metadata("boot", 2, false).dns.unwrap().records.len(),
        DNS_QUERY_LIMIT
    );
    store.merge_dns(vec![], None);
    assert!(
        store.metadata("boot", 2, false).dns.is_some(),
        "temporary failures retain the last history"
    );
    store.merge_dns(vec![], Some(false));
    assert!(store.metadata("boot", 2, false).dns.is_none());
}

#[tokio::test]
async fn dns_query_reaches_worker_sample_and_json_runtime_view() {
    use super::super::{worker_sample, App, WorkerSample};
    use astra_model::runtime::RuntimeView;
    let app = App::from_text(
        std::path::Path::new("dns-history.toml"),
        r#"[dns]
enable=true
fake_ip=true
nameserver=["udp://1.1.1.1:53", "tcp://1.0.0.1:53", "tls://1.1.1.1:853", "https://resolver.example/dns-query"]
fallback=["tls://8.8.8.8:853"]
"#.into(),
    )
    .unwrap();
    let query = astra_dns::wire::query("telemetry.example", hickory_proto::rr::RecordType::A)
        .unwrap()
        .to_vec()
        .unwrap();
    app.dns_answer(&query).await.unwrap();
    let wire = serde_json::to_value(worker_sample(&app)).unwrap();
    let old: Snapshot = serde_json::from_value(wire.clone()).unwrap();
    let sample: WorkerSample = serde_json::from_value(wire).unwrap();
    let resolver = sample.dns_resolver.as_ref().unwrap();
    assert!(resolver.enabled && resolver.fake_ip);
    assert_eq!(
        resolver
            .nameservers
            .iter()
            .map(|s| s.transport.as_str())
            .collect::<Vec<_>>(),
        ["UDP", "TCP", "DoT", "DoH"]
    );
    assert_eq!(resolver.fallback[0].address, "tls://8.8.8.8:853");
    let mut store = Store::default();
    store.dns_resolver = sample.dns_resolver.map(Box::new);
    store.merge_dns(vec![sample.dns.unwrap()], Some(true));
    let wire = serde_json::to_value(RuntimeView {
        snapshot: old,
        telemetry: Some(store.metadata("boot", 1, false)),
    })
    .unwrap();
    let decoded: RuntimeView = serde_json::from_value(wire).unwrap();
    let telemetry = decoded.telemetry.unwrap();
    let resolver = telemetry.dns_resolver.unwrap();
    assert_eq!(resolver.listen[0].to_string(), "127.0.0.1:1053");
    assert_eq!(resolver.nameservers[3].transport, "DoH");
    let log = telemetry.dns.unwrap();
    assert_eq!(log.records.len(), 1);
    assert_eq!(log.records[0].domain, "telemetry.example");
    assert_eq!(log.records[0].source, "fake_ip");
    assert!(!log.records[0].answers.is_empty());
}

#[tokio::test]
async fn simple_dns_query_through_current_listener_uses_fake_ip_and_enters_history() {
    use astra_model::nettool::{Egress, Event, Probe, Request};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = std::sync::Arc::new(
        crate::App::from_text(
            std::path::Path::new("dns-query.toml"),
            format!("[dns]\nenable=true\nfake_ip=true\nlisten=[\"{address}\"]\n"),
        )
        .unwrap(),
    );
    let serving = app.clone();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        // The one-shot query closes its stream after the answer; the server's
        // next framed read then ends with EOF.
        let _ = crate::dns::tcp(stream, serving).await;
    });
    let status = super::super::worker_sample(&app).dns_resolver.unwrap();
    let request = Request {
        config: String::new(),
        egress: Egress::System,
        probe: Probe::Dns {
            name: "simple-query.example".into(),
            record: "A".into(),
            server: format!("tcp://{}", status.listen[0]),
        },
        count: 1,
        interval_ms: 0,
        timeout_ms: 2000,
    };
    let events = std::sync::Mutex::new(Vec::new());
    let summary = crate::nettool::run(None, &request, false, |event| {
        events.lock().unwrap().push(event);
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(summary.received, 1);
    assert!(events.lock().unwrap().iter().any(
        |event| matches!(event, Event::Attempt(attempt) if attempt.detail.contains("198.18."))
    ));
    server.await.unwrap();
    let log = app.dns.as_ref().unwrap().query_log();
    assert_eq!(log.records.len(), 1);
    assert_eq!(log.records[0].domain, "simple-query.example");
    assert_eq!(log.records[0].source, "fake_ip");
}
