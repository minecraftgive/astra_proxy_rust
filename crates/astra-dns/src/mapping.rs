use super::Config;
use anyhow::{bail, Result};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    sync::{Arc, Mutex},
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
    /// Non-mutating diagnostic observation. Unknown and expired addresses return None.
    fn lookup_fake(&self, _ip: IpAddr) -> Result<Option<String>> {
        anyhow::bail!("This mapping backend does not support diagnostic observation")
    }
    fn observe(&self, config: &Config, domain: &str, ip: IpAddr, ttl: u32) -> Result<()>;
}

/// The default implementation also serves as the host's authoritative store.
/// Export one Arc to all local resolvers and dispatch worker RPCs to the same
/// object; no mapping tables need to be serialized across worker generations.
#[derive(Default)]
pub struct LocalMappingStore {
    state: Arc<Mutex<SharedMappings>>,
    maintenance: crate::maintenance::Maintenance,
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
        self.maintenance.start(&self.state);
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
        self.maintenance.start(&self.state);
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
    fn lookup_fake(&self, ip: IpAddr) -> Result<Option<String>> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("DNS mapping store is unavailable"))?;
        Ok(state
            .mappings
            .fake
            .get(&ip)
            .filter(|entry| entry.expires > Instant::now())
            .map(|entry| entry.domain.to_string()))
    }
    fn observe(&self, config: &Config, domain: &str, ip: IpAddr, ttl: u32) -> Result<()> {
        self.maintenance.start(&self.state);
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
    domain: Arc<str>,
    expires: Instant,
}
struct RealEntry {
    domain: Option<Arc<str>>,
    expires: Instant,
}
pub(crate) struct Mappings {
    fake: HashMap<IpAddr, Entry>,
    real: HashMap<IpAddr, RealEntry>,
    domains: [HashMap<Arc<str>, IpAddr>; 2],
    fake_expiry: BTreeSet<(Instant, IpAddr)>,
    real_expiry: BTreeSet<(Instant, IpAddr)>,
    fake_underused: crate::maintenance::Underused,
    real_underused: crate::maintenance::Underused,
    next4: u32,
    next6: u128,
}
impl Default for Mappings {
    fn default() -> Self {
        Self {
            fake: HashMap::new(),
            real: HashMap::new(),
            domains: Default::default(),
            fake_expiry: Default::default(),
            real_expiry: Default::default(),
            fake_underused: Default::default(),
            real_underused: Default::default(),
            next4: 2,
            next6: 2,
        }
    }
}
impl Mappings {
    fn expire(&mut self, now: Instant) {
        while let Some(&(expires, ip)) = self.fake_expiry.first() {
            if expires > now {
                break;
            }
            self.fake_expiry.pop_first();
            if let Some(entry) = self.fake.remove(&ip) {
                self.domains[usize::from(ip.is_ipv6())].remove(&entry.domain);
                astra_observe::gauge!("dns.mapping.fake.entries", "Fake-IP 映射条目", "count")
                    .subtract(1);
            }
        }
        while let Some(&(expires, ip)) = self.real_expiry.first() {
            if expires > now {
                break;
            }
            self.real_expiry.pop_first();
            if self.real.remove(&ip).is_some() {
                astra_observe::gauge!("dns.mapping.real.entries", "真实地址映射条目", "count")
                    .subtract(1);
            }
        }
    }
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
        self.expire(now);
        if let Some(ip) = self.domains[usize::from(ipv6)].get(domain).copied() {
            let entry = self.fake.get_mut(&ip).expect("mapping index");
            self.fake_expiry.remove(&(entry.expires, ip));
            entry.expires = entry
                .expires
                .max(now + Duration::from_secs(config.mapping_ttl_secs.into()));
            self.fake_expiry.insert((entry.expires, ip));
            return Ok(Some(ip));
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
                let domain: Arc<str> = domain.into();
                let expires = now + Duration::from_secs(config.mapping_ttl_secs.into());
                slot.insert(Entry {
                    domain: domain.clone(),
                    expires,
                });
                self.domains[usize::from(ipv6)].insert(domain, ip);
                self.fake_expiry.insert((expires, ip));
                if self.fake.len().saturating_mul(4) >= self.fake.capacity() {
                    self.fake_underused = Default::default();
                }
                astra_observe::gauge!("dns.mapping.fake.entries", "Fake-IP 映射条目", "count")
                    .add(1);
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
                .map(|entry| Some(entry.domain.to_string()))
                .ok_or_else(|| {
                    anyhow::anyhow!("unknown or expired Fake IP {ip}; refresh client DNS cache")
                });
        }
        Ok(self
            .real
            .get(&ip)
            .filter(|entry| entry.expires > now)
            .and_then(|entry| entry.domain.as_ref().map(ToString::to_string)))
    }
    pub fn observe(&mut self, config: &Config, domain: &str, ip: IpAddr, ttl: u32) {
        if ttl == 0 || Self::contains_fake(config, ip) {
            return;
        }
        let now = Instant::now();
        self.expire(now);
        if !self.real.contains_key(&ip) && self.real.len() >= config.mapping_capacity {
            return;
        }
        let expires = now + Duration::from_secs(ttl.min(config.cache_max_ttl_secs).into());
        if let Some(entry) = self.real.get_mut(&ip) {
            self.real_expiry.remove(&(entry.expires, ip));
            if entry.domain.as_deref() != Some(domain) {
                entry.domain = None;
            }
            entry.expires = entry.expires.max(expires);
            self.real_expiry.insert((entry.expires, ip));
        } else {
            self.real.insert(
                ip,
                RealEntry {
                    domain: Some(domain.into()),
                    expires,
                },
            );
            self.real_expiry.insert((expires, ip));
            if self.real.len().saturating_mul(4) >= self.real.capacity() {
                self.real_underused = Default::default();
            }
            astra_observe::gauge!("dns.mapping.real.entries", "真实地址映射条目", "count").add(1);
        }
    }
}

