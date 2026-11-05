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
