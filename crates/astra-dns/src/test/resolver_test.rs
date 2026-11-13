use anyhow::Result;
use astra_dns::{wire, Config, LocalMappingStore, MappingStore, Resolver, Transport, Upstream};
use hickory_proto::{
    op::{Message, ResponseCode},
    rr::{rdata::A, RData, Record, RecordType},
};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
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

#[test]
fn proxy_dns_requires_independent_bootstrap_and_obeys_encryption_and_loop_checks() {
    let mut config = Config::default();
    config.proxy_server_nameserver = vec!["https://dns.example/dns-query".into()];
    assert!(config.validate().is_err());
    config
        .proxy_server_nameserver
        .push("https://192.0.2.53/dns-query".into());
    config.nameserver = vec!["https://other.example/dns-query".into()];
    config.validate().unwrap();
    config.proxy_server_nameserver = vec!["udp://192.0.2.53".into()];
    assert!(config.validate().is_err());
    config.plaintext_bypass = true;
    config.validate().unwrap();
    config.proxy_server_nameserver = vec!["udp://127.0.0.1:1053".into()];
    assert!(
        config.validate().is_err(),
        "must not point back to its own DNS listener"
    );
}

#[tokio::test]
async fn proxy_dns_ignores_business_policy_has_separate_cache_and_never_uses_regular_fallback() {
    let config = Config {
        proxy_server_nameserver: vec!["tls://192.0.2.1".into(), "tls://192.0.2.53".into()],
        nameserver_policy: [("node.example".into(), vec!["tls://192.0.2.99".into()])]
            .into_iter()
            .collect(),
        ipv6: false,
        ..Default::default()
    };
    let resolver = Resolver::new(config.clone()).unwrap();
    let mock = Mock {
        ttl: 60,
        ..Default::default()
    };
    resolver.lookup("node.example", &mock).await.unwrap();
    resolver.lookup_proxy("node.example", &mock).await.unwrap();
    resolver.lookup_proxy("node.example", &mock).await.unwrap();
    resolver.lookup("node.example", &mock).await.unwrap();
    assert_eq!(
        *mock.servers.lock().unwrap(),
        ["192.0.2.99", "192.0.2.1", "192.0.2.53"]
    );
    let mut failed = config;
    failed.proxy_server_nameserver.pop();
    failed.fallback = vec!["tls://192.0.2.100".into()];
    let resolver = Resolver::new(failed).unwrap();
    let mock = Mock::default();
    assert!(resolver.lookup_proxy("node.example", &mock).await.is_err());
    assert_eq!(*mock.servers.lock().unwrap(), ["192.0.2.1"]);
}

#[tokio::test]
async fn shared_mapping_store_preserves_ownership_and_reservations_across_generations() {
    let config = Config {
        fake_ip: true,
        fake_ip_range6: Some("fdab::/64".into()),
        mapping_capacity: 4,
        ..Config::default()
    };
    let host = Arc::new(LocalMappingStore::default());
    let mut old = Resolver::with_mapping_store(config.clone(), host.clone()).unwrap();
    // Probe through more reservations than the old capacity+3 loop allowed.
    for offset in 2..=10 {
        old.try_reserve_address(format!("198.18.0.{offset}").parse().unwrap())
            .unwrap();
    }
    let mut new = Resolver::with_mapping_store(config.clone(), host.clone()).unwrap();
    new.try_reserve_address("198.18.0.11".parse().unwrap())
        .unwrap();
    let mock = Mock::default();
    let first = address(&answer(&old, &mock, "old.example", RecordType::A).await);
    assert_eq!(first, "198.18.0.12".parse::<std::net::IpAddr>().unwrap());
    assert_eq!(new.restore(first).unwrap().as_deref(), Some("old.example"));
    assert_eq!(
        first,
        address(&answer(&new, &mock, "old.example", RecordType::A).await)
    );
    let second = address(&answer(&new, &mock, "new.example", RecordType::A).await);
    assert_ne!(first, second);
    assert_eq!(old.restore(second).unwrap().as_deref(), Some("new.example"));
    let ipv6 = address(&answer(&old, &mock, "old.example", RecordType::AAAA).await);
    assert_eq!(new.restore(ipv6).unwrap().as_deref(), Some("old.example"));
    assert!(new.try_reserve_address(first).is_err());
    let changed = Config {
        fake_ip_range: "198.19.0.0/16".into(),
        ..config.clone()
    };
    assert!(Resolver::with_mapping_store(changed, host.clone()).is_err());
    // A resolver created before Fake-IP was enabled may still observe real
    // answers, but an inactive configuration cannot allocate fake addresses.
    let inactive = Config {
        fake_ip: false,
        ..config
    };
    assert!(Resolver::with_mapping_store(inactive.clone(), host.clone()).is_ok());
    assert!(host
        .allocate(&inactive, "disabled.example", false, &Default::default())
        .is_err());
    // Failed registration did not change existing address ownership.
    assert_eq!(new.restore(first).unwrap().as_deref(), Some("old.example"));
}

