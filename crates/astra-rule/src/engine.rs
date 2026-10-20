use anyhow::{bail, Context, Result};
use astra_ipc::{Network, RouteAction, RouteRequest};
use ipnet::IpNet;
use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    net::IpAddr,
    path::Path,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleConfig {
    pub socket: String,
    #[serde(default)]
    pub watch_config: bool,
    pub default_action: String,
    #[serde(default = "enabled")]
    pub direct_local: bool,
    #[serde(default)]
    pub rules: Vec<RuleSpec>,
    #[serde(default)]
    pub rule_sets: BTreeMap<String, astra_rule_data::RuleSet>,
    #[serde(default)]
    pub geoip_database: Option<String>,
    #[serde(default)]
    pub geoip_url: Option<String>,
    #[serde(default = "geoip_interval")]
    pub geoip_interval: u64,
    #[serde(default)]
    pub geosite_database: Option<String>,
    #[serde(default)]
    pub geosite_url: Option<String>,
    #[serde(default = "geoip_interval")]
    pub geosite_interval: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleSpec {
    pub kind: String,
    pub value: String,
    pub action: String,
}

#[derive(Debug, Clone)]
struct Entry {
    index: usize,
    action: RouteAction,
}

#[derive(Debug)]
pub struct RuleSource {
    index: usize,
    end: usize,
    kind: String,
    value: String,
}
pub enum DecisionPath<'a> {
    Rule {
        source: &'a RuleSource,
        expanded_index: usize,
    },
    Local,
    Default,
}
impl std::fmt::Display for DecisionPath<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rule {
                source,
                expanded_index,
            } => write!(
                f,
                "rules[{}] -> {}({:?}) -> compiled[{}]",
                source.index, source.kind, source.value, expanded_index
            ),
            Self::Local => f.write_str("direct_local -> DIRECT"),
            Self::Default => f.write_str("default_action"),
        }
    }
}
pub struct Decision<'a> {
    pub action: RouteAction,
    pub path: DecisionPath<'a>,
}

#[derive(Debug)]
enum GeoDatabase {
    Mmdb(maxminddb::Reader<Vec<u8>>),
    Dat(astra_rule_data::geoip::Database),
}
impl GeoDatabase {
    fn decode(bytes: &[u8], categories: &[String]) -> Result<Self> {
        if maxminddb::Reader::from_source(bytes).is_ok() {
            Ok(Self::Mmdb(maxminddb::Reader::from_source(bytes.to_vec())?))
        } else {
            Ok(Self::Dat(
                astra_rule_data::geoip::Database::decode_selected(bytes, categories)
                    .context("expected a valid MMDB or geoip.dat database")?,
            ))
        }
    }
}

#[derive(Debug)]
pub struct RuleEngine {
    sources: Vec<RuleSource>,
    default: RouteAction,
    direct_local: bool,
    geoip: Option<GeoDatabase>,
    countries: HashMap<String, Entry>,
    keywords: Vec<(String, Entry)>,
    regexes: Vec<(regex::Regex, Entry)>,
    patterns: Vec<(String, Entry)>,
    subdomains: HashMap<String, Entry>,
    process_name: HashMap<String, Entry>,
    process_path: HashMap<String, Entry>,
    exact: HashMap<String, Entry>,
    suffix: HashMap<String, Entry>,
    port: HashMap<u16, Entry>,
    network: HashMap<u8, Entry>,
    v4: Vec<(u8, HashMap<u32, Entry>)>,
    v6: Vec<(u8, HashMap<u128, Entry>)>,
}

fn compile_regex(pattern: &str) -> Result<regex::Regex> {
    astra_rule_data::compile_domain_regex(pattern)
}

fn geoip_interval() -> u64 {
    86400
}

fn enabled() -> bool {
    true
}

fn process_path(value: &str) -> String {
    if value.contains('\\') || value.as_bytes().get(1) == Some(&b':') {
        value.replace('\\', "/").to_lowercase()
    } else {
        value.to_owned()
    }
}

fn local_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_private() || ip.is_loopback() || ip.is_link_local(),
        IpAddr::V6(ip) => ip.to_ipv4_mapped().map_or_else(
            || ip.is_loopback() || ip.is_unique_local() || ip.is_unicast_link_local(),
            |ip| local_ip(IpAddr::V4(ip)),
        ),
    }
}

fn parse_action(input: &str) -> Result<RouteAction> {
    match input {
        "direct" => Ok(RouteAction::Direct),
        "block" => Ok(RouteAction::Block),
        s if s.starts_with("proxy:") && s.len() > 6 && s.len() <= 70 => {
            Ok(RouteAction::Proxy(s[6..].to_owned()))
        }
        _ => bail!("invalid action: {input} (expected direct, block, proxy:TAG)"),
    }
}

fn consider<'a>(best: &mut Option<&'a Entry>, candidate: Option<&'a Entry>) {
    if let Some(entry) = candidate {
        if best.is_none_or(|current| entry.index < current.index) {
            *best = Some(entry);
        }
    }
}

fn mask4(bits: u8) -> u32 {
    if bits == 0 {
        0
    } else {
        u32::MAX << (32 - bits)
    }
}
fn mask6(bits: u8) -> u128 {
    if bits == 0 {
        0
    } else {
        u128::MAX << (128 - bits)
    }
}

