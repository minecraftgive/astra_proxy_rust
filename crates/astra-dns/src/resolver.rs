use super::{mapping::Mappings, wire, Config, Upstream};
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
    mappings: Arc<Mutex<Mappings>>,
    cache: Mutex<HashMap<Vec<u8>, Cached>>,
    permits: Arc<Semaphore>,
    requests: Arc<Semaphore>,
}
impl Resolver {
    pub fn new(config: Config) -> Result<Self> {
        config.validate()?;
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
            mappings: Arc::new(Mutex::new(Mappings::default())),
            cache: Mutex::new(HashMap::new()),
        })
    }
    /// A reload keeps address ownership; changing fake ranges requires restart.
    pub fn inherit(&mut self, previous: &Self) -> Result<()> {
        for ip in &self.reserved {
            anyhow::ensure!(
                !(previous.is_fake(*ip) && previous.restore(*ip).ok().flatten().is_some()),
                "TUN address/subnet endpoint overlaps an active Fake IP mapping; restart proxy"
            );
        }
        if previous.config.enable && previous.config.fake_ip {
            anyhow::ensure!(
                self.config.enable
                    && self.config.fake_ip
                    && self.config.fake_ip_range == previous.config.fake_ip_range
                    && self.config.fake_ip_range6 == previous.config.fake_ip_range6,
                "changing/disabling active Fake IP ranges requires a proxy restart"
            );
        }
        self.mappings = previous.mappings.clone();
        Ok(())
    }
    pub fn restore(&self, ip: IpAddr) -> Result<Option<String>> {
        self.mappings.lock().unwrap().reverse(&self.config, ip)
    }
    pub fn exclude_domain(&mut self, domain: &str) {
        self.excluded
            .insert(domain.trim_end_matches('.').to_ascii_lowercase());
    }
    pub fn reserve_address(&mut self, address: IpAddr) {
        self.reserved.insert(address);
    }
    pub fn is_fake(&self, ip: IpAddr) -> bool {
        self.config.fake_ip && Mappings::contains_fake(&self.config, ip)
    }
    pub async fn answer(&self, raw: &[u8], transport: &dyn Transport) -> Result<Vec<u8>> {
        let _request = self
            .requests
            .clone()
            .try_acquire_owned()
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
            return Ok(wire::response(&query, ResponseCode::NoError).to_vec()?);
        }
        if self.config.use_fake(&domain) && !self.excluded.contains(&domain) {
            // Prevent HTTPS/SVCB address hints bypassing Fake IP domain routing.
            if matches!(q.query_type(), RecordType::HTTPS | RecordType::SVCB) {
                return Ok(wire::response(&query, ResponseCode::NoError).to_vec()?);
            }
            if matches!(q.query_type(), RecordType::A | RecordType::AAAA) {
                let mut reply = wire::response(&query, ResponseCode::NoError);
                if let Some(ip) = self.mappings.lock().unwrap().allocate(
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
                return Ok(reply.to_vec()?);
            }
        }
        Ok(self.real(&query, transport).await?.to_vec()?)
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
            match self.real(&wire::query(name, kind)?, transport).await {
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
    async fn real(&self, query: &Message, transport: &dyn Transport) -> Result<Message> {
        let mut normalized = query.clone();
        normalized.metadata.id = 0;
        let key = normalized.to_vec()?;
        {
            let mut cache = self.cache.lock().unwrap();
            if let Some(entry) = cache.get(&key) {
                let age = entry.inserted.elapsed().as_secs();
                if age < entry.ttl as u64 {
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
            cache.remove(&key);
        }
        let _permit = self
            .permits
            .clone()
            .try_acquire_owned()
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
                match tokio::time::timeout(
                    Duration::from_millis(self.config.query_timeout_ms),
                    transport.exchange(&upstream, query.to_vec()?),
                )
                .await
                {
                    Ok(Ok(bytes)) => match Message::from_vec(&bytes)
                        .map_err(anyhow::Error::from)
                        .and_then(|message| {
                            wire::validate_response(query, &message)?;
                            Ok(message)
                        }) {
                        Ok(message)
                            if !matches!(
                                message.metadata.response_code,
                                ResponseCode::ServFail | ResponseCode::Refused
                            ) =>
                        {
                            return Ok(message)
                        }
                        Ok(_) => last = Some(anyhow::anyhow!("upstream returned SERVFAIL/REFUSED")),
                        Err(error) => last = Some(error),
                    },
                    Ok(Err(error)) => last = Some(error),
                    Err(_) => last = Some(anyhow::anyhow!("DNS upstream timed out")),
                }
            }
            Err(last.unwrap_or_else(|| anyhow::anyhow!("no usable DNS upstream")))
        };
        let mut message = tokio::time::timeout(
            Duration::from_millis(self.config.query_timeout_ms * 2),
            exchange,
        )
        .await
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
        let mut mappings = self.mappings.lock().unwrap();
        for record in &message.answers {
            let ip = match record.data {
                RData::A(A(ip)) => IpAddr::V4(ip),
                RData::AAAA(AAAA(ip)) => IpAddr::V6(ip),
                _ => continue,
            };
            if names.contains(&record.name) {
                mappings.observe(&self.config, &domain, ip, record.ttl.min(chain_ttl));
            }
        }
        drop(mappings);
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
            cache.retain(|_, entry| entry.inserted.elapsed().as_secs() < entry.ttl as u64);
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
