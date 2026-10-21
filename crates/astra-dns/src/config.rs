use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    net::{IpAddr, SocketAddr},
};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub enable: bool,
    pub listen: Vec<SocketAddr>,
    pub nameserver: Vec<String>,
    pub fallback: Vec<String>,
    pub nameserver_policy: BTreeMap<String, Vec<String>>,
    pub bootstrap: Vec<IpAddr>,
    pub respect_rules: bool,
    pub ipv6: bool,
    pub cache_capacity: usize,
    pub cache_max_ttl_secs: u32,
    pub negative_ttl_secs: u32,
    pub query_timeout_ms: u64,
    pub max_concurrent: usize,
    pub hijack: bool,
    pub fake_ip: bool,
    pub fake_ip_range: String,
    pub fake_ip_range6: Option<String>,
    pub fake_ip_filter: Vec<String>,
    pub fake_ip_filter_mode: String,
    pub fake_ip_ttl: u32,
    pub mapping_ttl_secs: u32,
    pub mapping_capacity: usize,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            enable: true,
            listen: vec!["127.0.0.1:1053".parse().unwrap()],
            nameserver: vec!["udp://1.1.1.1:53".into()],
            fallback: vec![],
            nameserver_policy: BTreeMap::new(),
            bootstrap: vec!["1.1.1.1".parse().unwrap(), "8.8.8.8".parse().unwrap()],
            respect_rules: false,
            ipv6: true,
            cache_capacity: 4096,
            cache_max_ttl_secs: 3600,
            negative_ttl_secs: 60,
            query_timeout_ms: 3000,
            max_concurrent: 128,
            hijack: true,
            fake_ip: false,
            fake_ip_range: "198.18.0.0/16".into(),
            fake_ip_range6: None,
            fake_ip_filter: vec!["+.lan".into(), "+.local".into(), "localhost".into()],
            fake_ip_filter_mode: "blacklist".into(),
            fake_ip_ttl: 60,
            mapping_ttl_secs: 86400,
            mapping_capacity: 8192,
        }
    }
}
impl Config {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.listen.len() <= 16 && self.listen.iter().all(|a| a.port() != 0),
            "DNS listen requires nonzero ports, at most 16 listeners"
        );
        ensure!(
            !self.nameserver.is_empty() && self.nameserver.len() <= 16 && self.fallback.len() <= 16,
            "DNS requires 1..16 nameservers and at most 16 fallbacks"
        );
        ensure!(
            !self.bootstrap.is_empty()
                && self.bootstrap.len() <= 8
                && self
                    .bootstrap
                    .iter()
                    .all(|ip| !ip.is_unspecified() && !ip.is_multicast()),
            "DNS bootstrap requires 1..8 literal IP addresses"
        );
        ensure!(
            self.cache_capacity <= 65536 && (1..=65536).contains(&self.mapping_capacity),
            "DNS cache/mapping capacity exceeds limit"
        );
        ensure!(
            (1..=1024).contains(&self.max_concurrent)
                && (100..=30000).contains(&self.query_timeout_ms),
            "invalid DNS concurrency/timeout"
        );
        ensure!(
            (1..=86400).contains(&self.cache_max_ttl_secs) && self.negative_ttl_secs <= 3600,
            "invalid DNS cache TTL"
        );
        ensure!(
            (1..=3600).contains(&self.fake_ip_ttl)
                && self.mapping_ttl_secs >= self.fake_ip_ttl
                && self.mapping_ttl_secs <= 604800,
            "invalid Fake IP mapping TTL"
        );
        ensure!(
            matches!(self.fake_ip_filter_mode.as_str(), "blacklist" | "whitelist"),
            "unsupported fake_ip_filter_mode"
        );
        let range: ipnet::Ipv4Net = self.fake_ip_range.parse()?;
        ensure!(
            range.prefix_len() >= 15
                && range.prefix_len() <= 30
                && ipnet::Ipv4Net::new("198.18.0.0".parse()?, 15)?.contains(&range),
            "fake_ip_range must be inside 198.18.0.0/15, prefix 15..30"
        );
        if let Some(range) = &self.fake_ip_range6 {
            let range: ipnet::Ipv6Net = range.parse()?;
            ensure!(
                range.prefix_len() >= 64
                    && range.prefix_len() <= 126
                    && ipnet::Ipv6Net::new("fc00::".parse()?, 7)?.contains(&range),
                "fake_ip_range6 must be a ULA subnet with prefix 64..126"
            );
        }
        ensure!(
            self.nameserver_policy.len() <= 256 && self.fake_ip_filter.len() <= 4096,
            "too many DNS policies/filters"
        );
        for (pattern, servers) in &self.nameserver_policy {
            validate_pattern(pattern)?;
            ensure!(
                !servers.is_empty() && servers.len() <= 16,
                "empty/oversized DNS policy"
            );
        }
        for pattern in &self.fake_ip_filter {
            validate_pattern(pattern)?;
        }
        for raw in self
            .nameserver
            .iter()
            .chain(&self.fallback)
            .chain(self.nameserver_policy.values().flatten())
        {
            let upstream = Upstream::parse(raw)?;
            if let Ok(ip) = upstream.host.parse::<IpAddr>() {
                ensure!(
                    !self
                        .listen
                        .iter()
                        .any(|listen| listen.port() == upstream.port
                            && (listen.ip() == ip
                                || (listen.ip().is_unspecified() && ip.is_loopback()))),
                    "DNS upstream points to its own listener"
                );
            }
        }
        Ok(())
    }
    pub fn servers(&self, domain: &str) -> &[String] {
        self.nameserver_policy
            .iter()
            .filter(|(pattern, _)| matches_domain(pattern, domain))
            .max_by_key(|(pattern, _)| pattern.len())
            .map(|(_, servers)| servers.as_slice())
            .unwrap_or(&self.nameserver)
    }
    pub fn use_fake(&self, domain: &str) -> bool {
        let matched = self
            .fake_ip_filter
            .iter()
            .any(|p| matches_domain(p, domain));
        self.fake_ip && (matched == (self.fake_ip_filter_mode == "whitelist"))
    }
}
fn validate_pattern(pattern: &str) -> Result<()> {
    let value = pattern.trim_start_matches("+.").trim_start_matches("*.");
    ensure!(
        !value.is_empty()
            && value.len() <= 253
            && value
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b".-_".contains(&c)),
        "unsupported DNS domain pattern: {pattern}"
    );
    Ok(())
}
pub fn matches_domain(pattern: &str, domain: &str) -> bool {
    let pattern = pattern.trim_end_matches('.').to_ascii_lowercase();
    let domain = domain.trim_end_matches('.').to_ascii_lowercase();
    if let Some(suffix) = pattern.strip_prefix("+.") {
        return domain == suffix || domain.ends_with(&format!(".{suffix}"));
    }
    if let Some(suffix) = pattern.strip_prefix("*.") {
        return domain.ends_with(&format!(".{suffix}"));
    }
    domain == pattern
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Upstream {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub path: String,
}
impl Upstream {
    pub fn parse(raw: &str) -> Result<Self> {
        let raw = if raw.contains("://") {
            raw.to_owned()
        } else if raw.parse::<IpAddr>().is_ok_and(|a| a.is_ipv6()) {
            format!("udp://[{raw}]")
        } else {
            format!("udp://{raw}")
        };
        let uri = url::Url::parse(&raw)?;
        ensure!(
            matches!(uri.scheme(), "udp" | "tcp" | "tls" | "https"),
            "unsupported DNS upstream scheme"
        );
        ensure!(
            uri.username().is_empty() && uri.password().is_none() && uri.fragment().is_none(),
            "DNS upstream credentials/fragments unsupported"
        );
        let host = uri
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("DNS upstream requires host"))?
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned();
        let port = uri.port().unwrap_or(match uri.scheme() {
            "https" => 443,
            "tls" => 853,
            _ => 53,
        });
        ensure!(port != 0, "DNS upstream port must be nonzero");
        ensure!(
            uri.scheme() == "https"
                || ((uri.path().is_empty() || uri.path() == "/") && uri.query().is_none()),
            "only DoH supports upstream paths"
        );
        let path = if uri.path().is_empty() || uri.path() == "/" {
            "/dns-query"
        } else {
            uri.path()
        };
        let path = if let Some(query) = uri.query() {
            format!("{path}?{query}")
        } else {
            path.to_owned()
        };
        Ok(Self {
            scheme: uri.scheme().into(),
            host,
            port,
            path,
        })
    }
}
