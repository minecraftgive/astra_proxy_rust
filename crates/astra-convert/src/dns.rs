//! Explicit Mihomo DNS translation; unsupported semantics produce diagnostics.
use anyhow::{ensure, Context, Result};
use serde_yaml::Value as Y;

fn strings(value: &Y) -> Result<Vec<String>> {
    if let Some(value) = value.as_str() {
        return Ok(vec![value.to_owned()]);
    }
    value
        .as_sequence()
        .context("DNS nameservers/filter must be a string or list")?
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .context("DNS list contains a non-string value")
        })
        .collect()
}
pub(crate) fn convert(value: &Y, warnings: &mut Vec<String>) -> Result<toml::Value> {
    let map = value.as_mapping().context("dns must be a mapping")?;
    let mut config = astra_dns::Config::default();
    for (key, value) in map {
        let key = key.as_str().context("DNS key must be a string")?;
        match key {
            "enable" => config.enable = value.as_bool().context("dns.enable must be boolean")?,
            "listen" => {
                config.listen = vec![value
                    .as_str()
                    .context("dns.listen must be an IP:port")?
                    .parse()?]
            }
            "ipv6" => config.ipv6 = value.as_bool().context("dns.ipv6 must be boolean")?,
            "respect-rules" => {
                config.respect_rules = value
                    .as_bool()
                    .context("dns.respect-rules must be boolean")?
            }
            "nameserver" => config.nameserver = strings(value)?,
            "fallback" => config.fallback = strings(value)?,
            "default-nameserver" => {
                config.bootstrap = strings(value)?
                    .iter()
                    .map(|raw| {
                        let upstream = astra_dns::Upstream::parse(raw)?;
                        ensure!(
                            upstream.scheme == "udp" && upstream.port == 53,
                            "DNS default-nameserver bootstrap supports literal UDP IP:53 only"
                        );
                        Ok(upstream.host.parse()?)
                    })
                    .collect::<Result<_>>()?;
            }
            "enhanced-mode" => {
                let mode = value.as_str().context("invalid enhanced-mode")?;
                ensure!(
                    matches!(mode, "fake-ip" | "redir-host" | "normal"),
                    "unsupported DNS enhanced-mode"
                );
                config.fake_ip = mode == "fake-ip";
            }
            "fake-ip-range" => {
                config.fake_ip_range = value.as_str().context("invalid fake-ip-range")?.into()
            }
            "fake-ip-range6" => {
                config.fake_ip_range6 =
                    Some(value.as_str().context("invalid fake-ip-range6")?.into())
            }
            "fake-ip-ttl" => {
                config.fake_ip_ttl = u32::try_from(value.as_u64().context("invalid fake-ip-ttl")?)?
            }
            "fake-ip-filter" => config.fake_ip_filter = strings(value)?,
            "fake-ip-filter-mode" => {
                config.fake_ip_filter_mode = value
                    .as_str()
                    .context("invalid fake-ip-filter-mode")?
                    .into()
            }
            "cache-size" => {
                config.cache_capacity =
                    usize::try_from(value.as_u64().context("invalid cache-size")?)?
            }
            "nameserver-policy" => {
                for (pattern, servers) in value.as_mapping().context("invalid nameserver-policy")? {
                    let pattern = pattern.as_str().context("invalid DNS policy domain")?;
                    ensure!(
                        !pattern.starts_with("geosite:") && !pattern.starts_with("rule-set:"),
                        "DNS geosite/rule-set policies are not supported; use domain patterns"
                    );
                    config
                        .nameserver_policy
                        .insert(pattern.into(), strings(servers)?);
                }
            }
            "fallback-filter" => warnings.push(
                "DNS fallback-filter 尚不支持；fallback 仅用于上游失败，未采用 GeoIP/污染过滤条件"
                    .into(),
            ),
            _ => warnings.push(format!("忽略不支持的 DNS 字段 {key}")),
        }
    }
    config
        .validate()
        .context("invalid converted DNS configuration")?;
    Ok(toml::Value::try_from(&config)?)
}