#[tokio::test]
async fn shared_real_observations_retain_ambiguity() {
    let host = Arc::new(LocalMappingStore::default());
    let old = Resolver::with_mapping_store(Config::default(), host.clone()).unwrap();
    let new = Resolver::with_mapping_store(Config::default(), host).unwrap();
    let mock = Mock {
        ttl: 30,
        ..Default::default()
    };
    let ip = address(&answer(&old, &mock, "one.example", RecordType::A).await);
    assert_eq!(new.restore(ip).unwrap().as_deref(), Some("one.example"));
    answer(&new, &mock, "two.example", RecordType::A).await;
    assert_eq!(old.restore(ip).unwrap(), None);
}

#[tokio::test]
async fn enabling_fake_ip_does_not_break_the_previous_real_only_resolver() {
    let old = Resolver::new(Config::default()).unwrap();
    let mut new = Resolver::new(Config {
        fake_ip: true,
        ..Config::default()
    })
    .unwrap();
    new.inherit(&old).unwrap();
    let mock = Mock {
        ttl: 30,
        ..Default::default()
    };
    let real = address(&answer(&old, &mock, "existing.example", RecordType::A).await);
    assert_eq!(
        old.restore(real).unwrap().as_deref(),
        Some("existing.example")
    );
    assert_eq!(
        new.restore(real).unwrap().as_deref(),
        Some("existing.example")
    );
    let fake = address(&answer(&new, &mock, "new.example", RecordType::A).await);
    assert!(new.is_fake(fake));
    let mut disabled = Resolver::new(Config::default()).unwrap();
    assert!(disabled.inherit(&new).is_err());
}

#[derive(Default)]
struct RemoteMappings {
    host: LocalMappingStore,
    calls: [AtomicUsize; 4],
    fail: std::sync::atomic::AtomicBool,
}
impl RemoteMappings {
    fn called(&self, operation: usize) -> Result<()> {
        self.calls[operation].fetch_add(1, Ordering::SeqCst);
        anyhow::ensure!(
            !self.fail.load(Ordering::SeqCst),
            "simulated host RPC failure"
        );
        Ok(())
    }
}
impl MappingStore for RemoteMappings {
    fn configure(
        &self,
        config: &Config,
        reserved: &std::collections::HashSet<std::net::IpAddr>,
    ) -> Result<()> {
        self.called(0)?;
        self.host.configure(config, reserved)
    }
    fn allocate(
        &self,
        config: &Config,
        domain: &str,
        ipv6: bool,
        reserved: &std::collections::HashSet<std::net::IpAddr>,
    ) -> Result<Option<std::net::IpAddr>> {
        self.called(1)?;
        self.host.allocate(config, domain, ipv6, reserved)
    }
    fn restore(&self, config: &Config, ip: std::net::IpAddr) -> Result<Option<String>> {
        self.called(2)?;
        self.host.restore(config, ip)
    }
    fn observe(&self, config: &Config, domain: &str, ip: std::net::IpAddr, ttl: u32) -> Result<()> {
        self.called(3)?;
        self.host.observe(config, domain, ip, ttl)
    }
}

#[tokio::test]
async fn injected_store_receives_every_mapping_operation_and_fails_closed() {
    let config = Config {
        fake_ip: true,
        ..Config::default()
    };
    let remote = Arc::new(RemoteMappings::default());
    let mut resolver = Resolver::new(config.clone()).unwrap();
    resolver.reserve_address("198.18.0.2".parse().unwrap());
    resolver.set_mapping_store(remote.clone()).unwrap();
    let mock = Mock {
        ttl: 30,
        ..Default::default()
    };
    let fake = address(&answer(&resolver, &mock, "one.example", RecordType::A).await);
    assert_eq!(fake, "198.18.0.3".parse::<std::net::IpAddr>().unwrap());
    assert_eq!(
        resolver.restore(fake).unwrap().as_deref(),
        Some("one.example")
    );
    let real = address(&answer(&resolver, &mock, "printer.lan", RecordType::A).await);
    assert_eq!(
        resolver.restore(real).unwrap().as_deref(),
        Some("printer.lan")
    );
    assert!(remote
        .calls
        .iter()
        .all(|count| count.load(Ordering::SeqCst) > 0));
    remote.fail.store(true, Ordering::SeqCst);
    assert!(resolver.restore(fake).is_err());
    for name in ["other.example", "another.lan"] {
        assert!(resolver
            .answer(
                &wire::query(name, RecordType::A).unwrap().to_vec().unwrap(),
                &mock
            )
            .await
            .is_err());
    }
    let mut replacement = Resolver::new(config).unwrap();
    assert!(replacement.inherit(&resolver).is_err());
    remote.fail.store(false, Ordering::SeqCst);
    replacement.inherit(&resolver).unwrap();
    assert!(Arc::ptr_eq(
        &resolver.mapping_store(),
        &replacement.mapping_store()
    ));
    assert_eq!(
        replacement.restore(fake).unwrap().as_deref(),
        Some("one.example")
    );
}