impl crate::maintenance::Maintain for SharedMappings {
    fn maintain(&mut self, now: Instant) {
        let m = &mut self.mappings;
        m.expire(now);
        if m.fake_underused.ready(m.fake.len(), m.fake.capacity(), now) {
            m.fake.shrink_to_fit();
            for domains in &mut m.domains {
                domains.shrink_to_fit();
            }
        }
        if m.real_underused.ready(m.real.len(), m.real.capacity(), now) {
            m.real.shrink_to_fit();
        }
    }
}
impl Drop for Mappings {
    fn drop(&mut self) {
        astra_observe::gauge!("dns.mapping.fake.entries", "Fake-IP 映射条目", "count")
            .subtract(self.fake.len() as u64);
        astra_observe::gauge!("dns.mapping.real.entries", "真实地址映射条目", "count")
            .subtract(self.real.len() as u64);
    }
}
#[cfg(test)]
#[path = "test/mapping_memory_test.rs"]
mod memory_tests;

#[cfg(test)]
mod diagnostic_tests {
    use super::*;
    #[test]
    fn fake_lookup_requires_a_live_mapping_and_does_not_allocate_or_refresh() {
        let store = LocalMappingStore::default();
        let config = Config {
            fake_ip: true,
            ..Default::default()
        };
        let unknown = "198.18.0.2".parse().unwrap();
        assert_eq!(store.lookup_fake(unknown).unwrap(), None);
        let ip = store
            .allocate(&config, "mapped.test", false, &HashSet::new())
            .unwrap()
            .unwrap();
        let before = store.state.lock().unwrap().mappings.fake[&ip].expires;
        assert_eq!(
            store.lookup_fake(ip).unwrap().as_deref(),
            Some("mapped.test")
        );
        assert_eq!(
            store.state.lock().unwrap().mappings.fake[&ip].expires,
            before
        );
        store
            .state
            .lock()
            .unwrap()
            .mappings
            .fake
            .get_mut(&ip)
            .unwrap()
            .expires = Instant::now() - Duration::from_secs(1);
        assert_eq!(store.lookup_fake(ip).unwrap(), None);
        let real = "203.0.113.1".parse().unwrap();
        store.observe(&config, "real.test", real, 60).unwrap();
        assert_eq!(store.lookup_fake(real).unwrap(), None);
        assert_eq!(store.state.lock().unwrap().mappings.fake.len(), 1);
    }
}
