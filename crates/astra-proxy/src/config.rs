use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};

pub use astra_protocols::config::OutboundConfig;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyConfig {
    #[serde(default)]
    pub rule_socket: String,
    #[serde(default)]
    pub standalone: bool,
    pub standalone_outbound: Option<String>,
    #[serde(default = "default_shutdown_timeout")]
    pub shutdown_timeout_secs: u64,
    pub socks5_listen: Option<String>,
    pub http_listen: Option<String>,
    #[serde(default)]
    pub watch_config: bool,
    pub subscription: Option<astra_convert::subscription::Subscription>,
    #[serde(default)]
    pub subscriptions: BTreeMap<String, astra_convert::subscription::Subscription>,
    pub system_proxy: Option<crate::mode_config::SystemProxy>,
    pub tun: Option<crate::mode_config::Tun>,
    pub tproxy: Option<crate::mode_config::Transparent>,
    pub redirect: Option<crate::mode_config::Transparent>,
    #[serde(default = "default_workers")]
    pub ipc_connections: usize,
    #[serde(default = "default_timeout")]
    pub decision_timeout_ms: u64,
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout_secs: u64,
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,
    #[serde(default = "default_udp_flows")]
    pub max_udp_flows: usize,
    #[serde(default = "default_udp_targets")]
    pub udp_targets_per_association: usize,
    #[serde(default = "default_udp_idle")]
    pub udp_idle_timeout_secs: u64,
    #[serde(default = "default_cache_ttl")]
    pub decision_cache_ttl_secs: u64,
    #[serde(default)]
    pub outbounds: Vec<OutboundConfig>,
    #[serde(default)]
    pub proxy_groups: BTreeMap<String, astra_ipc::groups::Group>,
}
fn default_udp_flows() -> usize {
    512
}
fn default_udp_targets() -> usize {
    32
}
fn default_udp_idle() -> u64 {
    120
}
fn default_shutdown_timeout() -> u64 {
    10
}
fn default_workers() -> usize {
    8
}
fn default_timeout() -> u64 {
    500
}
fn default_connect_timeout() -> u64 {
    8
}
fn default_max_connections() -> usize {
    4096
}
fn default_cache_ttl() -> u64 {
    5
}

impl ProxyConfig {
    /// Standalone never falls back to direct or chooses arbitrarily among nodes.
    pub fn standalone_tag(&self) -> Result<Option<String>> {
        if !self.standalone {
            if self.standalone_outbound.is_some() {
                bail!("standalone_outbound requires standalone = true");
            }
            return Ok(None);
        }
        if let Some(tag) = &self.standalone_outbound {
            if !self.outbounds.iter().any(|outbound| outbound.tag() == tag)
                && !self.proxy_groups.contains_key(tag)
            {
                bail!("standalone_outbound references undefined outbound: {tag}");
            }
            return Ok(Some(tag.clone()));
        }
        if self.outbounds.len() != 1 && !(self.outbounds.is_empty() && self.proxy_groups.len() == 1)
        {
            bail!(
                "standalone requires exactly one outbound or an explicit standalone_outbound tag"
            );
        }
        Ok(Some(if let Some(node) = self.outbounds.first() {
            node.tag().to_owned()
        } else {
            self.proxy_groups.keys().next().unwrap().clone()
        }))
    }

