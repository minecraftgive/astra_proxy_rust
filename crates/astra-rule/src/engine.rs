use anyhow::{bail, Context, Result};
use astra_model::Network;
use astra_model::RouteAction;
use astra_model::RouteRequest;
use ipnet::IpNet;
use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap},
    net::IpAddr,
    path::Path,
};

pub use astra_model::config::rule::{RuleConfig, RuleSpec};
use astra_rule_data::RuleSetExt;
#[derive(Debug, Clone, Copy)]
struct Entry {
    index: u32,
    action: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    RuleSet,
    Geosite,
    Geoip,
    DomainKeyword,
    DomainRegex,
    DomainSubdomain,
    DomainPattern,
    ProcessName,
    ProcessPath,
    DomainExact,
    DomainSuffix,
    IpCidr,
    Port,
    Network,
}
impl Kind {
    fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "rule_set" => Self::RuleSet,
            "geosite" => Self::Geosite,
            "geoip" => Self::Geoip,
            "domain_keyword" => Self::DomainKeyword,
            "domain_regex" => Self::DomainRegex,
            "domain_subdomain" => Self::DomainSubdomain,
            "domain_pattern" => Self::DomainPattern,
            "process_name" => Self::ProcessName,
            "process_path" => Self::ProcessPath,
            "domain_exact" => Self::DomainExact,
            "domain_suffix" => Self::DomainSuffix,
            "ip_cidr" => Self::IpCidr,
            "port" => Self::Port,
            "network" => Self::Network,
            other => bail!("unsupported rule kind: {other}"),
        })
    }
}

