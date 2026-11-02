use super::Config;
use anyhow::{bail, Result};
use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    sync::Mutex,
    time::{Duration, Instant},
};

/// Address ownership shared by resolvers or supplied by a host RPC adapter.
/// Implementations must fail closed on communication errors. In particular,
/// a worker must never allocate from a local fallback after a host failure.
/// Calls are synchronous, including from async DNS handlers; RPC adapters must
/// bound their waits and avoid depending on work on the calling executor.
pub trait MappingStore: Send + Sync {
    /// Register configuration and reserved addresses before the resolver serves
    /// traffic. Reservations accumulate for the lifetime of the shared store.
    fn configure(&self, config: &Config, reserved: &HashSet<IpAddr>) -> Result<()>;
    fn allocate(
        &self,
        config: &Config,
        domain: &str,
        ipv6: bool,
        reserved: &HashSet<IpAddr>,
    ) -> Result<Option<IpAddr>>;
    fn restore(&self, config: &Config, ip: IpAddr) -> Result<Option<String>>;
    fn observe(&self, config: &Config, domain: &str, ip: IpAddr, ttl: u32) -> Result<()>;
}

/// The default implementation also serves as the host's authoritative store.
/// Export one Arc to all local resolvers and dispatch worker RPCs to the same
/// object; no mapping tables need to be serialized across worker generations.
#[derive(Default)]
pub struct LocalMappingStore {
    state: Mutex<SharedMappings>,
}

#[derive(Default)]
struct SharedMappings {
    mappings: Mappings,
    fake_ranges: Option<(String, Option<String>)>,
    reserved: HashSet<IpAddr>,
    validated: Option<Config>,
}

impl SharedMappings {
    fn configure(&mut self, config: &Config, reserved: &HashSet<IpAddr>) -> Result<()> {
        if self.validated.as_ref() != Some(config) {
            config.validate()?;
        }
        if let Some((range4, range6)) = &self.fake_ranges {
            anyhow::ensure!(
                !(config.enable && config.fake_ip)
                    || (&config.fake_ip_range == range4 && &config.fake_ip_range6 == range6),
                "changing shared Fake IP ranges requires host maintenance"
            );
        }
        let now = Instant::now();
        for address in reserved {
            anyhow::ensure!(
                !self
                    .mappings
                    .fake
                    .get(address)
                    .is_some_and(|entry| entry.expires > now),
                "TUN address/subnet endpoint overlaps an active Fake IP mapping; restart proxy"
            );
        }
        if config.enable && config.fake_ip && self.fake_ranges.is_none() {
            self.fake_ranges = Some((config.fake_ip_range.clone(), config.fake_ip_range6.clone()));
        }
        self.reserved.extend(reserved);
        if self.validated.as_ref() != Some(config) {
            self.validated = Some(config.clone());
        }
        Ok(())
    }
}

impl MappingStore for LocalMappingStore {
    fn configure(&self, config: &Config, reserved: &HashSet<IpAddr>) -> Result<()> {
        self.state
            .lock()
            .map_err(|_| anyhow::anyhow!("DNS mapping store is unavailable"))?
            .configure(config, reserved)
    }
    fn allocate(
        &self,
        config: &Config,
        domain: &str,
        ipv6: bool,
        reserved: &HashSet<IpAddr>,
    ) -> Result<Option<IpAddr>> {
        anyhow::ensure!(config.enable && config.fake_ip, "Fake IP is disabled");
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("DNS mapping store is unavailable"))?;
        state.configure(config, reserved)?;
        let SharedMappings {
            mappings, reserved, ..
        } = &mut *state;
        mappings.allocate(config, domain, ipv6, reserved)
    }
    fn restore(&self, config: &Config, ip: IpAddr) -> Result<Option<String>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("DNS mapping store is unavailable"))?;
        state.configure(config, &HashSet::new())?;
        state.mappings.reverse(config, ip)
    }
    fn observe(&self, config: &Config, domain: &str, ip: IpAddr, ttl: u32) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("DNS mapping store is unavailable"))?;
        state.configure(config, &HashSet::new())?;
        state.mappings.observe(config, domain, ip, ttl);
        Ok(())
    }
}

