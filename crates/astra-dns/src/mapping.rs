use super::Config;
use anyhow::{bail, Result};
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    time::{Duration, Instant},
};

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
            entry.expires = now + Duration::from_secs(config.mapping_ttl_secs.into());
            return Ok(Some(*ip));
        }
        if ipv6 && config.fake_ip_range6.is_none() {
            return Ok(None);
        }
        if self.fake.len() >= config.mapping_capacity {
            bail!("Fake IP mapping capacity reached; live mappings are not evicted");
        }
        // At most capacity + reserved endpoints need probing, never scan a /64.
        for _ in 0..config.mapping_capacity + 3 {
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
