use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    pub rule_sets: BTreeMap<String, super::ruleset::RuleSet>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleSpec {
    pub kind: String,
    pub value: String,
    pub action: String,
}

fn geoip_interval() -> u64 {
    86400
}

fn enabled() -> bool {
    true
}

pub fn parse_action(input: &str) -> anyhow::Result<crate::RouteAction> {
    match input {
        "direct" => Ok(crate::RouteAction::Direct),
        "block" => Ok(crate::RouteAction::Block),
        s if s.starts_with("proxy:") && s.len() > 6 && s.len() <= 70 => {
            Ok(crate::RouteAction::Proxy(s[6..].to_owned()))
        }
        _ => anyhow::bail!("invalid action: {input} (expected direct, block, proxy:TAG)"),
    }
}

impl RuleConfig {
    pub fn validate_structure(&self) -> anyhow::Result<()> {
        use anyhow::Context;
        use std::collections::HashSet;
        parse_action(&self.default_action)?;
        // Validate definitions without downloading unused or disabled resources.
        let referenced: HashSet<&str> = self
            .rules
            .iter()
            .filter(|r| r.kind == "rule_set")
            .map(|r| r.value.as_str())
            .collect();
        for (name, source) in &self.rule_sets {
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
        for (index, spec) in self.rules.iter().enumerate() {
            parse_action(&spec.action).with_context(|| format!("rules[{}] action", index + 1))?;
            spec.validate_match()
                .with_context(|| format!("rules[{}]", index + 1))?;
            if spec.kind == "rule_set" {
                anyhow::ensure!(
                    self.rule_sets.contains_key(&spec.value),
                    "unknown rule-set: {}",
                    spec.value
                );
            }
        }
        Ok(())
    }
}

impl RuleSpec {
    /// Validate a matcher without opening its referenced resource.
    pub fn validate_match(&self) -> anyhow::Result<()> {
        use anyhow::{ensure, Context};
        let input = &self.value;
        match self.kind.as_str() {
            "domain_exact" | "domain_suffix" | "domain_subdomain" => {
                ensure!(!input.contains('*'), "use domain_pattern for wildcards");
                normalize_domain(input)?;
            }
            "domain_pattern" => {
                let key = normalize_domain(input)?;
                ensure!(
                    key.split('.')
                        .all(|part| !part.is_empty() && (!part.contains('*') || part == "*")),
                    "invalid domain pattern"
                );
            }
            "domain_keyword" => ensure!(
                !input.is_empty() && input.len() <= 253 && !input.chars().any(char::is_whitespace),
                "invalid domain keyword"
            ),
            "domain_regex" => {
                super::ruleset::compile_domain_regex(input)?;
            }
            "geoip" => {
                super::ruleset::geoip_category(input)?;
            }
            "geosite" => {
                super::ruleset::geosite_selector(input)?;
            }
            "ip_cidr" => {
                input.parse::<ipnet::IpNet>().context("invalid CIDR")?;
            }
            "port" => ensure!(
                input.parse::<u16>().is_ok_and(|p| p > 0),
                "invalid destination port"
            ),
            "network" => ensure!(matches!(input.as_str(), "tcp" | "udp"), "invalid network"),
            "process_name" | "process_path" => {
                ensure!(
                    !input.trim().is_empty() && input.len() <= 4096 && !input.contains('\0'),
                    "invalid process rule"
                );
                if self.kind == "process_name" {
                    ensure!(
                        !input.contains(['/', '\\']),
                        "process_name expects a file name"
                    );
                } else {
                    ensure!(
                        input.starts_with('/')
                            || input.starts_with("\\\\")
                            || (input.as_bytes().get(1) == Some(&b':')
                                && matches!(input.as_bytes().get(2), Some(b'/' | b'\\'))),
                        "process_path expects an absolute executable path"
                    );
                }
            }
            "rule_set" => ensure!(!input.trim().is_empty(), "empty rule-set reference"),
            other => anyhow::bail!("unsupported rule kind: {other}"),
        }
        Ok(())
    }
}

pub fn normalize_domain(s: &str) -> anyhow::Result<String> {
    let d = s.trim_end_matches('.').to_ascii_lowercase();
    if d.is_empty()
        || d.len() > 253
        || d.chars().any(char::is_whitespace)
        || d.contains([',', '/', '\\', '+', '\0'])
        || d.split('.').any(str::is_empty)
    {
        anyhow::bail!("invalid domain rule: {s}");
    }
    Ok(d)
}
