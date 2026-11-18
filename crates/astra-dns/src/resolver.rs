use super::{wire, Config, LocalMappingStore, MappingStore, Upstream};
use anyhow::{bail, Context, Result};
use hickory_proto::{
    op::{Message, ResponseCode},
    rr::{
        rdata::{A, AAAA},
        RData, Record, RecordType,
    },
};
use std::{
    collections::HashMap,
    future::Future,
    net::IpAddr,
    pin::Pin,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;

pub trait Transport: Send + Sync {
    fn exchange<'a>(
        &'a self,
        upstream: &'a Upstream,
        query: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>>> + Send + 'a>>;
}

impl Transport for &dyn Transport {
    fn exchange<'a>(
        &'a self,
        upstream: &'a Upstream,
        query: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>>> + Send + 'a>> {
        (**self).exchange(upstream, query)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LookupScope {
    Proxy,
    Direct,
    Endpoint,
}

enum MagicName<'a> {
    Missing,
    Records(&'a [IpAddr]),
}

impl MagicName<'_> {
    fn answer(&self, query: &Message, ipv6: bool) -> Message {
        let code = match self {
            Self::Missing => ResponseCode::NXDomain,
            Self::Records(_) => ResponseCode::NoError,
        };
        let mut reply = wire::response(query, code);
        reply.metadata.authoritative = true;
        if let Self::Records(addresses) = self {
            let q = &query.queries[0];
            for ip in *addresses {
                let data = match (q.query_type(), ip) {
                    (RecordType::A, IpAddr::V4(ip)) => RData::A(A(*ip)),
                    (RecordType::AAAA, IpAddr::V6(ip)) if ipv6 => RData::AAAA(AAAA(*ip)),
                    _ => continue,
                };
                reply
                    .answers
                    .push(Record::from_rdata(q.name().clone(), 60, data));
            }
        }
        reply
    }
}

pub struct Resolver {
    excluded: std::collections::HashSet<String>,
    reserved: std::collections::HashSet<IpAddr>,
    pub config: Config,
    mappings: Arc<dyn MappingStore>,
    cache: Arc<Mutex<crate::cache::Cache>>,
    maintenance: crate::maintenance::Maintenance,
    permits: Arc<Semaphore>,
    requests: Arc<Semaphore>,
    query_log: Arc<super::query_log::Store>,
    bootstrap: Mutex<HashMap<String, (IpAddr, Instant)>>,
}
impl Resolver {
    pub fn new(config: Config) -> Result<Self> {
        Self::with_mapping_store(config, Arc::new(LocalMappingStore::default()))
    }
    pub fn with_mapping_store(mut config: Config, mappings: Arc<dyn MappingStore>) -> Result<Self> {
        config.validate()?;
        let magic = &mut config.magic_dns;
        magic.domain = magic.domain.trim_end_matches('.').to_ascii_lowercase();
        magic.hosts = std::mem::take(&mut magic.hosts)
            .into_iter()
            .map(|(name, ips)| (name.to_ascii_lowercase(), ips))
            .collect();
        mappings.configure(&config, &Default::default())?;
        let excluded = config
            .proxy_nameserver
            .iter()
            .chain(&config.fallback)
            .chain(&config.direct_nameserver)
            .chain(config.nameserver_policy.values().flatten())
            .filter_map(|raw| {
                Upstream::parse(raw)
                    .ok()
                    .map(|u| u.host.to_ascii_lowercase())
            })
            .collect();
        Ok(Self {
            requests: Arc::new(Semaphore::new(config.max_concurrent)),
            reserved: Default::default(),
            excluded,
            permits: Arc::new(Semaphore::new(config.max_concurrent)),
            config,
            mappings,
            cache: Default::default(),
            maintenance: Default::default(),
            query_log: Default::default(),
            bootstrap: Default::default(),
        })
    }
    /// A reload keeps address ownership; changing fake ranges requires restart.
    pub fn inherit(&mut self, previous: &Self) -> Result<()> {
        if previous.config.enable && previous.config.fake_ip {
            anyhow::ensure!(
                self.config.enable
                    && self.config.fake_ip
                    && self.config.fake_ip_range == previous.config.fake_ip_range
                    && self.config.fake_ip_range6 == previous.config.fake_ip_range6,
                "changing/disabling active Fake IP ranges requires a proxy restart"
            );
        }
        let mut reserved = previous.reserved.clone();
        reserved.extend(&self.reserved);
        previous.mappings.configure(&self.config, &reserved)?;
        self.reserved = reserved;
        self.query_log = previous.query_log.clone();
        self.set_mapping_store(previous.mappings.clone())
    }
    /// Install a host-backed store before this resolver is shared with request
    /// handlers. Existing cached answers are discarded so observations cannot
    /// bypass the newly installed backend.
    pub fn set_mapping_store(&mut self, mappings: Arc<dyn MappingStore>) -> Result<()> {
        mappings.configure(&self.config, &self.reserved)?;
        self.mappings = mappings;
        self.cache
            .lock()
            .map_err(|_| anyhow::anyhow!("DNS cache is unavailable"))?
            .clear();
        Ok(())
    }
    pub fn mapping_store(&self) -> Arc<dyn MappingStore> {
        self.mappings.clone()
    }
    pub fn lookup_fake(&self, ip: IpAddr) -> Result<Option<String>> {
        if !self.config.enable || !self.config.fake_ip {
            return Ok(None);
        }
        self.mappings.lookup_fake(ip)
    }
    pub fn restore(&self, ip: IpAddr) -> Result<Option<String>> {
        self.mappings.restore(&self.config, ip)
    }
    pub fn exclude_domain(&mut self, domain: &str) {
        self.excluded
            .insert(domain.trim_end_matches('.').to_ascii_lowercase());
    }
    pub fn reserve_address(&mut self, address: IpAddr) {
        self.reserved.insert(address);
    }
    /// Reserve immediately in a shared store. The legacy reserve_address
    /// collects addresses validated by set_mapping_store/inherit/allocation.
    pub fn try_reserve_address(&mut self, address: IpAddr) -> Result<()> {
        let mut reserved = self.reserved.clone();
        reserved.insert(address);
        self.mappings.configure(&self.config, &reserved)?;
        self.reserved = reserved;
        Ok(())
    }
    pub fn is_fake(&self, ip: IpAddr) -> bool {
        self.config
            .active_fake_ranges()
            .any(|net| net.contains(&ip))
    }
    /// Cached bootstrap address of an upstream hostname, if still fresh.
    pub fn bootstrap_cached(&self, host: &str) -> Option<IpAddr> {
        let cache = self.bootstrap.lock().ok()?;
        cache
            .get(&host.to_ascii_lowercase())
            .filter(|(_, expires)| *expires > Instant::now())
            .map(|(ip, _)| *ip)
    }
    /// Remember an upstream hostname address; bounded by the configured
    /// upstream count and capped at five minutes.
    pub fn bootstrap_store(&self, host: &str, ip: IpAddr, ttl: u32) {
        let ttl = Duration::from_secs(u64::from(ttl.min(300)));
        if let Ok(mut cache) = self.bootstrap.lock() {
            let now = Instant::now();
            cache.retain(|_, (_, expires)| *expires > now);
            if cache.len() < 256 {
                cache.insert(host.to_ascii_lowercase(), (ip, now + ttl));
            }
        }
    }
    pub fn query_log(&self) -> astra_model::runtime::DnsQueryLog {
        self.query_log.snapshot()
    }
    pub async fn answer(&self, raw: &[u8], transport: &dyn Transport) -> Result<Vec<u8>> {
        self.answer_routed(raw, |_| {
            std::future::ready(Ok((LookupScope::Direct, transport)))
        })
        .await
    }

    pub async fn answer_routed<T: Transport, F: Future<Output = Result<(LookupScope, T)>>>(
        &self,
        raw: &[u8],
        select: impl FnOnce(String) -> F,
    ) -> Result<Vec<u8>> {
        let operation = astra_observe::operation!("dns.answer", "DNS 应答");
        let query = Message::from_vec(raw).map_err(Into::into);
        let mut trace = super::query_log::Query::new(&self.query_log, query.as_ref().ok());
        let result = match self.answer_inner(query, select, &mut trace).await {
            Ok(message) => {
                let result = message.to_vec().map_err(anyhow::Error::from);
                trace.finish(
                    result
                        .as_ref()
                        .map(|_| Some(&message))
                        .map_err(|e| e.to_string()),
                );
                result
            }
            Err(error) => {
                trace.finish(Err(error.to_string()));
                Err(error)
            }
        };
        operation.finish(result.is_ok());
        result
    }
    async fn answer_inner<T: Transport, F: Future<Output = Result<(LookupScope, T)>>>(
        &self,
        query: Result<Message>,
        select: impl FnOnce(String) -> F,
        trace: &mut super::query_log::Query,
    ) -> Result<Message> {
        let _request = self
            .requests
            .clone()
            .try_acquire_owned()
            .inspect_err(|_| {
                astra_observe::counter!("dns.request_limit_rejections", "DNS 请求并发上限拒绝")
                    .add(1);
            })
            .context("DNS concurrent request limit reached")?;
        let query = query?;
        wire::validate_query(&query)?;
        let q = &query.queries[0];
        let domain = q
            .name()
            .to_ascii()
            .trim_end_matches('.')
            .to_ascii_lowercase();
        if let Some(local) = self.magic_name(&domain) {
            trace.source("magic_dns");
            return Ok(local.answer(&query, self.config.ipv6));
        }
        if q.query_type() == RecordType::AAAA && !self.config.ipv6 {
            trace.source("ipv6_disabled");
            return Ok(wire::response(&query, ResponseCode::NoError));
        }
        if self.config.use_fake(&domain) && !self.excluded.contains(&domain) {
            // Prevent HTTPS/SVCB address hints bypassing Fake IP domain routing.
            if matches!(q.query_type(), RecordType::HTTPS | RecordType::SVCB) {
                trace.source("fake_ip");
                astra_observe::counter!(
                    "dns.fake_ip.suppressed_hints",
                    "Fake-IP 屏蔽 HTTPS/SVCB 地址提示"
                )
                .add(1);
                return Ok(wire::response(&query, ResponseCode::NoError));
            }
            if matches!(q.query_type(), RecordType::A | RecordType::AAAA) {
                trace.source("fake_ip");
                let mut reply = wire::response(&query, ResponseCode::NoError);
                if let Some(ip) = self.mappings.allocate(
                    &self.config,
                    &domain,
                    q.query_type() == RecordType::AAAA,
                    &self.reserved,
                )? {
                    let data = match ip {
                        IpAddr::V4(ip) => RData::A(A(ip)),
                        IpAddr::V6(ip) => RData::AAAA(AAAA(ip)),
                    };
                    reply.answers.push(Record::from_rdata(
                        q.name().clone(),
                        self.config.fake_ip_ttl,
                        data,
                    ));
                }
                astra_observe::counter!("dns.fake_ip.replies", "Fake-IP 应答").add(1);
                return Ok(reply);
            }
        }
        let (scope, transport) = select(domain).await?;
        self.real(&query, &transport, trace, scope).await
    }
    // Both the zone and the input to this function are canonical lowercase names.
    fn magic_name(&self, name: &str) -> Option<MagicName<'_>> {
        let magic = &self.config.magic_dns;
        if !magic.enable {
            return None;
        }
        if name == magic.domain {
            return Some(MagicName::Records(&[]));
        }
        if let Some(label) = name
            .strip_suffix(&magic.domain)
            .and_then(|s| s.strip_suffix('.'))
        {
            return Some(
                magic
                    .hosts
                    .get(label)
                    .map_or(MagicName::Missing, |ips| MagicName::Records(ips)),
            );
        }
        magic.hosts.get(name).map(|ips| MagicName::Records(ips))
    }

    /// Classify before using the OS resolver or bootstrapping an upstream.
    /// Some(Err) is a final local negative answer and must never fall through.
    pub fn lookup_magic(&self, name: &str) -> Option<Result<Vec<IpAddr>>> {
        if !self.config.magic_dns.enable {
            return None;
        }
        let normalized = name.trim_end_matches('.').to_ascii_lowercase();
        self.magic_name(&normalized).map(|entry| {
            let mut addresses = Vec::new();
            for kind in [RecordType::A, RecordType::AAAA] {
                if kind == RecordType::AAAA && !self.config.ipv6 {
                    continue;
                }
                let query = wire::query(name, kind)?;
                let reply = entry.answer(&query, self.config.ipv6);
                let mut trace = super::query_log::Query::new(&self.query_log, Some(&query));
                trace.source("magic_dns");
                trace.finish(Ok(Some(&reply)));
                if reply.metadata.response_code == ResponseCode::NXDomain {
                    bail!("unknown Magic DNS device {name}");
                }
                addresses.extend(wire::address_answers(&reply).map(|(ip, _)| ip));
            }
            anyhow::ensure!(
                !addresses.is_empty(),
                "Magic DNS device {name} has no usable addresses"
            );
            Ok(addresses)
        })
    }
    pub async fn lookup(&self, name: &str, transport: &dyn Transport) -> Result<Vec<IpAddr>> {
        self.lookup_with(name, transport, LookupScope::Proxy).await
    }
    /// Real answers from the dedicated direct list, with a separate cache scope.
    pub async fn lookup_proxy(&self, name: &str, transport: &dyn Transport) -> Result<Vec<IpAddr>> {
        self.lookup_with(name, transport, LookupScope::Endpoint)
            .await
    }
    pub async fn lookup_with(
        &self,
        name: &str,
        transport: &dyn Transport,
        scope: LookupScope,
    ) -> Result<Vec<IpAddr>> {
        if let Ok(ip) = name.parse() {
            return Ok(vec![ip]);
        }
        if let Some(local) = self.lookup_magic(name) {
            return local;
        }
        let mut addresses = Vec::new();
        let mut last = None;
        for kind in [RecordType::A, RecordType::AAAA] {
            if kind == RecordType::AAAA && !self.config.ipv6 {
                continue;
            }
            let query = wire::query(name, kind)?;
            let mut trace = super::query_log::Query::new(&self.query_log, Some(&query));
            let result = self.real(&query, transport, &mut trace, scope).await;
            trace.finish(result.as_ref().map(Some).map_err(|e| e.to_string()));
            match result {
                Ok(reply) => {
                    for (ip, _) in wire::address_answers(&reply) {
                        if !self.is_fake(ip) && !addresses.contains(&ip) {
                            addresses.push(ip);
                        }
                    }
                }
                Err(error) => last = Some(error),
            }
            if !addresses.is_empty() {
                break;
            }
        }
        if addresses.is_empty() {
            return Err(last.unwrap_or_else(|| anyhow::anyhow!("DNS lookup has no addresses")));
        }
        Ok(addresses)
    }
    async fn real(
        &self,
        query: &Message,
        transport: &dyn Transport,
        trace: &mut super::query_log::Query,
        scope: LookupScope,
    ) -> Result<Message> {
        let operation = astra_observe::operation!("dns.resolve", "DNS 真实解析（含缓存）");
        let result = self.real_inner(query, transport, trace, scope).await;
        operation.finish(result.is_ok());
        result
    }
    async fn real_inner(
        &self,
        query: &Message,
        transport: &dyn Transport,
        trace: &mut super::query_log::Query,
        scope: LookupScope,
    ) -> Result<Message> {
        let mut normalized = query.clone();
        normalized.metadata.id = 0;
        let mut key = normalized.to_vec()?;
        key.push(scope as u8);
        self.maintenance.start(&self.cache);
        let cached = self.cache.lock().unwrap().get(&key, Instant::now());
        if let Some((wire, age)) = cached {
            astra_observe::counter!("dns.cache.hits", "DNS 缓存命中").add(1);
            trace.source("cache");
            let mut message = Message::from_vec(&wire)?;
            message.metadata.id = query.metadata.id;
            for record in message
                .answers
                .iter_mut()
                .chain(&mut message.authorities)
                .chain(&mut message.additionals)
            {
                record.ttl = record.ttl.saturating_sub(age);
            }
            return Ok(message);
        }
        astra_observe::counter!("dns.cache.misses", "DNS 缓存未命中").add(1);
        let _permit = self
            .permits
            .clone()
            .try_acquire_owned()
            .inspect_err(|_| {
                astra_observe::counter!("dns.query_limit_rejections", "DNS 上游查询并发上限拒绝")
                    .add(1);
            })
            .context("DNS concurrent query limit reached")?;
        let domain = query.queries[0]
            .name()
            .to_ascii()
            .trim_end_matches('.')
            .to_ascii_lowercase();
        let mut last = None;
        // Each upstream gets its own deadline; the entire ordered failover list
        // is also bounded so an unreachable list cannot multiply user wait time.
        let exchange = async {
            let servers = if scope == LookupScope::Endpoint {
                self.config.direct_nameserver.as_slice()
            } else {
                self.config.servers(&domain, scope == LookupScope::Direct)
            };
            let upstreams = servers.iter().chain(&self.config.fallback);
            let deadline = tokio::time::Instant::now()
                + Duration::from_millis(self.config.query_timeout_ms * 2);
            let count = servers.len() + self.config.fallback.len();
            for (index, raw) in upstreams.enumerate() {
                let upstream = Upstream::parse(raw)?;
                // Defense in depth for configs built without validate(): an
                // encrypted-only resolver never downgrades to plaintext.
                if !upstream.is_encrypted() && !self.config.plaintext_allowed(&domain) {
                    astra_observe::counter!(
                        "dns.upstream.plaintext_blocked",
                        "DNS 明文上游被加密策略拦截"
                    )
                    .add(1);
                    last = Some(anyhow::anyhow!(
                        "plaintext DNS upstream {raw} blocked by encrypted-only policy"
                    ));
                    continue;
                }
                trace.upstream(raw);
                let packet = query.to_vec()?;
                let attempt = astra_observe::operation!("dns.upstream", "DNS 上游交换");
                let result = match tokio::time::timeout(
                    Duration::from_millis(self.config.query_timeout_ms).min(
                        deadline.saturating_duration_since(tokio::time::Instant::now())
                            / (count - index) as u32,
                    ),
                    transport.exchange(&upstream, packet),
                )
                .await
                {
                    Ok(result) => result.and_then(|bytes| {
                        let message = Message::from_vec(&bytes)?;
                        wire::validate_response(query, &message)?;
                        anyhow::ensure!(
                            !matches!(
                                message.metadata.response_code,
                                ResponseCode::ServFail | ResponseCode::Refused
                            ),
                            "upstream returned SERVFAIL/REFUSED"
                        );
                        Ok(message)
                    }),
                    Err(_) => {
                        astra_observe::counter!("dns.upstream.timeouts", "DNS 上游超时").add(1);
                        Err(anyhow::anyhow!("DNS upstream timed out"))
                    }
                };
                attempt.finish(result.is_ok());
                match result {
                    Ok(message) => return Ok(message),
                    Err(error) => last = Some(error),
                }
            }
            Err(last.unwrap_or_else(|| anyhow::anyhow!("no usable DNS upstream")))
        };
        let mut message = tokio::time::timeout(
            Duration::from_millis(self.config.query_timeout_ms * 2),
            exchange,
        )
        .await
        .inspect_err(|_| {
            astra_observe::counter!("dns.query_deadline_exceeded", "DNS 解析总截止时间超时").add(1);
        })
        .context("DNS query deadline exceeded")??;
        if message.metadata.truncation {
            bail!("truncated upstream response after TCP fallback");
        }
        let mut names = std::collections::HashSet::from([query.queries[0].name().clone()]);
        for _ in 0..16 {
            let before = names.len();
            for record in &message.answers {
                if names.contains(&record.name) {
                    if let RData::CNAME(cname) = &record.data {
                        names.insert(cname.0.clone());
                    }
                }
            }
            if names.len() == before {
                break;
            }
        }
        let chain_ttl = message
            .answers
            .iter()
            .map(|record| record.ttl)
            .min()
            .unwrap_or(0);
        for record in &message.answers {
            let ip = match record.data {
                RData::A(A(ip)) => IpAddr::V4(ip),
                RData::AAAA(AAAA(ip)) => IpAddr::V6(ip),
                _ => continue,
            };
            if names.contains(&record.name) {
                self.mappings
                    .observe(&self.config, &domain, ip, record.ttl.min(chain_ttl))?;
            }
        }
        let ttl = if message.answers.is_empty()
            || message.metadata.response_code == ResponseCode::NXDomain
        {
            message
                .authorities
                .iter()
                .filter_map(|record| {
                    if let RData::SOA(soa) = &record.data {
                        Some(
                            record
                                .ttl
                                .min(soa.minimum)
                                .min(self.config.negative_ttl_secs),
                        )
                    } else {
                        None
                    }
                })
                .min()
                .unwrap_or(0)
        } else {
            message
                .answers
                .iter()
                .chain(&message.authorities)
                .chain(&message.additionals)
                .map(|record| record.ttl)
                .min()
                .unwrap_or(0)
        }
        .min(self.config.cache_max_ttl_secs);
        for record in message
            .answers
            .iter_mut()
            .chain(&mut message.authorities)
            .chain(&mut message.additionals)
        {
            record.ttl = record.ttl.min(self.config.cache_max_ttl_secs);
        }
        if ttl > 0
            && self.config.cache_capacity > 0
            && matches!(
                message.metadata.response_code,
                ResponseCode::NoError | ResponseCode::NXDomain
            )
        {
            let wire = message.to_vec()?;
            self.cache.lock().unwrap().insert(
                key,
                wire,
                ttl,
                self.config.cache_capacity,
                Instant::now(),
            );
        }
        Ok(message)
    }
}

#[cfg(test)]
#[path = "test/resolver_monitor_test.rs"]
mod monitor_tests;
