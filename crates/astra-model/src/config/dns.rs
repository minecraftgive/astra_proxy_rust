use anyhow::{ensure, Result};
use ipnet::{IpNet, Ipv4Net, Ipv6Net};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    net::{IpAddr, Ipv4Addr, SocketAddr},
};

pub const BENCHMARK_NET: Ipv4Net = Ipv4Net::new_assert(Ipv4Addr::new(198, 18, 0, 0), 15);
const IPV4_POOLS: [Ipv4Net; 4] = [
    BENCHMARK_NET,
    Ipv4Net::new_assert(Ipv4Addr::new(10, 0, 0, 0), 8),
    Ipv4Net::new_assert(Ipv4Addr::new(172, 16, 0, 0), 12),
    Ipv4Net::new_assert(Ipv4Addr::new(192, 168, 0, 0), 16),
];

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct MagicDns {
    pub enable: bool,
    pub domain: String,
    #[serde(deserialize_with = "read_magic_hosts")]
    pub hosts: BTreeMap<String, Vec<IpAddr>>,
}

fn read_magic_hosts<'de, D: serde::Deserializer<'de>>(
    input: D,
) -> Result<BTreeMap<String, Vec<IpAddr>>, D::Error> {
    let hosts = BTreeMap::<String, Vec<String>>::deserialize(input)?;
    hosts
        .into_iter()
        .map(|(name, values)| {
            let ips = values
                .into_iter()
                .map(|value| {
                    value.parse().map_err(|_| {
                        serde::de::Error::custom(format!(
                            "Magic DNS device {name} has invalid IP address {value}"
                        ))
                    })
                })
                .collect::<Result<_, D::Error>>()?;
            Ok((name, ips))
        })
        .collect()
}

impl Default for MagicDns {
    fn default() -> Self {
        Self {
            enable: false,
            domain: "astra.home.arpa".into(),
            hosts: BTreeMap::new(),
        }
    }
}

fn valid_label(label: &str) -> bool {
    (1..=63).contains(&label.len())
        && !label.starts_with('-')
        && !label.ends_with('-')
        && label
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-')
}

pub fn valid_search_domain(domain: &str) -> bool {
    let domain = domain.strip_suffix('.').unwrap_or(domain);
    domain.len() <= 253 && domain.split('.').all(valid_label)
}

impl MagicDns {
    fn is_default(&self) -> bool {
        !self.enable && self.domain == "astra.home.arpa" && self.hosts.is_empty()
    }

    pub fn search_domain(&self) -> Option<String> {
        self.enable
            .then(|| self.domain.trim_end_matches('.').to_ascii_lowercase())
    }

