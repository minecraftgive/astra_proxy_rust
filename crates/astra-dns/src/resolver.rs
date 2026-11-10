use super::{mapping::Mappings, wire, Config, LocalMappingStore, MappingStore, Upstream};
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

struct Cached {
    message: Message,
    inserted: Instant,
    ttl: u32,
    bytes: usize,
}
pub struct Resolver {
    excluded: std::collections::HashSet<String>,
    reserved: std::collections::HashSet<IpAddr>,
    pub config: Config,
    mappings: Arc<dyn MappingStore>,
    cache: Mutex<HashMap<Vec<u8>, Cached>>,
    permits: Arc<Semaphore>,
    requests: Arc<Semaphore>,
    query_log: Arc<super::query_log::Store>,
    bootstrap: Mutex<HashMap<String, (IpAddr, Instant)>>,
}
impl Resolver {
    pub fn new(config: Config) -> Result<Self> {
        Self::with_mapping_store(config, Arc::new(LocalMappingStore::default()))
    }
    pub fn with_mapping_store(config: Config, mappings: Arc<dyn MappingStore>) -> Result<Self> {
        config.validate()?;
        mappings.configure(&config, &Default::default())?;
        let excluded = config
            .nameserver
            .iter()
            .chain(&config.fallback)
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
            cache: Mutex::new(HashMap::new()),
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
            .get_mut()
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
        self.config.fake_ip && Mappings::contains_fake(&self.config, ip)
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
        let ttl = Duration::from_secs(u64::from(ttl.clamp(1, 300)));
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
        let operation = astra_observe::operation!("dns.answer", "DNS 应答");
        let mut trace =
            super::query_log::Query::new(&self.query_log, Message::from_vec(raw).ok().as_ref());
        let result = self.answer_inner(raw, transport, &mut trace).await;
        trace.finish(
            result
                .as_ref()
                .map(|raw| Message::from_vec(raw).ok())
                .map_err(|e| e.to_string()),
        );
        operation.finish(result.is_ok());
        result
    }
    async fn answer_inner(
        &self,
        raw: &[u8],
        transport: &dyn Transport,
        trace: &mut super::query_log::Query,
    ) -> Result<Vec<u8>> {
        let _request = self
            .requests
            .clone()
            .try_acquire_owned()
            .inspect_err(|_| {
                astra_observe::counter!("dns.request_limit_rejections", "DNS 请求并发上限拒绝")
                    .add(1);
            })
            .context("DNS concurrent request limit reached")?;
        let query = Message::from_vec(raw)?;
        wire::validate_query(&query)?;
        let q = &query.queries[0];
        let domain = q
            .name()
            .to_ascii()
            .trim_end_matches('.')
            .to_ascii_lowercase();
        if q.query_type() == RecordType::AAAA && !self.config.ipv6 {
            trace.source("ipv6_disabled");
            return Ok(wire::response(&query, ResponseCode::NoError).to_vec()?);
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
                return Ok(wire::response(&query, ResponseCode::NoError).to_vec()?);
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
                let bytes = reply.to_vec()?;
                astra_observe::counter!("dns.fake_ip.replies", "Fake-IP 应答").add(1);
                return Ok(bytes);
            }
        }
        Ok(self.real(&query, transport, trace).await?.to_vec()?)
    }
    pub async fn lookup(&self, name: &str, transport: &dyn Transport) -> Result<Vec<IpAddr>> {
        if let Ok(ip) = name.parse() {
            return Ok(vec![ip]);
        }
        let mut addresses = Vec::new();
        let mut last = None;
        for kind in [RecordType::A, RecordType::AAAA] {
            if kind == RecordType::AAAA && !self.config.ipv6 {
                continue;
            }
            let query = wire::query(name, kind)?;
            let mut trace = super::query_log::Query::new(&self.query_log, Some(&query));
            let result = self.real(&query, transport, &mut trace).await;
            trace.finish(
                result
                    .as_ref()
                    .map(|reply| Some(reply.clone()))
                    .map_err(|e| e.to_string()),
            );
            match result {
                Ok(reply) => {
                    for record in reply.answers {
                        let ip = match record.data {
                            RData::A(A(ip)) => IpAddr::V4(ip),
                            RData::AAAA(AAAA(ip)) => IpAddr::V6(ip),
                            _ => continue,
                        };
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
    ) -> Result<Message> {
        let operation = astra_observe::operation!("dns.resolve", "DNS 真实解析（含缓存）");
        let result = self.real_inner(query, transport, trace).await;
        operation.finish(result.is_ok());
        result
    }
    async fn real_inner(
        &self,
        query: &Message,
        transport: &dyn Transport,
        trace: &mut super::query_log::Query,
    ) -> Result<Message> {
        let mut normalized = query.clone();
        normalized.metadata.id = 0;
        let key = normalized.to_vec()?;
        {
            let mut cache = self.cache.lock().unwrap();
            if let Some(entry) = cache.get(&key) {
                let age = entry.inserted.elapsed().as_secs();
                if age < entry.ttl as u64 {
                    astra_observe::counter!("dns.cache.hits", "DNS 缓存命中").add(1);
                    trace.source("cache");
                    let mut message = entry.message.clone();
                    message.metadata.id = query.metadata.id;
                    for record in message
                        .answers
                        .iter_mut()
                        .chain(&mut message.authorities)
                        .chain(&mut message.additionals)
                    {
                        record.ttl = record.ttl.saturating_sub(age as u32);
                    }
                    return Ok(message);
                }
            }
            if cache.remove(&key).is_some() {
                astra_observe::counter!("dns.cache.expired", "DNS 缓存过期移除").add(1);
            }
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
            for raw in self
                .config
                .servers(&domain)
                .iter()
                .chain(&self.config.fallback)
            {
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
                    Duration::from_millis(self.config.query_timeout_ms),
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
            let mut cache = self.cache.lock().unwrap();
            let before = cache.len();
            cache.retain(|_, entry| entry.inserted.elapsed().as_secs() < entry.ttl as u64);
            astra_observe::counter!("dns.cache.expired", "DNS 缓存过期移除")
                .add((before - cache.len()) as u64);
            let bytes = key.len() + message.to_vec()?.len();
            while cache.len() >= self.config.cache_capacity
                || cache.values().map(|entry| entry.bytes).sum::<usize>() + bytes > 16 * 1024 * 1024
            {
                if let Some(oldest) = cache
                    .iter()
                    .min_by_key(|(_, entry)| entry.inserted)
                    .map(|(key, _)| key.clone())
                {
                    cache.remove(&oldest);
                    astra_observe::counter!("dns.cache.evictions", "DNS 缓存容量淘汰").add(1);
                } else {
                    break;
                }
            }
            cache.insert(
                key,
                Cached {
                    message: message.clone(),
                    inserted: Instant::now(),
                    ttl,
                    bytes,
                },
            );
        }
        Ok(message)
    }
}

#[cfg(test)]
#[path = "test/resolver_monitor_test.rs"]
mod monitor_tests;