struct Entry {
    domain: String,
    expires: Instant,
}
struct RealEntry {
    domain: Option<String>,
    expires: Instant,
}
pub(crate) struct Mappings {
    fake: HashMap<IpAddr, Entry>,
    real: HashMap<IpAddr, RealEntry>,
    next4: u32,
    next6: u128,
}
impl Default for Mappings {
    fn default() -> Self {
        Self {
            fake: HashMap::new(),
            real: HashMap::new(),
            next4: 2,
            next6: 2,
        }
    }
}
impl Mappings {
    pub fn contains_fake(config: &Config, ip: IpAddr) -> bool {
        match ip {
            IpAddr::V4(ip) => config
                .fake_ip_range
                .parse::<ipnet::Ipv4Net>()
                .is_ok_and(|net| net.contains(&ip)),
            IpAddr::V6(ip) => config
                .fake_ip_range6
                .as_ref()
                .and_then(|raw| raw.parse::<ipnet::Ipv6Net>().ok())
                .is_some_and(|net| net.contains(&ip)),
        }
    }
    pub fn allocate(
        &mut self,
        config: &Config,
        domain: &str,
        ipv6: bool,
        reserved: &std::collections::HashSet<IpAddr>,
    ) -> Result<Option<IpAddr>> {
        let now = Instant::now();
        self.fake.retain(|_, entry| entry.expires > now);
        if let Some((ip, entry)) = self
            .fake
            .iter_mut()
            .find(|(ip, entry)| ip.is_ipv6() == ipv6 && entry.domain == domain)
        {
            entry.expires = entry
                .expires
                .max(now + Duration::from_secs(config.mapping_ttl_secs.into()));
            return Ok(Some(*ip));
        }
        if ipv6 && config.fake_ip_range6.is_none() {
            return Ok(None);
        }
        if self.fake.len() >= config.mapping_capacity {
            bail!("Fake IP mapping capacity reached; live mappings are not evicted");
        }
        // At most capacity + reserved endpoints need probing, never scan a /64.
        for _ in 0..config
            .mapping_capacity
            .saturating_add(reserved.len())
            .saturating_add(3)
        {
            let ip = if ipv6 {
                let net: ipnet::Ipv6Net = config.fake_ip_range6.as_ref().unwrap().parse()?;
                let count = 1u128 << (128 - net.prefix_len());
                if self.next6 >= count - 1 {
                    self.next6 = 2;
                }
                let ip = IpAddr::V6(Ipv6Addr::from(u128::from(net.network()) + self.next6));
                self.next6 += 1;
                ip
            } else {
                let net: ipnet::Ipv4Net = config.fake_ip_range.parse()?;
                let count = 1u32 << (32 - net.prefix_len());
                if self.next4 >= count - 1 {
                    self.next4 = 2;
                }
                let ip = IpAddr::V4(Ipv4Addr::from(u32::from(net.network()) + self.next4));
                self.next4 += 1;
                ip
            };
            if reserved.contains(&ip) {
                continue;
            }
            if let std::collections::hash_map::Entry::Vacant(slot) = self.fake.entry(ip) {
                slot.insert(Entry {
                    domain: domain.to_owned(),
                    expires: now + Duration::from_secs(config.mapping_ttl_secs.into()),
                });
                return Ok(Some(ip));
            }
        }
        bail!("Fake IP address pool exhausted")
    }
    pub fn reverse(&self, config: &Config, ip: IpAddr) -> Result<Option<String>> {
        let now = Instant::now();
        if config.fake_ip && Self::contains_fake(config, ip) {
            return self
                .fake
                .get(&ip)
                .filter(|entry| entry.expires > now)
                .map(|entry| Some(entry.domain.clone()))
                .ok_or_else(|| {
                    anyhow::anyhow!("unknown or expired Fake IP {ip}; refresh client DNS cache")
                });
        }
        Ok(self
            .real
            .get(&ip)
            .filter(|entry| entry.expires > now)
            .and_then(|entry| entry.domain.clone()))
    }
    pub fn observe(&mut self, config: &Config, domain: &str, ip: IpAddr, ttl: u32) {
        if ttl == 0 || Self::contains_fake(config, ip) {
            return;
        }
        let now = Instant::now();
        self.real.retain(|_, entry| entry.expires > now);
        if !self.real.contains_key(&ip) && self.real.len() >= config.mapping_capacity {
            return;
        }
        let expires = now + Duration::from_secs(ttl.min(config.cache_max_ttl_secs).into());
        self.real
            .entry(ip)
            .and_modify(|entry| {
                if entry.domain.as_deref() != Some(domain) {
                    entry.domain = None;
                }
                // Retain ambiguity until all observed answers have expired.
                entry.expires = entry.expires.max(expires);
            })
            .or_insert(RealEntry {
                domain: Some(domain.to_owned()),
                expires,
            });
    }
}
