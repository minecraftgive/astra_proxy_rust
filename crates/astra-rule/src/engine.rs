use anyhow::{bail, Context, Result};
use astra_ipc::{Network, RouteAction, RouteRequest};
use ipnet::IpNet;
use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashMap},
    net::IpAddr,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleConfig {
    pub socket: String,
    #[serde(default)]
    pub watch_config: bool,
    pub default_action: String,
    #[serde(default)]
    pub rules: Vec<RuleSpec>,
}

#[derive(Debug, Deserialize)]
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
pub struct RuleEngine {
    default: RouteAction,
    exact: HashMap<String, Entry>,
    suffix: HashMap<String, Entry>,
    port: HashMap<u16, Entry>,
    network: HashMap<u8, Entry>,
    v4: Vec<(u8, HashMap<u32, Entry>)>,
    v6: Vec<(u8, HashMap<u128, Entry>)>,
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
    pub fn compile(config: &RuleConfig) -> Result<Self> {
        let mut out = Self {
            default: parse_action(&config.default_action)?,
            exact: HashMap::new(),
            suffix: HashMap::new(),
            port: HashMap::new(),
            network: HashMap::new(),
            v4: Vec::new(),
            v6: Vec::new(),
        };
        let mut v4: BTreeMap<u8, HashMap<u32, Entry>> = BTreeMap::new();
        let mut v6: BTreeMap<u8, HashMap<u128, Entry>> = BTreeMap::new();
        for (index, r) in config.rules.iter().enumerate() {
            let entry = Entry {
                index,
                action: parse_action(&r.action)?,
            };
            match r.kind.as_str() {
                "domain_exact" => {
                    let key = normalize_domain(&r.value)?;
                    out.exact.entry(key).or_insert(entry);
                }
                "domain_suffix" => {
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

    pub fn decide(&self, req: &RouteRequest) -> RouteAction {
        let mut best: Option<&Entry> = None;

        consider(&mut best, self.port.get(&req.target.port));
        let netkey = match req.network {
            Network::Tcp => 1,
            Network::Udp => 2,
        };
        consider(&mut best, self.network.get(&netkey));

        if let Ok(ip) = req.target.host.parse::<IpAddr>() {
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
            let mut segment = domain.as_str();
            loop {
                consider(&mut best, self.suffix.get(segment));
                if let Some((_, rest)) = segment.split_once('.') {
                    segment = rest;
                } else {
                    break;
                }
            }
        }
        best.map(|x| x.action.clone())
            .unwrap_or_else(|| self.default.clone())
    }
}

fn normalize_domain(s: &str) -> Result<String> {
    let d = s.trim_end_matches('.').to_ascii_lowercase();
    if d.is_empty() || d.len() > 253 || d.contains(' ') || d.starts_with('.') || d.ends_with('.') {
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
        })
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
