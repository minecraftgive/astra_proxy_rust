use anyhow::Result;
use astra_dns::{wire, Config, Resolver, Transport, Upstream};
use hickory_proto::{
    op::{Message, ResponseCode},
    rr::{rdata::A, RData, Record, RecordType},
};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    },
    time::Duration,
};

#[derive(Default)]
struct Mock {
    calls: AtomicUsize,
    servers: Mutex<Vec<String>>,
    wrong_id: bool,
    ttl: u32,
    negative: bool,
}
impl Transport for Mock {
    fn exchange<'a>(
        &'a self,
        upstream: &'a Upstream,
        raw: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>>> + Send + 'a>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.servers.lock().unwrap().push(upstream.host.clone());
            if upstream.host == "192.0.2.1" {
                anyhow::bail!("simulated failed upstream");
            }
            let query = Message::from_vec(&raw)?;
            let mut reply = wire::response(&query, ResponseCode::NoError);
            reply.answers.push(Record::from_rdata(
                query.queries[0].name().clone(),
                self.ttl,
                RData::A(A("203.0.113.7".parse()?)),
            ));
            if self.negative {
                reply.answers.clear();
                reply.metadata.response_code = ResponseCode::NXDomain;
                reply.authorities.push(Record::from_rdata(
                    query.queries[0].name().clone(),
                    30,
                    RData::SOA(hickory_proto::rr::rdata::SOA::new(
                        "ns.example.".parse()?,
                        "admin.example.".parse()?,
                        1,
                        3600,
                        600,
                        86400,
                        10,
                    )),
                ));
            }
            if self.wrong_id {
                reply.metadata.id = reply.metadata.id.wrapping_add(1);
            }
            Ok(reply.to_vec()?)
        })
    }
}
async fn answer(resolver: &Resolver, mock: &Mock, name: &str, kind: RecordType) -> Message {
    Message::from_vec(
        &resolver
            .answer(&wire::query(name, kind).unwrap().to_vec().unwrap(), mock)
            .await
            .unwrap(),
    )
    .unwrap()
}
fn address(message: &Message) -> std::net::IpAddr {
    match message.answers[0].data {
        RData::A(ip) => ip.0.into(),
        RData::AAAA(ip) => ip.0.into(),
        _ => panic!("address answer expected"),
    }
}
#[tokio::test]
async fn fake_ip_is_stable_unique_bounded_and_survives_reload() {
    let config = Config {
        fake_ip: true,
        fake_ip_range6: Some("fdab::/64".into()),
        mapping_capacity: 3,
        ..Config::default()
    };
    let resolver = Resolver::new(config.clone()).unwrap();
    let mock = Mock::default();
    let a = address(&answer(&resolver, &mock, "one.example", RecordType::A).await);
    let retry = address(&answer(&resolver, &mock, "ONE.example.", RecordType::A).await);
    assert_eq!(a, retry);
    let b = address(&answer(&resolver, &mock, "two.example", RecordType::A).await);
    assert_ne!(a, b);
    let v6 = address(&answer(&resolver, &mock, "one.example", RecordType::AAAA).await);
    assert!(v6.is_ipv6());
    assert_eq!(resolver.restore(a).unwrap().as_deref(), Some("one.example"));
    assert_eq!(
        resolver.restore(v6).unwrap().as_deref(),
        Some("one.example")
    );
    assert!(resolver
        .answer(
            &wire::query("overflow.example", RecordType::A)
                .unwrap()
                .to_vec()
                .unwrap(),
            &mock
        )
        .await
        .is_err());
    assert!(resolver.restore("198.18.99.9".parse().unwrap()).is_err());
    assert_eq!(mock.calls.load(Ordering::SeqCst), 0);
    let mut reload = Resolver::new(config.clone()).unwrap();
    reload.inherit(&resolver).unwrap();
    assert_eq!(reload.restore(a).unwrap(), resolver.restore(a).unwrap());
    let mut changed = Resolver::new(Config {
        fake_ip_range: "198.19.0.0/16".into(),
        ..config
    })
    .unwrap();
    assert!(changed.inherit(&resolver).is_err());
    assert!(answer(&resolver, &mock, "one.example", RecordType::HTTPS)
        .await
        .answers
        .is_empty());
}
#[tokio::test]
async fn cache_restores_transaction_ids_ages_ttl_and_expires() {
    let resolver = Resolver::new(Config::default()).unwrap();
    let mock = Mock {
        ttl: 1,
        ..Mock::default()
    };
    let query = wire::query("cache.example", RecordType::A).unwrap();
    resolver
        .answer(&query.to_vec().unwrap(), &mock)
        .await
        .unwrap();
    let mut second = query.clone();
    second.metadata.id = second.metadata.id.wrapping_add(1);
    let reply = Message::from_vec(
        &resolver
            .answer(&second.to_vec().unwrap(), &mock)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(reply.metadata.id, second.metadata.id);
    assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    resolver
        .answer(&second.to_vec().unwrap(), &mock)
        .await
        .unwrap();
    assert_eq!(mock.calls.load(Ordering::SeqCst), 2);
}
#[tokio::test]
async fn policies_failover_and_ambiguous_real_addresses() {
    let mut config = Config::default();
    config
        .nameserver_policy
        .insert("+.special.example".into(), vec!["192.0.2.1".into()]);
    config.fallback = vec!["tcp://192.0.2.2:5353".into()];
    let resolver = Resolver::new(config).unwrap();
    let mock = Mock {
        ttl: 30,
        ..Mock::default()
    };
    let ip = address(&answer(&resolver, &mock, "a.special.example", RecordType::A).await);
    assert_eq!(&*mock.servers.lock().unwrap(), &["192.0.2.1", "192.0.2.2"]);
    assert_eq!(
        resolver.restore(ip).unwrap().as_deref(),
        Some("a.special.example")
    );
    answer(&resolver, &mock, "another.example", RecordType::A).await;
    assert_eq!(
        resolver.restore(ip).unwrap(),
        None,
        "shared IP must not acquire an arbitrary domain"
    );
}
#[tokio::test]
async fn filters_bootstrap_exclusions_and_real_lookup_bypass_fake_answers() {
    let mut resolver = Resolver::new(Config {
        fake_ip: true,
        ..Config::default()
    })
    .unwrap();
    let mock = Mock {
        ttl: 60,
        ..Mock::default()
    };
    resolver.exclude_domain("node.example");
    let public = address(&answer(&resolver, &mock, "public.example", RecordType::A).await);
    assert!(resolver.is_fake(public));
    assert!(!resolver.is_fake(address(
        &answer(&resolver, &mock, "printer.lan", RecordType::A).await
    )));
    assert!(!resolver.is_fake(address(
        &answer(&resolver, &mock, "node.example", RecordType::A).await
    )));
    assert_eq!(
        resolver.lookup("public.example", &mock).await.unwrap(),
        ["203.0.113.7".parse::<std::net::IpAddr>().unwrap()]
    );
    let ipv6 = answer(&resolver, &mock, "public.example", RecordType::AAAA).await;
    assert!(
        ipv6.answers.is_empty(),
        "no unconfigured real AAAA leak in fake mode"
    );
}
#[tokio::test]
async fn mismatched_upstream_ids_and_malformed_queries_are_rejected() {
    let resolver = Resolver::new(Config::default()).unwrap();
    let mock = Mock {
        wrong_id: true,
        ttl: 60,
        ..Mock::default()
    };
    let mut query = wire::query("bad.example", RecordType::A).unwrap();
    assert!(resolver
        .answer(&query.to_vec().unwrap(), &mock)
        .await
        .is_err());
    query.queries.push(query.queries[0].clone());
    assert!(resolver
        .answer(&query.to_vec().unwrap(), &mock)
        .await
        .is_err());
    assert!(resolver.answer(&[0; 4], &mock).await.is_err());
}
#[test]
fn validates_upstreams_and_domain_patterns() {
    for raw in [
        "1.1.1.1",
        "[::1]:53",
        "udp://1.1.1.1:53",
        "tcp://1.1.1.1:53",
        "tls://dns.example",
        "https://dns.example/dns-query?x=1",
    ] {
        Upstream::parse(raw).unwrap();
    }
    for raw in [
        "https://user:pass@dns.example/",
        "quic://1.1.1.1",
        "udp://1.1.1.1:0",
        "tls://dns.example/wrong",
    ] {
        assert!(Upstream::parse(raw).is_err(), "{raw}");
    }
    assert!(astra_dns::config::matches_domain(
        "+.example.com",
        "a.example.com"
    ));
    assert!(!astra_dns::config::matches_domain(
        "*.example.com",
        "example.com"
    ));
    assert!(Resolver::new(Config {
        fake_ip_range: "10.0.0.0/8".into(),
        ..Config::default()
    })
    .is_err());
}

#[tokio::test]
async fn negative_cache_uses_soa_and_fake_expiration_fails_closed() {
    let resolver = Resolver::new(Config::default()).unwrap();
    let mock = Mock {
        negative: true,
        ..Mock::default()
    };
    for _ in 0..2 {
        assert_eq!(
            answer(&resolver, &mock, "missing.example", RecordType::A)
                .await
                .metadata
                .response_code,
            ResponseCode::NXDomain
        );
    }
    assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
    let mut fake = Resolver::new(Config {
        fake_ip: true,
        fake_ip_ttl: 1,
        mapping_ttl_secs: 1,
        ..Config::default()
    })
    .unwrap();
    fake.reserve_address("198.18.0.2".parse().unwrap());
    fake.reserve_address("198.18.0.3".parse().unwrap());
    let ip = address(&answer(&fake, &mock, "expire.example", RecordType::A).await);
    assert_eq!(ip, "198.18.0.4".parse::<std::net::IpAddr>().unwrap());
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(fake.restore(ip).is_err());
}
