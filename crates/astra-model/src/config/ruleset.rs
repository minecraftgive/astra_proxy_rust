use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
fn validate_url(value: &str) -> Result<()> {
    super::validate_remote_url(value).map(|_| ())
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MatchSpec {
    pub kind: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RuleSet {
    #[serde(skip_serializing_if = "is_enabled")]
    pub enabled: bool,
    pub behavior: String,
    pub format: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub interval: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub payload: Vec<String>,
}
impl Default for RuleSet {
    fn default() -> Self {
        Self {
            enabled: true,
            behavior: "classical".into(),
            format: "yaml".into(),
            path: None,
            url: None,
            interval: 86400,
            payload: Vec::new(),
        }
    }
}
fn is_enabled(value: &bool) -> bool {
    *value
}
pub fn compile_domain_regex(pattern: &str) -> Result<regex::Regex> {
    ensure!(pattern.len() <= 4096, "domain regex exceeds 4096 bytes");
    regex::RegexBuilder::new(pattern)
        .case_insensitive(true)
        .size_limit(2 * 1024 * 1024)
        .build()
        .context("invalid or unsupported domain regex")
}
impl RuleSet {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            matches!(self.behavior.as_str(), "classical" | "domain" | "ipcidr"),
            "unsupported rule-set behavior: {}",
            self.behavior
        );
        ensure!(
            matches!(self.format.as_str(), "yaml" | "text"),
            "unsupported rule-set format: {} (use yaml or text; MRS is not supported)",
            self.format
        );
        ensure!(
            self.interval <= 31_536_000,
            "rule-set interval exceeds one year"
        );
        ensure!(
            self.url.is_none() || self.path.is_some(),
            "HTTP rule-set requires a cache path"
        );
        if let Some(path) = &self.path {
            ensure!(
                !path.trim().is_empty() && !path.contains('\0'),
                "rule-set path must be a nonempty file path"
            );
        }
        ensure!(
            self.payload.is_empty() || (self.path.is_none() && self.url.is_none()),
            "inline payload cannot be combined with path or url"
        );
        if let Some(url) = &self.url {
            validate_url(url)?;
        }
        Ok(())
    }
    pub fn parse(&self, data: &[u8]) -> Result<Vec<MatchSpec>> {
        let text = std::str::from_utf8(data)
            .context("rule-set must be UTF-8")?
            .trim_start_matches('\u{feff}');
        let entries: Vec<String> =
            match self.format.as_str() {
                "yaml" => {
                    #[derive(Deserialize)]
                    #[serde(deny_unknown_fields)]
                    struct Payload {
                        payload: Vec<String>,
                    }
                    serde_yaml::from_str::<Payload>(text)
                        .context("expected YAML payload list")?
                        .payload
                }
                "text" => {
                    let mut output = Vec::new();
                    for (line, entry) in text.lines().enumerate() {
                        let entry = entry.trim();
                        if entry.is_empty() || entry.starts_with('#') || entry.starts_with("//") {
                            continue;
                        }
                        ensure!(output.len() < 500_000, "rule-set exceeds 500000 entries");
                        let index = output.len() + 1;
                        output.push(self.parse_entry(entry).with_context(|| {
                            format!("rule-set entry {index} (line {})", line + 1)
                        })?);
                    }
                    return Ok(output);
                }
                _ => bail!("unsupported rule-set format"),
            };
        self.parse_entries(&entries)
    }
    pub fn parse_entries(&self, entries: &[String]) -> Result<Vec<MatchSpec>> {
        ensure!(entries.len() <= 500_000, "rule-set exceeds 500000 entries");
        entries
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                self.parse_entry(entry.trim())
                    .with_context(|| format!("rule-set entry {}", i + 1))
            })
            .collect()
    }
    fn parse_entry(&self, entry: &str) -> Result<MatchSpec> {
        match self.behavior.as_str() {
            "classical" => {
                let matcher = parse_classical(entry)?;
                ensure!(
                    matcher.kind != "rule_set",
                    "RULE-SET cannot be nested in a classical provider"
                );
                Ok(matcher)
            }
            "ipcidr" => {
                entry
                    .parse::<ipnet::IpNet>()
                    .context("invalid rule-set CIDR")?;
                Ok(MatchSpec {
                    kind: "ip_cidr".into(),
                    value: entry.into(),
                })
            }
            "domain" => domain(entry),
            _ => bail!("unsupported rule-set behavior"),
        }
    }
}