    pub fn subscription_sources(
        &self,
    ) -> BTreeMap<String, astra_convert::subscription::Subscription> {
        let mut sources = self.subscriptions.clone();
        if let Some(legacy) = &self.subscription {
            sources.insert("__legacy__".into(), legacy.clone());
        }
        sources
    }
    pub fn validate(&self) -> Result<HashMap<String, OutboundConfig>> {
        anyhow::ensure!(
            self.subscriptions.len() <= 32,
            "at most 32 subscriptions allowed"
        );
        for (name, subscription) in self.subscription_sources() {
            anyhow::ensure!(
                !name.is_empty()
                    && !name.chars().any(char::is_control)
                    && name.len() <= 64
                    && (name != "__legacy__" || self.subscription.is_some()),
                "invalid subscription name"
            );
            subscription.validate()?;
        }
        anyhow::ensure!(
            !self.subscriptions.contains_key("__legacy__"),
            "reserved subscription name"
        );
        self.standalone_tag()?;
        crate::mode_config::validate(self)?;
        if !self.standalone && self.rule_socket.is_empty() {
            bail!("rule_socket is required unless standalone = true");
        }
        if self.http_listen.is_none()
            && self.socks5_listen.is_none()
            && self.tun.is_none()
            && self.tproxy.is_none()
            && self.redirect.is_none()
        {
            bail!("at least one inbound listener must be configured");
        }
        if self.ipc_connections == 0
            || self.ipc_connections > 128
            || self.max_connections == 0
            || self.max_connections > u32::MAX as usize
            || self.max_connections > tokio::sync::Semaphore::MAX_PERMITS
        {
            bail!("invalid connection limits");
        }
        if self.decision_timeout_ms == 0 || self.connect_timeout_secs == 0 {
            bail!("timeouts must be positive");
        }
        if self.max_udp_flows == 0
            || self.max_udp_flows > 65536
            || self.udp_targets_per_association == 0
            || self.udp_targets_per_association > self.max_udp_flows
            || self.udp_idle_timeout_secs == 0
        {
            bail!("invalid UDP limits");
        }
        let mut map = HashMap::new();
        for ob in &self.outbounds {
            match ob {
                OutboundConfig::Socks5 {
                    username, password, ..
                }
                | OutboundConfig::Http {
                    username, password, ..
                } => {
                    if username.is_some() != password.is_some() {
                        bail!("outbound {} requires both username and password", ob.tag());
                    }
                    if let (Some(user), Some(pass)) = (username, password) {
                        if user.is_empty()
                            || user.len() > 255
                            || pass.is_empty()
                            || pass.len() > 255
                        {
                            bail!("invalid credential length for outbound {}", ob.tag());
                        }
                    }
                }
                OutboundConfig::VlessTls { uuid, sni, .. } => {
                    uuid::Uuid::parse_str(uuid)
                        .with_context(|| format!("invalid UUID for {}", ob.tag()))?;
                    tokio_rustls::rustls::pki_types::ServerName::try_from(sni.clone())
                        .with_context(|| format!("invalid SNI for {}", ob.tag()))?;
                }
                OutboundConfig::TrojanTls { sni, .. } => {
                    tokio_rustls::rustls::pki_types::ServerName::try_from(sni.clone())
                        .with_context(|| format!("invalid SNI for {}", ob.tag()))?;
                }
                _ => {}
            }
            if ob.tag().is_empty() || map.insert(ob.tag().to_owned(), ob.clone()).is_some() {
                bail!("empty or duplicate outbound tag: {}", ob.tag());
            }
        }
        astra_ipc::groups::validate(&self.proxy_groups, &map.keys().cloned().collect())
            .map_err(anyhow::Error::msg)?;
        for (name, group) in &self.proxy_groups {
            if group.mode == astra_ipc::groups::Mode::Auto {
                crate::proxy::groups::test_uri(&group.url)
                    .with_context(|| format!("proxy group {name}"))?;
            }
        }
        if !self.proxy_groups.is_empty() && map.keys().any(|tag| astra_ipc::groups::builtin(tag)) {
            bail!("outbound tags DIRECT and REJECT are reserved when using proxy groups");
        }
        Ok(map)
    }
}

#[cfg(test)]
mod standalone_tests {
    use super::*;
    fn config(extra: &str, nodes: usize) -> ProxyConfig {
        let mut text = format!("socks5_listen='127.0.0.1:1080'\n{extra}\n");
        for index in 0..nodes {
            text.push_str(&format!(
                "[[outbounds]]\nkind='socks5'\ntag='node{index}'\nserver='127.0.0.1:1081'\n"
            ));
        }
        toml::from_str(&text).unwrap()
    }
    #[test]
    fn standalone_selection_is_explicit_and_never_direct() {
        let single = config("standalone=true", 1);
        single.validate().unwrap();
        assert_eq!(single.standalone_tag().unwrap().as_deref(), Some("node0"));
        let multiple = config("standalone=true\nstandalone_outbound='node1'", 2);
        multiple.validate().unwrap();
        assert_eq!(multiple.standalone_tag().unwrap().as_deref(), Some("node1"));
        for invalid in [
            config("standalone=true", 0),
            config("standalone=true", 2),
            config("standalone=true\nstandalone_outbound='missing'", 1),
            config(
                "rule_socket='/tmp/rules.sock'\nstandalone_outbound='node0'",
                1,
            ),
            config("", 1),
        ] {
            assert!(invalid.validate().is_err());
        }
        let normal = config("rule_socket='/tmp/rules.sock'", 0);
        normal.validate().unwrap();
        assert_eq!(normal.standalone_tag().unwrap(), None);
    }
}
