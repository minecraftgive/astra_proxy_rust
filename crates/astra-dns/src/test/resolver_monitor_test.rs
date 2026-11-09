use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

enum Behavior {
    Answer,
    Failure,
    Pending,
}
struct Mock {
    behavior: Behavior,
    calls: AtomicUsize,
}
impl Mock {
    fn new(behavior: Behavior) -> Self {
        Self {
            behavior,
            calls: AtomicUsize::new(0),
        }
    }
}
impl Transport for Mock {
    fn exchange<'a>(
        &'a self,
        _: &'a Upstream,
        raw: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>>> + Send + 'a>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::Relaxed);
            match self.behavior {
                Behavior::Failure => bail!("mock upstream failure"),
                Behavior::Pending => std::future::pending().await,
                Behavior::Answer => {
                    let query = Message::from_vec(&raw)?;
                    let mut reply = wire::response(&query, ResponseCode::NoError);
                    reply.answers.push(Record::from_rdata(
                        query.queries[0].name().clone(),
                        60,
                        RData::A(A("203.0.113.1".parse().unwrap())),
                    ));
                    Ok(reply.to_vec()?)
                }
            }
        })
    }
}

#[tokio::test]
async fn monitor_records_cache_failure_timeout_cancellation_fake_ip_and_saturation() {
    let resolver = Resolver::new(Config {
        cache_capacity: 1,
        max_concurrent: 1,
        query_timeout_ms: 100,
        ..Config::default()
    })
    .unwrap();
    let good = Mock::new(Behavior::Answer);
    let query = wire::query("one.example", RecordType::A)
        .unwrap()
        .to_vec()
        .unwrap();
    resolver.answer(&query, &good).await.unwrap();
    resolver.answer(&query, &good).await.unwrap();
    assert_eq!(good.calls.load(Ordering::Relaxed), 1);
    let other = wire::query("two.example", RecordType::A)
        .unwrap()
        .to_vec()
        .unwrap();
    resolver.answer(&other, &good).await.unwrap();

    let failed = Mock::new(Behavior::Failure);
    assert!(resolver.answer(&query, &failed).await.is_err());
    let hanging = Mock::new(Behavior::Pending);
    assert!(resolver.answer(&query, &hanging).await.is_err());
    let mut cancelled = Box::pin(resolver.answer(&query, &hanging));
    std::future::poll_fn(|cx| {
        assert!(cancelled.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    drop(cancelled);

    let permit = resolver.requests.clone().try_acquire_owned().unwrap();
    assert!(resolver
        .answer(&query, &good)
        .await
        .unwrap_err()
        .to_string()
        .contains("request limit"));
    drop(permit);
    let permit = resolver.permits.clone().try_acquire_owned().unwrap();
    assert!(resolver
        .answer(&query, &good)
        .await
        .unwrap_err()
        .to_string()
        .contains("query limit"));
    drop(permit);

    let fake = Resolver::new(Config {
        fake_ip: true,
        ..Config::default()
    })
    .unwrap();
    let raw = fake.answer(&query, &failed).await.unwrap();
    assert!(matches!(
        Message::from_vec(&raw).unwrap().answers[0].data,
        RData::A(_)
    ));

    let snapshot = astra_observe::snapshot("dns-test");
    // Other concurrently running resolvers may increase counters; assertions
    // only require evidence of these paths, never a process-global exact delta.
    for name in [
        "dns.cache.hits",
        "dns.cache.misses",
        "dns.cache.evictions",
        "dns.answer.failed",
        "dns.answer.cancelled",
        "dns.resolve.completed",
        "dns.upstream.failed",
        "dns.upstream.cancelled",
        "dns.upstream.timeouts",
        "dns.fake_ip.replies",
        "dns.request_limit_rejections",
        "dns.query_limit_rejections",
    ] {
        let metric = snapshot
            .metrics
            .iter()
            .find(|metric| metric.name == name)
            .unwrap();
        assert!(metric.value.parse::<u64>().unwrap() > 0, "{name}");
        assert_eq!(metric.kind, astra_observe::Kind::Counter);
        assert_eq!(metric.unit, "count");
    }
    let duration = snapshot
        .metrics
        .iter()
        .find(|metric| metric.name == "dns.upstream.duration_us")
        .unwrap();
    assert_eq!(duration.unit, "us");
    assert!(snapshot
        .metrics
        .iter()
        .any(|metric| metric.name == "dns.answer.in_flight"
            && metric.kind == astra_observe::Kind::Gauge));
}

#[tokio::test]
async fn dns_query_history_records_real_cache_fake_failure_and_cancellation() {
    let resolver = Resolver::new(Config::default()).unwrap();
    let query = wire::query("history.example", RecordType::A)
        .unwrap()
        .to_vec()
        .unwrap();
    let good = Mock::new(Behavior::Answer);
    resolver.answer(&query, &good).await.unwrap();
    resolver.answer(&query, &good).await.unwrap();
    let log = resolver.query_log();
    assert_eq!(log.records.len(), 2);
    assert_eq!(log.records[0].source, "cache");
    assert_eq!(log.records[1].source, "upstream");
    assert!(log.records[1].upstream.is_some());
    assert_eq!(log.records[0].domain, "history.example");
    assert_eq!(log.records[0].query_type, "A");
    assert_eq!(log.records[0].status, "NoError");
    assert!(log.records[0].answers[0].contains("203.0.113.1"));
    assert!(log.records[0].id > log.records[1].id);

    let failed_query = wire::query("failure.example", RecordType::A)
        .unwrap()
        .to_vec()
        .unwrap();
    assert!(resolver
        .answer(&failed_query, &Mock::new(Behavior::Failure))
        .await
        .is_err());
    assert_eq!(resolver.query_log().records[0].status, "ERROR");
    assert!(resolver.query_log().records[0].error.is_some());
    let hanging = Mock::new(Behavior::Pending);
    let mut cancelled = Box::pin(resolver.answer(&failed_query, &hanging));
    std::future::poll_fn(|cx| {
        assert!(cancelled.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    drop(cancelled);
    assert_eq!(resolver.query_log().records[0].status, "CANCELLED");
    let fake = Resolver::new(Config {
        fake_ip: true,
        ..Default::default()
    })
    .unwrap();
    fake.answer(&query, &good).await.unwrap();
    assert_eq!(fake.query_log().records[0].source, "fake_ip");
    assert!(fake.query_log().records[0].upstream.is_none());
    assert_ne!(
        fake.query_log().records[0].resolver,
        log.records[0].resolver
    );
}

#[tokio::test]
async fn dns_query_history_is_bounded_and_internal_lookups_are_not_duplicated() {
    let resolver = Resolver::new(Config::default()).unwrap();
    let good = Mock::new(Behavior::Answer);
    for _ in 0..140 {
        resolver.lookup("lookup.example", &good).await.unwrap();
    }
    let log = resolver.query_log();
    assert_eq!(log.records.len(), astra_model::runtime::DNS_QUERY_LIMIT);
    assert!(log.truncated);
    assert_eq!(log.records[0].id, 140);
    assert_eq!(good.calls.load(Ordering::Relaxed), 1);
    let mut replacement = Resolver::new(Config::default()).unwrap();
    replacement.inherit(&resolver).unwrap();
    assert_eq!(
        replacement.query_log().records[0].resolver,
        log.records[0].resolver
    );
}