fn domain(value: &str) -> Result<MatchSpec> {
    let value = value.trim_end_matches('.').to_ascii_lowercase();
    ensure!(
        !value.is_empty()
            && value.len() <= 253
            && !value.chars().any(char::is_whitespace)
            && !value.contains([',', '/', '\\']),
        "invalid rule-set domain"
    );
    let (kind, value) = if let Some(suffix) = value.strip_prefix("+.") {
        ("domain_suffix", suffix.to_owned())
    } else if let Some(suffix) = value.strip_prefix('.') {
        ("domain_subdomain", suffix.to_owned())
    } else if value.contains('*') {
        ("domain_pattern", value)
    } else {
        ("domain_exact", value)
    };
    ensure!(
        !value.is_empty()
            && value.split('.').all(|label| !label.is_empty()
                && !label.contains('+')
                && (!label.contains('*') || label == "*")),
        "invalid domain wildcard"
    );
    Ok(MatchSpec {
        kind: kind.into(),
        value,
    })
}

/// Classical providers omit the routing action. All native matchers operate on
/// the supplied target without DNS lookups, so trailing `no-resolve` is redundant
/// but accepted for every supported matcher (including domain rules).
pub fn parse_classical(input: &str) -> Result<MatchSpec> {
    let parts: Vec<_> = input.split(',').map(str::trim).collect();
    ensure!(
        parts.len() == 2 || (parts.len() == 3 && parts[2] == "no-resolve"),
        "unsupported classical rule parameters: {input}"
    );
    let value = parts[1];
    ensure!(!value.is_empty(), "empty rule value");
    let kind = match parts[0] {
        "DOMAIN" => "domain_exact",
        "DOMAIN-SUFFIX" => "domain_suffix",
        "DOMAIN-KEYWORD" => "domain_keyword",
        "DOMAIN-REGEX" => "domain_regex",
        "GEOSITE" => {
            geosite_selector(value)?;
            "geosite"
        }
        "IP-CIDR" | "IP-CIDR6" => {
            value.parse::<ipnet::IpNet>().context("invalid CIDR")?;
            "ip_cidr"
        }
        "DST-PORT" => {
            ensure!(
                value.parse::<u16>().is_ok_and(|p| p > 0),
                "expected single nonzero destination port"
            );
            "port"
        }
        "NETWORK" => {
            ensure!(
                matches!(value.to_ascii_lowercase().as_str(), "tcp" | "udp"),
                "invalid network"
            );
            "network"
        }
        "PROCESS-NAME" => "process_name",
        "PROCESS-PATH" => "process_path",
        "GEOIP" => {
            geoip_category(value)?;
            "geoip"
        }
        "RULE-SET" => "rule_set",
        other => bail!("unsupported rule type {other}"),
    };
    match kind {
        "domain_keyword" => ensure!(
            value.len() <= 253 && !value.chars().any(char::is_whitespace),
            "invalid domain keyword"
        ),
        "domain_regex" => {
            compile_domain_regex(value)?;
        }
        "domain_exact" | "domain_suffix" => ensure!(
            value.len() <= 253
                && !value.chars().any(char::is_whitespace)
                && !value.starts_with('.')
                && !value.trim_end_matches('.').is_empty()
                && !value.trim_end_matches('.').split('.').any(str::is_empty)
                && !value.contains(['*', '+', '/', '\\', '\0']),
            "invalid domain matcher"
        ),
        "process_name" => ensure!(
            value.len() <= 256 && !value.contains(['/', '\\', '\0']),
            "invalid process name"
        ),
        "process_path" => ensure!(
            value.len() <= 4096
                && !value.contains('\0')
                && (value.starts_with('/')
                    || value.starts_with("\\\\")
                    || (value.as_bytes().get(1) == Some(&b':')
                        && matches!(value.as_bytes().get(2), Some(b'/' | b'\\')))),
            "process_path requires an absolute executable path"
        ),
        _ => {}
    }
    Ok(MatchSpec {
        kind: kind.into(),
        value: match kind {
            "network" => value.to_ascii_lowercase(),
            "geoip" => value.to_ascii_uppercase(),
            _ => value.into(),
        },
    })
}

pub fn geosite_selector(input: &str) -> Result<(String, Vec<(String, bool)>)> {
    let mut parts = input.split('@');
    let group = parts.next().unwrap_or_default().to_ascii_lowercase();
    let valid = |value: &str| {
        !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.!".contains(&b))
    };
    ensure!(valid(&group), "invalid GEOSITE group: {input}");
    let filters = parts
        .map(|part| {
            let (part, negated) = part.strip_prefix('!').map_or((part, false), |s| (s, true));
            ensure!(
                valid(part) && !part.contains('!'),
                "invalid GEOSITE attribute: {input}"
            );
            Ok((part.to_ascii_lowercase(), negated))
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(filters.len() <= 16, "too many GEOSITE attribute filters");
    Ok((group, filters))
}

pub fn geoip_category(input: &str) -> Result<String> {
    ensure!(
        !input.is_empty()
            && input.len() <= 128
            && input
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)),
        "invalid GEOIP category: {input}"
    );
    Ok(input.to_ascii_uppercase())
}