struct ExpandedRule {
    kind: Kind,
    value: String,
    action: u32,
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

type CompiledPattern = (Box<[Box<str>]>, Entry);

#[derive(Debug)]
pub struct RuleEngine {
    sources: Vec<RuleSource>,
    actions: Vec<RouteAction>,
    default: RouteAction,
    direct_local: bool,
    geoip: Option<GeoDatabase>,
    countries: HashMap<Box<str>, Entry>,
    keywords: Vec<(Box<str>, Entry)>,
    keyword_matcher: Option<aho_corasick::AhoCorasick>,
    regexes: Vec<(regex::Regex, Entry)>,
    /// All `regexes` in one automaton: a lookup scans the domain once instead
    /// of once per pattern. `None` only if the combined set exceeds its limits.
    regex_set: Option<regex::RegexSet>,
    patterns: HashMap<usize, Vec<CompiledPattern>>,
    subdomains: HashMap<Box<str>, Entry>,
    process_name: HashMap<Box<str>, Entry>,
    process_path: HashMap<Box<str>, Entry>,
    exact: HashMap<Box<str>, Entry>,
    suffix: HashMap<Box<str>, Entry>,
    port: HashMap<u16, Entry>,
    network: HashMap<u8, Entry>,
    v4: Vec<(u8, HashMap<u32, Entry>)>,
    v6: Vec<(u8, HashMap<u128, Entry>)>,
}

fn compile_regex(pattern: &str) -> Result<regex::Regex> {
    astra_rule_data::compile_domain_regex(pattern)
}

/// Each pattern was already validated alone by `compile_regex`; the set uses
/// the same flags and a limit that grows with the number of patterns.
fn regex_set(regexes: &[(regex::Regex, Entry)]) -> Option<regex::RegexSet> {
    if regexes.is_empty() {
        return None;
    }
    regex::RegexSetBuilder::new(regexes.iter().map(|(regex, _)| regex.as_str()))
        .case_insensitive(true)
        .size_limit((2 * 1024 * 1024 * regexes.len()).min(256 * 1024 * 1024))
        .build()
        .inspect_err(|error| {
            tracing::warn!(%error, "domain regexes are matched one by one");
        })
        .ok()
}

fn lowercase(value: &str) -> Cow<'_, str> {
    if value.is_ascii() && !value.bytes().any(|b| b.is_ascii_uppercase()) {
        Cow::Borrowed(value)
    } else {
        Cow::Owned(value.to_lowercase())
    }
}
fn process_path(value: &str) -> Cow<'_, str> {
    if value.contains('\\') {
        Cow::Owned(value.replace('\\', "/").to_lowercase())
    } else if value.as_bytes().get(1) == Some(&b':') {
        lowercase(value)
    } else {
        Cow::Borrowed(value)
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

fn resource_unavailable(error: &anyhow::Error) -> bool {
    if error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .any(|cause| {
            matches!(
                cause.kind(),
                std::io::ErrorKind::NotFound
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::NotConnected
            )
        })
    {
        return true;
    }

    let text = format!("{error:#}").to_ascii_lowercase();
    (text.contains("resource download failed")
        || text.contains("no such file")
        || text.contains("not found")
        || text.contains("timed out")
        || text.contains("connection refused")
        || text.contains("network is unreachable"))
        && !text.contains("invalid")
        && !text.contains("unsupported")
        && !text.contains("corrupt")
        && !text.contains("category not found")
}

fn skip_unavailable_rules(
    rules: &mut Vec<ExpandedRule>,
    sources: &mut [RuleSource],
    skip: impl Fn(&ExpandedRule) -> bool,
) {
    let original = std::mem::take(rules);
    let mut retained = Vec::with_capacity(original.len());
    let mut input = original.into_iter();
    let mut previous_end = 0;
    for source in sources {
        let count = source.end - previous_end;
        previous_end = source.end;
        retained.extend(input.by_ref().take(count).filter(|rule| !skip(rule)));
        source.end = retained.len();
    }
    retained.extend(input.filter(|rule| !skip(rule)));
    *rules = retained;
}

use astra_model::config::rule::parse_action;

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
        Self::compile_with_loader(config, base, &astra_resource::ResourceLoader::default())
    }
    pub fn compile_with_loader(
        config: &RuleConfig,
        base: &Path,
        loader: &astra_resource::ResourceLoader,
    ) -> Result<Self> {
        config.validate_structure()?;
        fn expand(
            spec: ExpandedRule,
            config: &RuleConfig,
            base: &Path,
            loader: &astra_resource::ResourceLoader,
            active: &mut Vec<String>,
            cache: &mut HashMap<String, Vec<(Kind, String)>>,
            output: &mut Vec<ExpandedRule>,
        ) -> Result<()> {
            anyhow::ensure!(
                output.len() < 1_000_000,
                "expanded rules exceed 1000000 entries"
            );
            if spec.kind != Kind::RuleSet {
                output.push(spec);
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
                let loaded = source
                    .load_with(base, loader)
                    .with_context(|| format!("load rule-set {}", spec.value));
                let loaded = match loaded {
                    Ok(value) => value,
                    Err(error) if resource_unavailable(&error) => {
                        tracing::warn!(rule_set=%spec.value, error=%error, "rule-set unavailable; skipping dependent rules");
                        cache.insert(spec.value.clone(), Vec::new());
                        return Ok(());
                    }
                    Err(error) => return Err(error),
                };
                let loaded = loaded
                    .into_iter()
                    .map(|matcher| Ok((Kind::parse(&matcher.kind)?, matcher.value)))
                    .collect::<Result<_>>()?;
                cache.insert(spec.value.clone(), loaded);
            }
            active.push(spec.value.clone());
            for index in 0..cache[&spec.value].len() {
                let (kind, value) = &cache[&spec.value][index];
                let nested = ExpandedRule {
                    kind: *kind,
                    value: value.clone(),
                    action: spec.action,
                };
                expand(nested, config, base, loader, active, cache, output)?;
            }
            active.pop();
            Ok(())
        }
        let mut rules = Vec::new();
        let mut sources = Vec::new();
        let mut cache = HashMap::new();
        let mut actions = Vec::new();
        let mut action_ids = HashMap::new();
        for (index, spec) in config.rules.iter().enumerate() {
            let action = match action_ids.get(spec.action.as_str()) {
                Some(&id) => id,
                None => {
                    let action = parse_action(&spec.action)
                        .with_context(|| format!("rules[{}] action", index + 1))?;
                    let id = u32::try_from(actions.len()).context("too many rule actions")?;
                    actions.push(action);
                    action_ids.insert(spec.action.as_str(), id);
                    id
                }
            };
            expand(
                ExpandedRule {
                    kind: Kind::parse(&spec.kind)?,
                    value: spec.value.clone(),
                    action,
                },
                config,
                base,
                loader,
                &mut Vec::new(),
                &mut cache,
                &mut rules,
            )
            .with_context(|| format!("rules[{}]", index + 1))?;
            sources.push(RuleSource {
                index: index + 1,
                end: rules.len(),
                kind: spec.kind.clone(),
                value: spec.value.clone(),
            });
        }
        drop(cache);
        drop(action_ids);
        if rules.iter().any(|rule| rule.kind == Kind::Geosite) {
            let path = match config.geosite_database.as_deref() {
                Some(path) => path,
                None => {
                    tracing::warn!("GEOSITE database unavailable; skipping dependent rules");
                    skip_unavailable_rules(&mut rules, &mut sources, |rule| {
                        rule.kind == Kind::Geosite
                    });
                    ""
                }
            };
            if path.is_empty() {
                // The remaining rules do not require a GEOSITE database.
            } else {
                let selectors: Vec<_> = rules
                    .iter()
                    .filter(|r| r.kind == Kind::Geosite)
                    .map(|r| r.value.clone())
                    .collect();
                let database = loader.load_resource_with(
                    base,
                    path,
                    config.geosite_url.as_deref(),
                    config.geosite_interval,
                    128 * 1024 * 1024,
                    |bytes| {
                        let db =
                            astra_rule_data::geosite::Database::decode_selected(bytes, &selectors)?;
                        let mut count = rules
                            .iter()
                            .filter(|rule| rule.kind != Kind::Geosite)
                            .count();
                        for selector in &selectors {
                            db.expand_with(selector, |kind, value| {
                                count += 1;
                                anyhow::ensure!(
                                    count <= 1_000_000,
                                    "expanded rules exceed 1000000 entries"
                                );
                                if kind == "domain_regex" {
                                    compile_regex(value)?;
                                } else if kind == "domain_keyword" {
                                    anyhow::ensure!(
                                        !value.chars().any(char::is_whitespace),
                                        "invalid GEOSITE keyword"
                                    );
                                } else {
                                    normalize_domain(value)?;
                                }
                                Ok(())
                            })?;
                        }
                        Ok(db)
                    },
                );
                let database = match database {
                    Ok((_, database)) => Some(database),
                    Err(error) if resource_unavailable(&error) => {
                        tracing::warn!(error=%error, "GEOSITE database unavailable; skipping dependent rules");
                        skip_unavailable_rules(&mut rules, &mut sources, |rule| {
                            rule.kind == Kind::Geosite
                        });
                        None
                    }
                    Err(error) => return Err(error),
                };
                if let Some(database) = database {
                    let mut expanded = Vec::new();
                    let mut input = rules.into_iter();
                    let mut previous_end = 0;
                    for source in &mut sources {
                        let count = source.end - previous_end;
                        previous_end = source.end;
                        for rule in input.by_ref().take(count) {
                            if rule.kind == Kind::Geosite {
                                database.expand_with(&rule.value, |kind, value| {
                                    anyhow::ensure!(
                                        expanded.len() < 1_000_000,
                                        "expanded rules exceed 1000000 entries"
                                    );
                                    expanded.push(ExpandedRule {
                                        kind: Kind::parse(kind)?,
                                        value: value.into(),
                                        action: rule.action,
                                    });
                                    Ok(())
                                })?;
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
            }
        }
        let geoip_categories = rules
            .iter()
            .filter(|r| r.kind == Kind::Geoip && !r.value.eq_ignore_ascii_case("LAN"))
            .map(|r| astra_rule_data::geoip::category(&r.value))
            .collect::<Result<Vec<_>>>()?;
        let geoip = if !geoip_categories.is_empty() {
            let path = config.geoip_database.as_deref();
            if let Some(path) = path {
                match loader.load_resource_with(
                    base,
                    path,
                    config.geoip_url.as_deref(),
                    config.geoip_interval,
                    128 * 1024 * 1024,
                    |bytes| GeoDatabase::decode(bytes, &geoip_categories),
                ) {
                    Ok((_, database)) => Some(database),
                    Err(error) if resource_unavailable(&error) => {
                        tracing::warn!(error=%error, "GEOIP database unavailable; skipping dependent rules");
                        skip_unavailable_rules(&mut rules, &mut sources, |rule| {
                            rule.kind == Kind::Geoip && !rule.value.eq_ignore_ascii_case("LAN")
                        });
                        None
                    }
                    Err(error) => return Err(error),
                }
            } else {
                tracing::warn!("GEOIP database unavailable; skipping dependent rules");
                skip_unavailable_rules(&mut rules, &mut sources, |rule| {
                    rule.kind == Kind::Geoip && !rule.value.eq_ignore_ascii_case("LAN")
                });
                None
            }
        } else {
            None
        };
        let mut out = Self {
            sources,
            actions,
            default: parse_action(&config.default_action)?,
            direct_local: config.direct_local,
            geoip,
            countries: HashMap::new(),
            keywords: Vec::new(),
            keyword_matcher: None,
            regexes: Vec::new(),
            regex_set: None,
            patterns: HashMap::new(),
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
        for (index, r) in rules.into_iter().enumerate() {
            let entry = Entry {
                index: u32::try_from(index).context("too many expanded rules")?,
                action: r.action,
            };
            match r.kind {
                Kind::Geoip => {
                    let code = astra_rule_data::geoip::category(&r.value)?;
                    out.countries.entry(code.into_boxed_str()).or_insert(entry);
                }
                Kind::DomainKeyword => {
                    anyhow::ensure!(
                        !r.value.is_empty()
                            && r.value.len() <= 253
                            && !r.value.chars().any(char::is_whitespace),
                        "invalid domain keyword"
                    );
                    out.keywords
                        .push((r.value.to_ascii_lowercase().into_boxed_str(), entry));
                }
                Kind::DomainRegex => {
                    out.regexes.push((compile_regex(&r.value)?, entry));
                }
                Kind::DomainSubdomain => {
                    anyhow::ensure!(!r.value.contains('*'), "use domain_pattern for wildcards");
                    let key = normalize_domain(&r.value)?;
                    out.subdomains.entry(key.into_boxed_str()).or_insert(entry);
                }
                Kind::DomainPattern => {
                    let key = normalize_domain(&r.value)?;
                    anyhow::ensure!(
                        key.split('.')
                            .all(|part| !part.is_empty() && (!part.contains('*') || part == "*")),
                        "invalid domain pattern"
                    );
                    let labels: Box<[Box<str>]> = key.split('.').map(Box::from).collect();
                    out.patterns
                        .entry(labels.len())
                        .or_default()
                        .push((labels, entry));
                }
                Kind::ProcessName | Kind::ProcessPath => {
                    if r.value.trim().is_empty() || r.value.len() > 4096 || r.value.contains('\0') {
                        bail!("invalid process rule: {}", r.value);
                    }
                    if r.kind == Kind::ProcessName {
                        if r.value.contains(['/', '\\']) {
                            bail!("process_name expects a file name");
                        }
                        out.process_name
                            .entry(r.value.to_lowercase().into_boxed_str())
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
                            .entry(process_path(&r.value).into_owned().into_boxed_str())
                            .or_insert(entry);
                    }
                }
                Kind::DomainExact => {
                    anyhow::ensure!(!r.value.contains('*'), "use domain_pattern for wildcards");
                    let key = normalize_domain(&r.value)?;
                    out.exact.entry(key.into_boxed_str()).or_insert(entry);
                }
                Kind::DomainSuffix => {
                    anyhow::ensure!(!r.value.contains('*'), "use domain_pattern for wildcards");
                    let key = normalize_domain(&r.value)?;
                    out.suffix.entry(key.into_boxed_str()).or_insert(entry);
                }
                Kind::IpCidr => {
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
                Kind::Port => {
                    let port: u16 = r
                        .value
                        .parse()
                        .with_context(|| format!("invalid port {}", r.value))?;
                    if port == 0 {
                        bail!("port 0 is not a valid destination");
                    }
                    out.port.entry(port).or_insert(entry);
                }
                Kind::Network => {
                    let proto = match r.value.as_str() {
                        "tcp" => 1,
                        "udp" => 2,
                        _ => bail!("invalid network: {}", r.value),
                    };
                    out.network.entry(proto).or_insert(entry);
                }
                Kind::RuleSet | Kind::Geosite => unreachable!("rules were expanded"),
            }
        }
        out.v4 = v4.into_iter().collect();
        out.v6 = v6.into_iter().collect();
        out.regex_set = regex_set(&out.regexes);
        if !out.keywords.is_empty() {
            out.keyword_matcher = Some(
                aho_corasick::AhoCorasick::new(
                    out.keywords.iter().map(|(keyword, _)| keyword.as_ref()),
                )
                .context("compile domain keyword matcher")?,
            );
        }
        out.sources.shrink_to_fit();
        out.actions.shrink_to_fit();
        out.countries.shrink_to_fit();
        out.keywords.shrink_to_fit();
        out.regexes.shrink_to_fit();
        for patterns in out.patterns.values_mut() {
            patterns.shrink_to_fit();
        }
        out.patterns.shrink_to_fit();
        out.subdomains.shrink_to_fit();
        out.process_name.shrink_to_fit();
        out.process_path.shrink_to_fit();
        out.exact.shrink_to_fit();
        out.suffix.shrink_to_fit();
        out.port.shrink_to_fit();
        out.network.shrink_to_fit();
        for (_, bucket) in &mut out.v4 {
            bucket.shrink_to_fit();
        }
        for (_, bucket) in &mut out.v6 {
            bucket.shrink_to_fit();
        }
        out.v4.shrink_to_fit();
        out.v6.shrink_to_fit();
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
                self.process_name.get(lowercase(&process.name).as_ref()),
            );
            consider(
                &mut best,
                self.process_path.get(process_path(&process.path).as_ref()),
            );
        }

        consider(&mut best, self.port.get(&req.target.port));
        let netkey = match req.network {
            Network::Tcp => 1,
            Network::Udp => 2,
        };
        consider(&mut best, self.network.get(&netkey));

        let host_ip = req.target.host.parse::<IpAddr>().ok();
        let destination_ip = req.destination_ip.or(host_ip);
        if let Some(ip) = destination_ip {
            if local_ip(ip) {
                consider(&mut best, self.countries.get("LAN"));
            }
            if let Some(GeoDatabase::Dat(db)) = &self.geoip {
                for (name, entry) in &self.countries {
                    if name.as_ref() != "LAN" && db.matches(name, ip) {
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
                            consider(
                                &mut best,
                                self.countries.get(code.to_ascii_uppercase().as_str()),
                            );
                        }
                    } else if let Ok(Some(code)) = result.decode::<String>() {
                        consider(
                            &mut best,
                            self.countries.get(code.to_ascii_uppercase().as_str()),
                        );
                    } else if let Ok(Some(codes)) = result.decode::<Vec<String>>() {
                        for code in codes {
                            consider(
                                &mut best,
                                self.countries.get(code.to_ascii_uppercase().as_str()),
                            );
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
        }
        if host_ip.is_none() {
            let domain = req.target.host.trim_end_matches('.').to_ascii_lowercase();
            consider(&mut best, self.exact.get(domain.as_str()));
            match &self.regex_set {
                Some(set) => {
                    for index in set.matches(&domain).iter() {
                        consider(&mut best, Some(&self.regexes[index].1));
                    }
                }
                None => {
                    for (pattern, entry) in &self.regexes {
                        if pattern.is_match(&domain) {
                            consider(&mut best, Some(entry));
                        }
                    }
                }
            }
            if let Some(matcher) = &self.keyword_matcher {
                // Overlapping matches preserve earliest-rule precedence even
                // when an earlier keyword is contained in a later one.
                for matched in matcher.find_overlapping_iter(domain.as_bytes()) {
                    consider(
                        &mut best,
                        Some(&self.keywords[matched.pattern().as_usize()].1),
                    );
                }
            }
            if !self.patterns.is_empty() {
                let labels: Vec<_> = domain.split('.').collect();
                if let Some(patterns) = self.patterns.get(&labels.len()) {
                    for (expected, entry) in patterns {
                        if labels.iter().zip(expected.iter()).all(|(label, expected)| {
                            expected.as_ref() == "*" || *label == expected.as_ref()
                        }) {
                            consider(&mut best, Some(entry));
                        }
                    }
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
            let source = &self.sources[self
                .sources
                .partition_point(|s| s.end <= entry.index as usize)];
            Decision {
                action: self.actions[entry.action as usize].clone(),
                path: DecisionPath::Rule {
                    source,
                    expanded_index: entry.index as usize + 1,
                },
            }
        } else if self.direct_local && destination_ip.is_some_and(local_ip) {
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
            if let Some((value, _)) = entries
                .iter()
                .find(|(_, entry)| entry.index as usize == index)
            {
                return Some((kind.into(), value.to_string()));
            }
        }
        if let Some((value, _)) = self
            .keywords
            .iter()
            .find(|(_, entry)| entry.index as usize == index)
        {
            return Some(("domain_keyword".into(), value.to_string()));
        }
        for patterns in self.patterns.values() {
            if let Some((labels, _)) = patterns
                .iter()
                .find(|(_, entry)| entry.index as usize == index)
            {
                return Some(("domain_pattern".into(), labels.join(".")));
            }
        }
        if let Some((regex, _)) = self
            .regexes
            .iter()
            .find(|(_, entry)| entry.index as usize == index)
        {
            return Some(("domain_regex".into(), regex.as_str().into()));
        }
        if let Some((value, _)) = self
            .port
            .iter()
            .find(|(_, entry)| entry.index as usize == index)
        {
            return Some(("port".into(), value.to_string()));
        }
        if let Some((value, _)) = self
            .network
            .iter()
            .find(|(_, entry)| entry.index as usize == index)
        {
            return Some((
                "network".into(),
                if *value == 1 { "tcp" } else { "udp" }.into(),
            ));
        }
        for (bits, entries) in &self.v4 {
            if let Some((value, _)) = entries
                .iter()
                .find(|(_, entry)| entry.index as usize == index)
            {
                return Some((
                    "ip_cidr".into(),
                    format!("{}/{bits}", std::net::Ipv4Addr::from(*value)),
                ));
            }
        }
        for (bits, entries) in &self.v6 {
            if let Some((value, _)) = entries
                .iter()
                .find(|(_, entry)| entry.index as usize == index)
            {
                return Some((
                    "ip_cidr".into(),
                    format!("{}/{bits}", std::net::Ipv6Addr::from(*value)),
                ));
            }
        }
        None
    }
}

use astra_model::config::rule::normalize_domain;

#[cfg(test)]
#[path = "test/engine_test.rs"]
mod tests;