impl RuleEngine {
    #[cfg(test)]
    pub fn compile(config: &RuleConfig) -> Result<Self> {
        Self::compile_at(config, Path::new("."))
    }
    pub fn compile_at(config: &RuleConfig, base: &Path) -> Result<Self> {
        // Validate definitions without downloading unused or disabled resources.
        let referenced: HashSet<&str> = config
            .rules
            .iter()
            .filter(|r| r.kind == "rule_set")
            .map(|r| r.value.as_str())
            .collect();
        for (name, source) in &config.rule_sets {
            anyhow::ensure!(
                !name.trim().is_empty() && name.len() <= 128 && !name.chars().any(char::is_control),
                "invalid rule-set name: {name:?}"
            );
            source
                .validate()
                .with_context(|| format!("rule_sets[{name:?}]"))?;
            if source.enabled && source.path.is_none() && !referenced.contains(name.as_str()) {
                source
                    .parse_entries(&source.payload)
                    .with_context(|| format!("rule_sets[{name:?}]"))?;
            }
        }
        fn expand(
            spec: &RuleSpec,
            config: &RuleConfig,
            base: &Path,
            active: &mut Vec<String>,
            cache: &mut HashMap<String, Vec<astra_rule_data::MatchSpec>>,
            output: &mut Vec<RuleSpec>,
        ) -> Result<()> {
            anyhow::ensure!(
                output.len() < 1_000_000,
                "expanded rules exceed 1000000 entries"
            );
            if spec.kind != "rule_set" {
                output.push(spec.clone());
                return Ok(());
            }
            anyhow::ensure!(
                active.len() < 32 && !active.contains(&spec.value),
                "rule-set cycle or excessive nesting: {}",
                spec.value
            );
            if !cache.contains_key(&spec.value) {
                let source = config
                    .rule_sets
                    .get(&spec.value)
                    .with_context(|| format!("unknown rule-set: {}", spec.value))?;
                if !source.enabled {
                    return Ok(());
                }
                cache.insert(
                    spec.value.clone(),
                    source
                        .load(base)
                        .with_context(|| format!("load rule-set {}", spec.value))?,
                );
            }
            active.push(spec.value.clone());
            for matcher in cache[&spec.value].clone() {
                expand(
                    &RuleSpec {
                        kind: matcher.kind,
                        value: matcher.value,
                        action: spec.action.clone(),
                    },
                    config,
                    base,
                    active,
                    cache,
                    output,
                )?;
            }
            active.pop();
            Ok(())
        }
        let mut rules = Vec::new();
        let mut sources = Vec::new();
        let mut cache = HashMap::new();
        for (index, spec) in config.rules.iter().enumerate() {
            parse_action(&spec.action).with_context(|| format!("rules[{}] action", index + 1))?;
            expand(spec, config, base, &mut Vec::new(), &mut cache, &mut rules)
                .with_context(|| format!("rules[{}]", index + 1))?;
            sources.push(RuleSource {
                index: index + 1,
                end: rules.len(),
                kind: spec.kind.clone(),
                value: spec.value.clone(),
            });
        }
        if rules.iter().any(|rule| rule.kind == "geosite") {
            let path = config
                .geosite_database
                .as_deref()
                .context("GEOSITE rules require geosite_database (geosite.dat)")?;
            let selectors: Vec<_> = rules
                .iter()
                .filter(|r| r.kind == "geosite")
                .map(|r| r.value.clone())
                .collect();
            let bytes = astra_rule_data::load_resource(
                base,
                path,
                config.geosite_url.as_deref(),
                config.geosite_interval,
                128 * 1024 * 1024,
                |bytes| {
                    let db = astra_rule_data::geosite::Database::decode(bytes)?;
                    let mut count = rules.iter().filter(|rule| rule.kind != "geosite").count();
                    for selector in &selectors {
                        let entries = db.expand(selector)?;
                        count += entries.len();
                        anyhow::ensure!(
                            count <= 1_000_000,
                            "expanded rules exceed 1000000 entries"
                        );
                        for matcher in entries {
                            if matcher.kind == "domain_regex" {
                                compile_regex(&matcher.value)?;
                            } else if matcher.kind == "domain_keyword" {
                                anyhow::ensure!(
                                    !matcher.value.chars().any(char::is_whitespace),
                                    "invalid GEOSITE keyword"
                                );
                            } else {
                                normalize_domain(&matcher.value)?;
                            }
                        }
                    }
                    Ok(())
                },
            )?;
            let database = astra_rule_data::geosite::Database::decode(&bytes)?;
            let mut expanded = Vec::new();
            let mut input = rules.into_iter();
            let mut previous_end = 0;
            for source in &mut sources {
                let count = source.end - previous_end;
                previous_end = source.end;
                for rule in input.by_ref().take(count) {
                    if rule.kind == "geosite" {
                        for matcher in database.expand(&rule.value)? {
                            anyhow::ensure!(
                                expanded.len() < 1_000_000,
                                "expanded rules exceed 1000000 entries"
                            );
                            expanded.push(RuleSpec {
                                kind: matcher.kind,
                                value: matcher.value,
                                action: rule.action.clone(),
                            });
                        }
                    } else {
                        anyhow::ensure!(
                            expanded.len() < 1_000_000,
                            "expanded rules exceed 1000000 entries"
                        );
                        expanded.push(rule);
                    }
                }
                source.end = expanded.len();
            }
            rules = expanded;
        }
        let geoip_categories = rules
            .iter()
            .filter(|r| r.kind == "geoip" && !r.value.eq_ignore_ascii_case("LAN"))
            .map(|r| astra_rule_data::geoip::category(&r.value))
            .collect::<Result<Vec<_>>>()?;
        let geoip = if !geoip_categories.is_empty() {
            let path = config
                .geoip_database
                .as_deref()
                .context("GEOIP rules require geoip_database (MMDB or geoip.dat)")?;
            let (_, database) = astra_rule_data::load_resource_with(
                base,
                path,
                config.geoip_url.as_deref(),
                config.geoip_interval,
                128 * 1024 * 1024,
                |bytes| GeoDatabase::decode(bytes, &geoip_categories),
            )?;
            Some(database)
        } else {
            None
        };
        let mut out = Self {
            sources,
            default: parse_action(&config.default_action)?,
            direct_local: config.direct_local,
            geoip,
            countries: HashMap::new(),
            keywords: Vec::new(),
            regexes: Vec::new(),
            patterns: Vec::new(),
            subdomains: HashMap::new(),
            process_name: HashMap::new(),
            process_path: HashMap::new(),
            exact: HashMap::new(),
            suffix: HashMap::new(),
            port: HashMap::new(),
            network: HashMap::new(),
            v4: Vec::new(),
            v6: Vec::new(),
        };
        let mut v4: BTreeMap<u8, HashMap<u32, Entry>> = BTreeMap::new();
        let mut v6: BTreeMap<u8, HashMap<u128, Entry>> = BTreeMap::new();
        for (index, r) in rules.iter().enumerate() {
            let entry = Entry {
                index,
                action: parse_action(&r.action)?,
            };
            match r.kind.as_str() {
                "geoip" => {
                    let code = astra_rule_data::geoip::category(&r.value)?;
                    out.countries.entry(code).or_insert(entry);
                }
                "domain_keyword" => {
                    anyhow::ensure!(
                        !r.value.is_empty()
                            && r.value.len() <= 253
                            && !r.value.chars().any(char::is_whitespace),
                        "invalid domain keyword"
                    );
                    out.keywords.push((r.value.to_ascii_lowercase(), entry));
                }
                "domain_regex" => {
                    out.regexes.push((compile_regex(&r.value)?, entry));
                }
                "domain_subdomain" => {
                    anyhow::ensure!(!r.value.contains('*'), "use domain_pattern for wildcards");
                    let key = normalize_domain(&r.value)?;
                    out.subdomains.entry(key).or_insert(entry);
                }
                "domain_pattern" => {
                    let key = normalize_domain(&r.value)?;
                    anyhow::ensure!(
                        key.split('.')
                            .all(|part| !part.is_empty() && (!part.contains('*') || part == "*")),
                        "invalid domain pattern"
                    );
                    out.patterns.push((key, entry));
                }
                "process_name" | "process_path" => {
                    if r.value.trim().is_empty() || r.value.len() > 4096 || r.value.contains('\0') {
                        bail!("invalid process rule: {}", r.value);
                    }
                    if r.kind == "process_name" {
                        if r.value.contains(['/', '\\']) {
                            bail!("process_name expects a file name");
                        }
                        out.process_name
                            .entry(r.value.to_lowercase())
                            .or_insert(entry);
                    } else {
                        if !r.value.starts_with('/')
                            && !r.value.starts_with("\\\\")
                            && !(r.value.as_bytes().get(1) == Some(&b':')
                                && matches!(r.value.as_bytes().get(2), Some(b'/' | b'\\')))
                        {
                            bail!("process_path expects an absolute executable path");
                        }
                        out.process_path
                            .entry(process_path(&r.value))
                            .or_insert(entry);
                    }
                }
                "domain_exact" => {
                    anyhow::ensure!(!r.value.contains('*'), "use domain_pattern for wildcards");
                    let key = normalize_domain(&r.value)?;
                    out.exact.entry(key).or_insert(entry);
                }
                "domain_suffix" => {
                    anyhow::ensure!(!r.value.contains('*'), "use domain_pattern for wildcards");
                    let key = normalize_domain(&r.value)?;
                    out.suffix.entry(key).or_insert(entry);
                }
                "ip_cidr" => {
                    let net: IpNet = r
                        .value
                        .parse()
                        .with_context(|| format!("invalid cidr {}", r.value))?;
                    match net {
                        IpNet::V4(n) => {
                            let bits = n.prefix_len();
                            let key = u32::from(n.network()) & mask4(bits);
                            v4.entry(bits).or_default().entry(key).or_insert(entry);
                        }
                        IpNet::V6(n) => {
                            let bits = n.prefix_len();
                            let key = u128::from(n.network()) & mask6(bits);
                            v6.entry(bits).or_default().entry(key).or_insert(entry);
                        }
                    }
                }
                "port" => {
                    let port: u16 = r
                        .value
                        .parse()
                        .with_context(|| format!("invalid port {}", r.value))?;
                    if port == 0 {
                        bail!("port 0 is not a valid destination");
                    }
                    out.port.entry(port).or_insert(entry);
                }
                "network" => {
                    let proto = match r.value.as_str() {
                        "tcp" => 1,
                        "udp" => 2,
                        _ => bail!("invalid network: {}", r.value),
                    };
                    out.network.entry(proto).or_insert(entry);
                }
                other => bail!("unsupported rule kind: {other}"),
            }
        }
        out.v4 = v4.into_iter().collect();
        out.v6 = v6.into_iter().collect();
        Ok(out)
    }