#[tokio::test]
async fn changing_backend_invalidates_cached_real_answers() {
    let mut resolver = Resolver::new(Config::default()).unwrap();
    let mock = Mock {
        ttl: 30,
        ..Default::default()
    };
    answer(&resolver, &mock, "cached.example", RecordType::A).await;
    let remote = Arc::new(RemoteMappings::default());
    resolver.set_mapping_store(remote.clone()).unwrap();
    let ip = address(&answer(&resolver, &mock, "cached.example", RecordType::A).await);
    assert_eq!(mock.calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        remote.restore(&Config::default(), ip).unwrap().as_deref(),
        Some("cached.example")
    );
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
    config.plaintext_bypass_domains = vec!["+.special.example".into()];
    config.fallback = vec!["tls://192.0.2.2:5353".into()];
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

#[test]
fn encrypted_only_is_default_and_plaintext_needs_explicit_bypass() {
    let defaults = Config::default();
    defaults.validate().unwrap();
    assert!(!defaults.plaintext_bypass);
    assert!(defaults
        .nameserver
        .iter()
        .all(|raw| Upstream::parse(raw).unwrap().is_encrypted()));
    let with = |edit: fn(&mut Config)| {
        let mut config = Config::default();
        edit(&mut config);
        config.validate()
    };
    let error = with(|c| c.nameserver = vec!["udp://1.1.1.1:53".into()]).unwrap_err();
    assert!(error.to_string().contains("plaintext_bypass"), "{error}");
    assert!(with(|c| c.nameserver = vec!["8.8.8.8".into()]).is_err());
    assert!(with(|c| c.fallback = vec!["tcp://8.8.8.8:53".into()]).is_err());
    with(|c| {
        c.plaintext_bypass = true;
        c.nameserver = vec!["udp://1.1.1.1:53".into()];
        c.fallback = vec!["tcp://8.8.8.8".into()];
    })
    .unwrap();
    // Hostname upstreams need an IP-literal encrypted upstream to bootstrap.
    assert!(with(|c| c.nameserver = vec!["https://dns.example/dns-query".into()]).is_err());
    with(|c| {
        c.nameserver = vec!["https://dns.example/dns-query".into()];
        c.fallback = vec!["tls://1.1.1.1".into()];
    })
    .unwrap();
    assert!(with(|c| c.plaintext_bypass_domains = vec!["bad domain".into()]).is_err());

    // Policy plaintext is allowed only where a bypass pattern covers it.
    let policy = |pattern: &str, bypass: &[&str]| {
        let mut config = Config::default();
        config
            .nameserver_policy
            .insert(pattern.into(), vec!["udp://192.168.1.1:53".into()]);
        config.plaintext_bypass_domains = bypass.iter().map(|b| (*b).to_owned()).collect();
        config.validate().is_ok()
    };
    assert!(!policy("+.lan", &[]));
    assert!(policy("+.lan", &["+.lan"]));
    assert!(policy("*.corp.lan", &["+.lan"]));
    assert!(policy("router.lan", &["*.lan"]));
    assert!(policy("router.lan", &["router.lan"]));
    assert!(
        !policy("+.lan", &["*.lan"]),
        "*.lan does not cover lan itself"
    );
    assert!(!policy("+.lan", &["router.lan"]));
    assert!(!policy("*.lan", &["a.lan"]));
    assert!(!policy("*.example", &["+.a.example"]));
    assert!(!policy("+.lan", &["+.local"]));
    // A policy may still mix in encrypted upstreams without any bypass.
    let mut config = Config::default();
    config
        .nameserver_policy
        .insert("+.example.org".into(), vec!["tls://9.9.9.9".into()]);
    config.validate().unwrap();
}
#[tokio::test]
async fn bypass_domains_use_plaintext_policy_while_other_names_stay_encrypted() {
    let mut config = Config {
        nameserver: vec!["https://1.1.1.1/dns-query".into()],
        ..Config::default()
    };
    config
        .nameserver_policy
        .insert("+.lan".into(), vec!["udp://192.168.1.10:53".into()]);
    config.plaintext_bypass_domains = vec!["+.lan".into()];
    let resolver = Resolver::new(config).unwrap();
    let mock = Mock {
        ttl: 30,
        ..Mock::default()
    };
    answer(&resolver, &mock, "printer.lan", RecordType::A).await;
    answer(&resolver, &mock, "public.example", RecordType::A).await;
    assert_eq!(&*mock.servers.lock().unwrap(), &["192.168.1.10", "1.1.1.1"]);
    assert!(resolver.config.plaintext_allowed("printer.lan"));
    assert!(!resolver.config.plaintext_allowed("public.example"));
}
#[test]
fn bootstrap_cache_is_case_insensitive_and_expires() {
    let resolver = Resolver::new(Config::default()).unwrap();
    let ip: std::net::IpAddr = "192.0.2.53".parse().unwrap();
    assert_eq!(resolver.bootstrap_cached("dns.example"), None);
    resolver.bootstrap_store("DNS.example", ip, 1);
    assert_eq!(resolver.bootstrap_cached("dns.EXAMPLE"), Some(ip));
    std::thread::sleep(Duration::from_millis(1100));
    assert_eq!(resolver.bootstrap_cached("dns.example"), None);
}