    fn validate(&self, config: &Config) -> Result<()> {
        let domain = self.domain.trim_end_matches('.').to_ascii_lowercase();
        ensure!(
            valid_search_domain(&self.domain)
                && domain.parse::<IpAddr>().is_err()
                && domain != "empty"
                && domain != "localhost"
                && !domain.ends_with(".localhost"),
            "invalid dns.magic_dns.domain: {}",
            self.domain
        );
        let mut names = BTreeSet::new();
        for (name, ips) in &self.hosts {
            ensure!(
                valid_label(name)
                    && name.len() + 1 + domain.len() <= 253
                    && !name.eq_ignore_ascii_case("localhost")
                    && !name.eq_ignore_ascii_case(&domain),
                "invalid Magic DNS device name: {name}"
            );
            ensure!(
                names.insert(name.to_ascii_lowercase()),
                "duplicate Magic DNS device: {name}"
            );
            ensure!(!ips.is_empty(), "Magic DNS device {name} has no addresses");
            for ip in ips {
                ensure!(
                    !matches!(ip, IpAddr::V4(v4) if BENCHMARK_NET.contains(v4))
                        && !config.active_fake_ranges().any(|range| range.contains(ip)),
                    "Magic DNS device {name} address {ip} conflicts with Fake IP space"
                );
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(remote = "Self", default, deny_unknown_fields)]
pub struct Config {
    pub enable: bool,
    #[serde(skip_serializing_if = "MagicDns::is_default")]
    pub magic_dns: MagicDns,
    pub listen: Vec<SocketAddr>,
    pub proxy_nameserver: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fallback: Vec<String>,
    pub direct_nameserver: Vec<String>,
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
    pub fake_ip_range: Ipv4Net,
    pub fake_ip_range6: Option<Ipv6Net>,
    pub fake_ip_filter: Vec<String>,
    pub fake_ip_filter_mode: String,
    pub fake_ip_ttl: u32,
    pub mapping_ttl_secs: u32,
    pub mapping_capacity: usize,
    /// Default false: only DoH (`https://`) and DoT (`tls://`) upstreams are
    /// accepted, upstream hostnames are bootstrapped through IP-literal
    /// encrypted upstreams, and failures never downgrade to plaintext.
    /// true skips the check (insecure; allows `udp://`/`tcp://` upstreams and
    /// plaintext UDP/53 bootstrap through `bootstrap`).
    pub plaintext_bypass: bool,
    /// Domain patterns whose `nameserver_policy` entries may use plaintext
    /// upstreams while the global check stays on, e.g. `+.lan` for a router.
    pub plaintext_bypass_domains: Vec<String>,
    /// While TUN is active (with `enable` and `hijack`), point the OS resolver
    /// at an address inside the TUN subnet so every system query is hijacked.
    /// The previous settings are restored when TUN stops or on crash recovery.
    pub system_takeover: bool,
    /// While TUN is active, route the previously configured system DNS servers
    /// (IPv6 too when tun.ipv6_address is set) into the TUN, so queries sent
    /// straight to them are hijacked as well. Gateway/link-local servers are
    /// skipped to keep the physical default route intact.
    pub route_system_dns: bool,
    /// Without TUN (macOS only): serve DNS on 0.0.0.0:53 to loopback clients
    /// and point the OS resolver at 127.0.0.1, restoring it on exit. Opt-in
    /// because it binds port 53 and changes system settings.
    pub system_takeover_without_tun: bool,
}
impl Serialize for Config {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        Config::serialize(self, serializer)
    }
}

impl<'de> Deserialize<'de> for Config {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Input {
            nameserver: Option<Vec<String>>,
            proxy_server_nameserver: Option<Vec<String>>,
            direct_nameserver: Option<Vec<String>>,
            proxy_nameserver: Option<Vec<String>>,
            #[serde(flatten, deserialize_with = "Config::deserialize")]
            settings: Config,
        }
        let input = Input::deserialize(deserializer)?;
        let mut config = input.settings;
        if input.nameserver.is_some() && input.proxy_nameserver.is_some()
            || input.proxy_server_nameserver.is_some() && input.direct_nameserver.is_some()
        {
            return Err(serde::de::Error::custom(
                "conflicting legacy and split DNS upstream fields",
            ));
        }
        if let Some(direct) = input
            .direct_nameserver
            .or_else(|| {
                input
                    .proxy_server_nameserver
                    .filter(|servers| !servers.is_empty())
            })
            .or_else(|| input.nameserver.clone())
        {
            config.direct_nameserver = direct;
        }
        if let Some(proxy) = input.proxy_nameserver.or(input.nameserver) {
            config.proxy_nameserver = proxy;
        }
        config.fake_ip_range = config.fake_ip_range.trunc();
        config.fake_ip_range6 = config.fake_ip_range6.map(|net| net.trunc());
        Ok(config)
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enable: true,
            magic_dns: Default::default(),
            listen: vec!["127.0.0.1:1053".parse().unwrap()],
            proxy_nameserver: vec![
                "https://1.1.1.1/dns-query".into(),
                "tls://1.0.0.1:853".into(),
            ],
            fallback: vec![],
            direct_nameserver: vec![
                "https://223.6.6.6/dns-query".into(),
                "https://223.5.5.5/dns-query".into(),
            ],
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
            fake_ip_range: Ipv4Net::new_assert(Ipv4Addr::new(198, 18, 0, 0), 16),
            fake_ip_range6: None,
            fake_ip_filter: vec!["+.lan".into(), "+.local".into(), "localhost".into()],
            fake_ip_filter_mode: "blacklist".into(),
            fake_ip_ttl: 60,
            mapping_ttl_secs: 86400,
            mapping_capacity: 8192,
            plaintext_bypass: false,
            plaintext_bypass_domains: vec![],
            system_takeover: true,
            route_system_dns: true,
            system_takeover_without_tun: false,
        }
    }
}
impl Config {
    pub fn active_fake_ranges(&self) -> impl Iterator<Item = IpNet> + '_ {
        std::iter::once(IpNet::V4(self.fake_ip_range))
            .chain(self.fake_ip_range6.map(IpNet::V6))
            .filter(|_| self.enable && self.fake_ip)
    }

    pub fn validate(&self) -> Result<()> {
        self.magic_dns.validate(self)?;
        ensure!(
            self.listen.len() <= 16 && self.listen.iter().all(|a| a.port() != 0),
            "DNS listen requires nonzero ports, at most 16 listeners"
        );
        ensure!(
            !self.proxy_nameserver.is_empty()
                && self.proxy_nameserver.len() <= 16
                && self.fallback.len() <= 16,
            "DNS requires 1..16 proxy_nameserver entries and at most 16 fallback entries"
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
            (1..=16).contains(&self.direct_nameserver.len()),
            "DNS direct_nameserver requires 1..16 upstreams"
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
        let range = self.fake_ip_range;
        ensure!(
            range.prefix_len() <= 30 && IPV4_POOLS.iter().any(|pool| pool.contains(&range)),
            "fake_ip_range must be a subnet of 198.18.0.0/15 or RFC 1918 private space, with at least four addresses"
        );
        if let Some(range) = self.fake_ip_range6 {
            ensure!(
                range.prefix_len() <= 126
                    && ipnet::Ipv6Net::new("fc00::".parse()?, 7)?.contains(&range),
                "fake_ip_range6 must be a ULA subnet with prefix 7..126"
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
        ensure!(
            self.plaintext_bypass_domains.len() <= 256,
            "too many DNS plaintext bypass domains"
        );
        for pattern in &self.plaintext_bypass_domains {
            validate_pattern(pattern)?;
        }
        self.validate_encryption()?;
        for raw in self
            .proxy_nameserver
            .iter()
            .chain(&self.fallback)
            .chain(&self.direct_nameserver)
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
    /// Encrypted-only policy. Errors name the offending entry and the bypass
    /// switch so a rejected configuration is self-explaining.
    fn validate_encryption(&self) -> Result<()> {
        if self.plaintext_bypass {
            return Ok(());
        }
        for raw in self
            .proxy_nameserver
            .iter()
            .chain(&self.fallback)
            .chain(&self.direct_nameserver)
        {
            ensure!(
                Upstream::parse(raw)?.is_encrypted(),
                "DNS upstream {raw} is plaintext; only https:// (DoH) and tls:// (DoT) are allowed \
                 (set dns.plaintext_bypass = true to allow plaintext, insecure)"
            );
        }
        for (pattern, servers) in &self.nameserver_policy {
            for raw in servers {
                ensure!(
                    Upstream::parse(raw)?.is_encrypted()
                        || self
                            .plaintext_bypass_domains
                            .iter()
                            .any(|bypass| covers(bypass, pattern)),
                    "DNS policy {pattern} uses plaintext upstream {raw}; use DoH/DoT or add \
                     {pattern} to dns.plaintext_bypass_domains"
                );
            }
        }
        let needs_bootstrap = self
            .proxy_nameserver
            .iter()
            .chain(&self.direct_nameserver)
            .chain(&self.fallback)
            .chain(self.nameserver_policy.values().flatten())
            .any(|raw| {
                Upstream::parse(raw).is_ok_and(|upstream| upstream.host.parse::<IpAddr>().is_err())
            });
        ensure!(!needs_bootstrap || !self.direct_bootstrap().is_empty(),
            "DNS upstream hostnames need an IP-literal direct_nameserver or fallback for encrypted bootstrap");
        Ok(())
    }
    /// Whether a plaintext upstream may answer `domain`.
    pub fn plaintext_allowed(&self, domain: &str) -> bool {
        self.plaintext_bypass
            || self
                .plaintext_bypass_domains
                .iter()
                .any(|pattern| matches_domain(pattern, domain))
    }
    pub fn direct_bootstrap(&self) -> Vec<Upstream> {
        self.direct_nameserver
            .iter()
            .chain(&self.fallback)
            .filter_map(|raw| Upstream::parse(raw).ok())
            .filter(|upstream| upstream.host.parse::<IpAddr>().is_ok())
            .collect()
    }
    /// Whether a nameserver_policy entry applies to `domain`.
    pub fn has_policy(&self, domain: &str) -> bool {
        self.nameserver_policy
            .keys()
            .any(|pattern| matches_domain(pattern, domain))
    }
    pub fn servers(&self, domain: &str, direct: bool) -> &[String] {
        self.nameserver_policy
            .iter()
            .filter(|(pattern, _)| matches_domain(pattern, domain))
            .max_by_key(|(pattern, _)| pattern.len())
            .map(|(_, servers)| servers.as_slice())
            .unwrap_or(if direct {
                &self.direct_nameserver
            } else {
                &self.proxy_nameserver
            })
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
/// True when every domain matched by `pattern` is also matched by `bypass`.
fn covers(bypass: &str, pattern: &str) -> bool {
    fn parts(raw: &str) -> (&'static str, String) {
        let raw = raw.trim_end_matches('.').to_ascii_lowercase();
        for prefix in ["+.", "*."] {
            if let Some(rest) = raw.strip_prefix(prefix) {
                return (prefix, rest.to_owned());
            }
        }
        ("", raw)
    }
    let (bypass_kind, base) = parts(bypass);
    let (pattern_kind, domain) = parts(pattern);
    let under = domain.ends_with(&format!(".{base}"));
    match (bypass_kind, pattern_kind) {
        (_, "") => matches_domain(bypass, &domain),
        ("+.", _) | ("*.", "*.") => domain == base || under,
        ("*.", _) => under,
        _ => false,
    }
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
    /// DoH or DoT; `udp`/`tcp` are plaintext.
    pub fn is_encrypted(&self) -> bool {
        matches!(self.scheme.as_str(), "tls" | "https")
    }
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