    #[cfg(test)]
    pub fn decide(&self, req: &RouteRequest) -> RouteAction {
        self.decision(req).action
    }
    pub fn decision(&self, req: &RouteRequest) -> Decision<'_> {
        let mut best: Option<&Entry> = None;

        if let Some(process) = &req.process {
            consider(
                &mut best,
                self.process_name.get(&process.name.to_lowercase()),
            );
            consider(
                &mut best,
                self.process_path.get(&process_path(&process.path)),
            );
        }

        consider(&mut best, self.port.get(&req.target.port));
        let netkey = match req.network {
            Network::Tcp => 1,
            Network::Udp => 2,
        };
        consider(&mut best, self.network.get(&netkey));

        if let Ok(ip) = req.target.host.parse::<IpAddr>() {
            if local_ip(ip) {
                consider(&mut best, self.countries.get("LAN"));
            }
            if let Some(GeoDatabase::Dat(db)) = &self.geoip {
                for (name, entry) in &self.countries {
                    if name != "LAN" && db.matches(name, ip) {
                        consider(&mut best, Some(entry));
                    }
                }
            }
            if let Some(GeoDatabase::Mmdb(reader)) = &self.geoip {
                if let Ok(result) = reader.lookup(ip.to_canonical()) {
                    if let Ok(Some(country)) = result.decode::<maxminddb::geoip2::Country>() {
                        if let Some(code) = country
                            .country
                            .iso_code
                            .or(country.registered_country.iso_code)
                        {
                            consider(&mut best, self.countries.get(&code.to_ascii_uppercase()));
                        }
                    } else if let Ok(Some(code)) = result.decode::<String>() {
                        consider(&mut best, self.countries.get(&code.to_ascii_uppercase()));
                    } else if let Ok(Some(codes)) = result.decode::<Vec<String>>() {
                        for code in codes {
                            consider(&mut best, self.countries.get(&code.to_ascii_uppercase()));
                        }
                    }
                }
            }
            match ip {
                IpAddr::V4(addr) => {
                    let n = u32::from(addr);
                    for (bits, bucket) in &self.v4 {
                        consider(&mut best, bucket.get(&(n & mask4(*bits))));
                    }
                }
                IpAddr::V6(addr) => {
                    let n = u128::from(addr);
                    for (bits, bucket) in &self.v6 {
                        consider(&mut best, bucket.get(&(n & mask6(*bits))));
                    }
                }
            }
        } else {
            let domain = req.target.host.trim_end_matches('.').to_ascii_lowercase();
            consider(&mut best, self.exact.get(&domain));
            for (pattern, entry) in &self.regexes {
                if pattern.is_match(&domain) {
                    consider(&mut best, Some(entry));
                }
            }
            for (keyword, entry) in &self.keywords {
                if domain.contains(keyword) {
                    consider(&mut best, Some(entry));
                }
            }
            for (pattern, entry) in &self.patterns {
                let labels: Vec<_> = domain.split('.').collect();
                let expected: Vec<_> = pattern.split('.').collect();
                if labels.len() == expected.len()
                    && labels
                        .iter()
                        .zip(expected)
                        .all(|(label, expected)| expected == "*" || *label == expected)
                {
                    consider(&mut best, Some(entry));
                }
            }
            let mut segment = domain.as_str();
            loop {
                consider(&mut best, self.suffix.get(segment));
                if let Some((_, rest)) = segment.split_once('.') {
                    segment = rest;
                    consider(&mut best, self.subdomains.get(segment));
                } else {
                    break;
                }
            }
        }
        if let Some(entry) = best {
            // Each source occupies a contiguous range, including after GEOSITE expansion.
            // Keep one provenance record per configured rule, not per expanded domain.
            let source = &self.sources[self.sources.partition_point(|s| s.end <= entry.index)];
            Decision {
                action: entry.action.clone(),
                path: DecisionPath::Rule {
                    source,
                    expanded_index: entry.index + 1,
                },
            }
        } else if self.direct_local && req.target.host.parse::<IpAddr>().is_ok_and(local_ip) {
            Decision {
                action: RouteAction::Direct,
                path: DecisionPath::Local,
            }
        } else {
            Decision {
                action: self.default.clone(),
                path: DecisionPath::Default,
            }
        }
    }

    /// Reverse lookup is only used by an explicit explanation query, avoiding
    /// a second copy of every expanded provider/GeoSite entry in memory.
    pub fn matched_condition(&self, path: &DecisionPath<'_>) -> Option<(String, String)> {
        let DecisionPath::Rule { expanded_index, .. } = path else {
            return None;
        };
        let index = expanded_index - 1;
        for (kind, entries) in [
            ("domain_exact", &self.exact),
            ("domain_suffix", &self.suffix),
            ("domain_subdomain", &self.subdomains),
            ("process_name", &self.process_name),
            ("process_path", &self.process_path),
            ("geoip", &self.countries),
        ] {
            if let Some((value, _)) = entries.iter().find(|(_, entry)| entry.index == index) {
                return Some((kind.into(), value.clone()));
            }
        }
        for (kind, entries) in [
            ("domain_keyword", &self.keywords),
            ("domain_pattern", &self.patterns),
        ] {
            if let Some((value, _)) = entries.iter().find(|(_, entry)| entry.index == index) {
                return Some((kind.into(), value.clone()));
            }
        }
        if let Some((regex, _)) = self.regexes.iter().find(|(_, entry)| entry.index == index) {
            return Some(("domain_regex".into(), regex.as_str().into()));
        }
        if let Some((value, _)) = self.port.iter().find(|(_, entry)| entry.index == index) {
            return Some(("port".into(), value.to_string()));
        }
        if let Some((value, _)) = self.network.iter().find(|(_, entry)| entry.index == index) {
            return Some((
                "network".into(),
                if *value == 1 { "tcp" } else { "udp" }.into(),
            ));
        }
        for (bits, entries) in &self.v4 {
            if let Some((value, _)) = entries.iter().find(|(_, entry)| entry.index == index) {
                return Some((
                    "ip_cidr".into(),
                    format!("{}/{bits}", std::net::Ipv4Addr::from(*value)),
                ));
            }
        }
        for (bits, entries) in &self.v6 {
            if let Some((value, _)) = entries.iter().find(|(_, entry)| entry.index == index) {
                return Some((
                    "ip_cidr".into(),
                    format!("{}/{bits}", std::net::Ipv6Addr::from(*value)),
                ));
            }
        }
        None
    }
}

fn normalize_domain(s: &str) -> Result<String> {
    let d = s.trim_end_matches('.').to_ascii_lowercase();
    if d.is_empty()
        || d.len() > 253
        || d.chars().any(char::is_whitespace)
        || d.contains([',', '/', '\\', '+', '\0'])
        || d.split('.').any(str::is_empty)
    {
        bail!("invalid domain rule: {s}");
    }
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_ipc::{Inbound, Target, API_VERSION};
    fn decision(rules: &str, host: &str, port: u16) -> RouteAction {
        let config: RuleConfig = toml::from_str(&format!(
            "socket = '/tmp/test.sock'\ndefault_action = 'direct'\n{rules}"
        ))
        .unwrap();
        let engine = RuleEngine::compile(&config).unwrap();
        engine.decide(&RouteRequest {
            api_version: API_VERSION,
            id: 1,
            target: Target {
                host: host.to_string(),
                port,
            },
            network: Network::Tcp,
            inbound: Inbound::Socks5,
            process: None,
        })
    }
    fn request(host: &str, process: Option<astra_ipc::ProcessInfo>) -> RouteRequest {
        RouteRequest {
            api_version: API_VERSION,
            id: 1,
            target: Target {
                host: host.into(),
                port: 443,
            },
            network: Network::Tcp,
            inbound: Inbound::Tun,
            process,
        }
    }
    fn compile(settings: &str) -> RuleEngine {
        RuleEngine::compile(
            &toml::from_str::<RuleConfig>(&format!(
                "socket='/tmp/test.sock'\ndefault_action='proxy:edge'\n{settings}"
            ))
            .unwrap(),
        )
        .unwrap()
    }
    #[test]
    fn local_default_covers_private_loopback_link_local_and_mapped_ipv6() {
        let engine = compile("");
        for ip in [
            "10.0.0.1",
            "10.255.255.255",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.0.1",
            "127.0.0.1",
            "169.254.1.2",
            "::1",
            "fc00::1",
            "fdff::1",
            "fe80::1",
            "febf::1",
            "::ffff:192.168.1.1",
        ] {
            assert_eq!(
                engine.decide(&request(ip, None)),
                RouteAction::Direct,
                "{ip}"
            );
        }
        for ip in [
            "8.8.8.8",
            "172.15.255.255",
            "172.32.0.1",
            "192.169.0.1",
            "2001:4860:4860::8888",
            "fec0::1",
            "::ffff:8.8.8.8",
            "example.com",
        ] {
            assert_eq!(
                engine.decide(&request(ip, None)),
                RouteAction::Proxy("edge".into()),
                "{ip}"
            );
        }
        assert_eq!(
            compile("direct_local=false").decide(&request("192.168.1.1", None)),
            RouteAction::Proxy("edge".into())
        );
        assert_eq!(
            compile("[[rules]]\nkind='ip_cidr'\nvalue='10.0.0.0/8'\naction='block'")
                .decide(&request("10.1.2.3", None)),
            RouteAction::Block
        );
    }
    #[test]
    fn process_rules_use_observed_identity_and_preserve_first_match_order() {
        let process = astra_ipc::ProcessInfo {
            pid: 42,
            name: "Browser.EXE".into(),
            path: r"C:\Apps\Browser.exe".into(),
        };
        let engine = compile("[[rules]]\nkind='process_name'\nvalue='browser.exe'\naction='block'\n[[rules]]\nkind='domain_suffix'\nvalue='example.com'\naction='direct'");
        assert_eq!(
            engine.decide(&request("example.com", Some(process.clone()))),
            RouteAction::Block
        );
        assert_eq!(
            engine.decide(&request("example.com", None)),
            RouteAction::Direct
        );
        let engine = compile("[[rules]]\nkind='domain_suffix'\nvalue='example.com'\naction='direct'\n[[rules]]\nkind='process_name'\nvalue='browser.exe'\naction='block'");
        assert_eq!(
            engine.decide(&request("example.com", Some(process.clone()))),
            RouteAction::Direct
        );
        let engine =
            compile("[[rules]]\nkind='process_path'\nvalue='c:/apps/browser.exe'\naction='block'");
        assert_eq!(
            engine.decide(&request("10.1.2.3", Some(process))),
            RouteAction::Block
        );
        assert_eq!(
            engine.decide(&request("8.8.8.8", None)),
            RouteAction::Proxy("edge".into())
        );
        let unix = astra_ipc::ProcessInfo {
            pid: 7,
            name: "browser".into(),
            path: "/usr/bin/Browser".into(),
        };
        assert_eq!(
            compile("[[rules]]\nkind='process_path'\nvalue='/usr/bin/browser'\naction='block'")
                .decide(&request("8.8.8.8", Some(unix))),
            RouteAction::Proxy("edge".into())
        );
    }
    #[test]
    fn invalid_process_rules_are_rejected() {
        for (kind, value) in [
            ("process_name", ""),
            ("process_name", "a/b"),
            ("process_path", "browser.exe"),
            ("process_path", "C:browser.exe"),
        ] {
            let config: RuleConfig = toml::from_str(&format!("socket='/tmp/a'\ndefault_action='direct'\n[[rules]]\nkind='{kind}'\nvalue='{value}'\naction='block'" )).unwrap();
            assert!(RuleEngine::compile(&config).is_err(), "{kind}: {value}");
        }
    }
    #[test]
    fn geoip_dat_categories_keep_priority_provenance_and_no_dns() {
        let dir = std::env::temp_dir().join(format!("astra-geoip-dat-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("geoip.dat");
        std::fs::write(&path, include_bytes!("../tests/data/geoip.dat")).unwrap();
        let config: RuleConfig = toml::from_str("socket='/tmp/a'\ndirect_local=false\ndefault_action='direct'\ngeoip_database='geoip.dat'\n[[rules]]\nkind='ip_cidr'\nvalue='127.0.0.1/32'\naction='proxy:first'\n[[rules]]\nkind='rule_set'\nvalue='ips'\naction='block'\n[[rules]]\nkind='ip_cidr'\nvalue='127.0.0.0/8'\naction='direct'\n[rule_sets.ips]\nbehavior='classical'\npayload=['GEOIP,TeLeGrAm,no-resolve']").unwrap();
        let engine = RuleEngine::compile_at(&config, &dir).unwrap();
        assert_eq!(
            engine.decide(&request("127.0.0.1", None)),
            RouteAction::Proxy("first".into())
        );
        for host in ["127.0.0.2", "::ffff:127.0.0.2", "2001:db8::1"] {
            let decision = engine.decision(&request(host, None));
            assert_eq!(decision.action, RouteAction::Block);
            assert!(decision
                .path
                .to_string()
                .starts_with("rules[2] -> rule_set"));
        }
        for host in ["localhost", "example.com", "192.0.2.1", "2001:db9::1"] {
            assert_eq!(engine.decide(&request(host, None)), RouteAction::Direct);
        }
        let missing: RuleConfig = toml::from_str("socket='/tmp/a'\ndefault_action='direct'\ngeoip_database='geoip.dat'\n[[rules]]\nkind='geoip'\nvalue='missing'\naction='block'").unwrap();
        assert!(
            format!("{:#}", RuleEngine::compile_at(&missing, &dir).unwrap_err())
                .contains("category not found")
        );
        std::fs::write(&path, b"invalid dat").unwrap();
        assert!(RuleEngine::compile_at(&config, &dir).is_err());
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
    #[test]
    fn geoip_country_flat_and_list_databases_match_and_preserve_priority() {
        let dir = std::env::temp_dir().join(format!("astra-geoip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for data in [
            include_bytes!("../tests/data/country.mmdb").as_slice(),
            include_bytes!("../tests/data/flat.mmdb"),
            include_bytes!("../tests/data/list.mmdb"),
        ] {
            std::fs::write(dir.join("test.mmdb"), data).unwrap();
            let config:RuleConfig=toml::from_str("socket='/tmp/a'\ndefault_action='direct'\ngeoip_database='test.mmdb'\n[[rules]]\nkind='geoip'\nvalue='CN'\naction='block'\n[[rules]]\nkind='geoip'\nvalue='US'\naction='proxy:edge'").unwrap();
            let engine = RuleEngine::compile_at(&config, &dir).unwrap();
            assert_eq!(
                engine.decide(&request("127.0.0.1", None)),
                RouteAction::Block
            );
            assert_eq!(
                engine.decide(&request("::ffff:127.0.0.1", None)),
                RouteAction::Block
            );
            assert_eq!(
                engine.decide(&request("200.1.2.3", None)),
                RouteAction::Proxy("edge".into())
            );
            assert_eq!(
                engine.decide(&request("example.com", None)),
                RouteAction::Direct
            );
        }
        std::fs::remove_file(dir.join("test.mmdb")).unwrap();
        std::fs::remove_dir(dir).unwrap();
        assert_eq!(
            compile("[[rules]]\nkind='geoip'\nvalue='LAN'\naction='block'")
                .decide(&request("fd00::1", None)),
            RouteAction::Block
        );
    }
    #[test]
    fn rule_set_expansion_keeps_reference_position_and_relative_paths() {
        let dir = std::env::temp_dir().join(format!("astra-rule-set-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("domains.yaml"),
            "payload: ['+.example.com', '.sub.test', '*.*.test']",
        )
        .unwrap();
        let config:RuleConfig=toml::from_str("socket='/tmp/a'\ndefault_action='direct'\n[[rules]]\nkind='domain_exact'\nvalue='keep.example.com'\naction='direct'\n[[rules]]\nkind='rule_set'\nvalue='domains'\naction='block'\n[[rules]]\nkind='domain_exact'\nvalue='example.com'\naction='proxy:edge'\n[rule_sets.domains]\nbehavior='domain'\npath='domains.yaml'").unwrap();
        let engine = RuleEngine::compile_at(&config, &dir).unwrap();
        assert!(engine
            .decision(&request("a.example.com", None))
            .path
            .to_string()
            .starts_with("rules[2] -> rule_set(\"domains\")"));
        assert_eq!(
            engine.decide(&request("keep.example.com", None)),
            RouteAction::Direct
        );
        for domain in ["example.com", "a.example.com", "x.sub.test", "a.b.test"] {
            assert_eq!(
                engine.decide(&request(domain, None)),
                RouteAction::Block,
                "{domain}"
            );
        }
        for domain in ["notexample.com", "sub.test", "a.test", "a.b.c.test"] {
            assert_eq!(
                engine.decide(&request(domain, None)),
                RouteAction::Direct,
                "{domain}"
            );
        }
        std::fs::remove_file(dir.join("domains.yaml")).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
    #[test]
    fn disabled_rule_sets_are_skipped_without_loading_resources() {
        let mut config: RuleConfig = toml::from_str("socket='/tmp/a'\ndefault_action='proxy:fallback'\ndirect_local=false\n[[rules]]\nkind='rule_set'\nvalue='ads'\naction='block'\n[[rules]]\nkind='domain_exact'\nvalue='next.test'\naction='direct'\n[rule_sets.ads]\nenabled=false\npath='missing.yaml'").unwrap();
        let engine = RuleEngine::compile(&config).unwrap();
        assert_eq!(
            engine.decide(&request("anything.test", None)),
            RouteAction::Proxy("fallback".into())
        );
        let decision = engine.decision(&request("next.test", None));
        assert_eq!(decision.action, RouteAction::Direct);
        assert!(decision.path.to_string().starts_with("rules[2]"));
        config.rule_sets.get_mut("ads").unwrap().enabled = true;
        assert!(RuleEngine::compile(&config).is_err());
    }
    #[test]
    fn native_inline_yaml_and_text_providers_have_identical_decisions() {
        let dir = std::env::temp_dir().join(format!("astra-rule-matrix-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cases = [
            (
                "domain_exact",
                "example.test",
                "DOMAIN,example.test",
                "example.test",
                "other.test",
            ),
            (
                "domain_suffix",
                "example.test",
                "DOMAIN-SUFFIX,example.test",
                "a.example.test",
                "notexample.test",
            ),
            (
                "domain_keyword",
                "needle",
                "DOMAIN-KEYWORD,needle",
                "a.needle.test",
                "other.test",
            ),
            (
                "domain_regex",
                r"^rx[0-9]+\.test$",
                r"DOMAIN-REGEX,^rx[0-9]+\.test$",
                "rx42.test",
                "rx.test",
            ),
            (
                "ip_cidr",
                "198.51.100.0/24",
                "IP-CIDR,198.51.100.0/24",
                "198.51.100.1",
                "203.0.113.1",
            ),
            (
                "ip_cidr",
                "2001:db8::/32",
                "IP-CIDR6,2001:db8::/32",
                "2001:db8::1",
                "2001:db9::1",
            ),
            ("geoip", "LAN", "GEOIP,LAN", "192.168.1.1", "203.0.113.1"),
            ("port", "53", "DST-PORT,53", "example.test", "example.test"),
            (
                "network",
                "udp",
                "NETWORK,udp",
                "example.test",
                "example.test",
            ),
            (
                "process_name",
                "browser.exe",
                "PROCESS-NAME,browser.exe",
                "example.test",
                "example.test",
            ),
            (
                "process_path",
                "C:/Program Files/Browser/browser.exe",
                "PROCESS-PATH,C:/Program Files/Browser/browser.exe",
                "example.test",
                "example.test",
            ),
        ];
        for (kind, value, classical, matched, missed) in cases {
            let mut hit = request(matched, None);
            let miss = request(missed, None);
            if kind == "port" {
                hit.target.port = 53;
            }
            if kind == "network" {
                hit.network = Network::Udp;
            }
            if kind.starts_with("process_") {
                hit.process = Some(astra_ipc::ProcessInfo {
                    pid: 1,
                    name: "Browser.EXE".into(),
                    path: r"c:\Program Files\Browser\browser.exe".into(),
                });
            }
            for mode in ["native", "inline", "yaml", "text"] {
                let mut config: RuleConfig = toml::from_str(
                    "socket='unused'\ndirect_local=false\ndefault_action='proxy:fallback'",
                )
                .unwrap();
                if mode == "native" {
                    config.rules.push(RuleSpec {
                        kind: kind.into(),
                        value: value.into(),
                        action: "block".into(),
                    });
                } else {
                    let entry = format!("{classical},no-resolve");
                    let mut source = astra_rule_data::RuleSet::default();
                    if mode == "inline" {
                        source.payload.push(entry);
                    } else {
                        source.format = mode.into();
                        source.path = Some(format!("provider.{mode}"));
                        let content = if mode == "yaml" {
                            format!("payload:\n  - '{entry}'\n")
                        } else {
                            format!("# original line 2 follows\n{entry}\n")
                        };
                        std::fs::write(dir.join(source.path.as_ref().unwrap()), content).unwrap();
                    }
                    config.rule_sets.insert("provider".into(), source);
                    // Reusing a provider must keep each reference's action and order.
                    for action in ["block", "direct"] {
                        config.rules.push(RuleSpec {
                            kind: "rule_set".into(),
                            value: "provider".into(),
                            action: action.into(),
                        });
                    }
                }
                let engine = RuleEngine::compile_at(&config, &dir).unwrap();
                let decision = engine.decision(&hit);
                assert_eq!(decision.action, RouteAction::Block, "{kind}/{mode}");
                assert!(
                    decision.path.to_string().starts_with("rules[1]"),
                    "{kind}/{mode}"
                );
                assert!(
                    engine.matched_condition(&decision.path).is_some(),
                    "{kind}/{mode}"
                );
                assert_eq!(
                    engine.decide(&miss),
                    RouteAction::Proxy("fallback".into()),
                    "{kind}/{mode}"
                );
            }
        }
        for name in ["provider.yaml", "provider.text"] {
            std::fs::remove_file(dir.join(name)).unwrap();
        }
        std::fs::remove_dir(dir).unwrap();
    }
    #[test]
    fn unused_definitions_validate_without_fetching_and_disabled_payload_can_be_repaired() {
        let mut config: RuleConfig = toml::from_str("socket='unused'\ndefault_action='direct'\n[rule_sets.unused]\npath='missing.yaml'\nurl='http://127.0.0.1:1/not-requested'").unwrap();
        assert!(RuleEngine::compile(&config).is_ok());
        config.rule_sets.get_mut("unused").unwrap().path = Some("  ".into());
        let error = format!("{:#}", RuleEngine::compile(&config).err().unwrap());
        assert!(error.contains("unused") && error.contains("nonempty"));
        let source = config.rule_sets.get_mut("unused").unwrap();
        source.path = None;
        source.url = None;
        source.payload = vec!["DOMAIN-REGEX,[".into()];
        assert!(RuleEngine::compile(&config).is_err());
        config.rule_sets.get_mut("unused").unwrap().enabled = false;
        assert!(RuleEngine::compile(&config).is_ok());
    }
    #[test]
    fn missing_cyclic_and_invalid_sets_reject_the_candidate() {
        for extra in [
            "",
            "[rule_sets.a]\npayload=['RULE-SET,a']",
            "[rule_sets.a]\npayload=['SRC-PORT,80']",
            "[rule_sets.a]\nformat='mrs'\npayload=['DOMAIN,a.test']",
        ] {
            let config:RuleConfig=toml::from_str(&format!("socket='/tmp/a'\ndefault_action='direct'\n[[rules]]\nkind='rule_set'\nvalue='a'\naction='block'\n{extra}")).unwrap();
            assert!(RuleEngine::compile(&config).is_err(), "{extra}");
        }
    }
    #[test]
    fn geosite_dat_matches_types_filters_and_source_order() {
        let dir = std::env::temp_dir().join(format!("astra-geosite-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("geosite.dat"),
            include_bytes!("../tests/data/geosite.dat"),
        )
        .unwrap();
        let config_for = |selector: &str| {
            toml::from_str::<RuleConfig>(&format!("socket='/tmp/a'\ndefault_action='proxy:edge'\ngeosite_database='geosite.dat'\n[[rules]]\nkind='domain_exact'\nvalue='keep.suffix.test'\naction='direct'\n[[rules]]\nkind='geosite'\nvalue='{selector}'\naction='block'\n[[rules]]\nkind='domain_exact'\nvalue='exact.test'\naction='proxy:later'")).unwrap()
        };
        let engine = RuleEngine::compile_at(&config_for("TEST"), &dir).unwrap();
        for host in [
            "EXACT.TEST.",
            "suffix.test",
            "a.suffix.test",
            "a.keyword.test",
            "rx42.test",
        ] {
            assert_eq!(
                engine.decide(&request(host, None)),
                RouteAction::Block,
                "{host}"
            );
        }
        for host in [
            "notsuffix.test",
            "a.exact.test",
            "rx.test",
            "rx42.test.other",
        ] {
            assert!(
                matches!(engine.decide(&request(host, None)), RouteAction::Proxy(_)),
                "{host}"
            );
        }
        assert_eq!(
            engine.decide(&request("keep.suffix.test", None)),
            RouteAction::Direct
        );
        let engine = RuleEngine::compile_at(&config_for("test@cn"), &dir).unwrap();
        assert_eq!(
            engine.decide(&request("suffix.test", None)),
            RouteAction::Block
        );
        assert_eq!(
            engine.decide(&request("exact.test", None)),
            RouteAction::Proxy("later".into())
        );
        let engine = RuleEngine::compile_at(&config_for("test@!cn"), &dir).unwrap();
        assert_eq!(
            engine.decide(&request("exact.test", None)),
            RouteAction::Block
        );
        assert_eq!(
            engine.decide(&request("suffix.test", None)),
            RouteAction::Proxy("edge".into())
        );
        let engine = RuleEngine::compile_at(&config_for("test@cn@fast"), &dir).unwrap();
        assert_eq!(
            engine.decide(&request("rx42.test", None)),
            RouteAction::Block
        );
        assert!(RuleEngine::compile_at(&config_for("missing"), &dir).is_err());
        std::fs::write(dir.join("geosite.dat"), b"invalid").unwrap();
        assert!(RuleEngine::compile_at(&config_for("test"), &dir).is_err());
        std::fs::remove_file(dir.join("geosite.dat")).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
    #[test]
    fn native_domains_reject_invalid_labels_and_wildcard_ambiguity() {
        for kind in [
            "domain_exact",
            "domain_suffix",
            "domain_subdomain",
            "domain_pattern",
        ] {
            for value in ["bad..test", "bad\ttest", "bad/test", "bad+test"] {
                let mut config: RuleConfig =
                    toml::from_str("socket='unused'\ndefault_action='direct'").unwrap();
                config.rules.push(RuleSpec {
                    kind: kind.into(),
                    value: value.into(),
                    action: "block".into(),
                });
                assert!(RuleEngine::compile(&config).is_err(), "{kind}: {value:?}");
            }
        }
        for kind in ["domain_exact", "domain_suffix", "domain_subdomain"] {
            let mut config: RuleConfig =
                toml::from_str("socket='unused'\ndefault_action='direct'").unwrap();
            config.rules.push(RuleSpec {
                kind: kind.into(),
                value: "*.test".into(),
                action: "block".into(),
            });
            assert!(RuleEngine::compile(&config).is_err(), "{kind}");
        }
    }
    #[test]
    fn domain_suffix_respects_label_boundaries() {
        let spec = "[[rules]]\nkind='domain_suffix'\nvalue='example.com'\naction='block'";
        assert_eq!(decision(spec, "a.EXAMPLE.com", 443), RouteAction::Block);
        assert_eq!(decision(spec, "notexample.com", 443), RouteAction::Direct);
    }
    #[test]
    fn earliest_rule_wins_across_indices() {
        let spec = "[[rules]]\nkind='port'\nvalue='443'\naction='block'\n[[rules]]\nkind='domain_suffix'\nvalue='example.com'\naction='proxy:edge'";
        assert_eq!(decision(spec, "example.com", 443), RouteAction::Block);
        assert_eq!(
            decision(spec, "example.com", 80),
            RouteAction::Proxy("edge".to_owned())
        );
    }
    #[test]
    #[ignore = "manual benchmark; run cargo test -p astra-rule --release -- --ignored --nocapture"]
    fn benchmark_mixed_rules() {
        use std::time::Instant;
        let config: RuleConfig = toml::from_str("socket = '/tmp/test.sock'\ndefault_action = 'direct'\n[[rules]]\nkind='domain_suffix'\nvalue='example.com'\naction='block'\n").unwrap();
        let engine = RuleEngine::compile(&config).unwrap();
        let req = RouteRequest {
            api_version: API_VERSION,
            id: 1,
            target: Target {
                host: "www.example.com".to_owned(),
                port: 443,
            },
            network: Network::Tcp,
            inbound: Inbound::Socks5,
            process: None,
        };
        let start = Instant::now();
        for _ in 0..1_000_000 {
            std::hint::black_box(engine.decide(std::hint::black_box(&req)));
        }
        eprintln!(
            "1M rule queries elapsed: {:?} (not a cross-platform benchmark)",
            start.elapsed()
        );
    }

    #[test]
    fn cidr_v4_and_v6() {
        let spec = "[[rules]]\nkind='ip_cidr'\nvalue='10.0.0.0/8'\naction='block'\n[[rules]]\nkind='ip_cidr'\nvalue='2001:db8::/32'\naction='block'";
        assert_eq!(decision(spec, "10.99.0.1", 80), RouteAction::Block);
        assert_eq!(decision(spec, "2001:db8::1", 80), RouteAction::Block);
        assert_eq!(decision(spec, "11.0.0.1", 80), RouteAction::Direct);
    }
}
