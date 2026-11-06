pub use super::protocol::OutboundConfig;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
};
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SystemProxy {
    /// macOS network service names; empty discovers all enabled services.
    #[serde(default)]
    pub services: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tun {
    pub name: Option<String>,
    #[serde(default = "address")]
    pub address: Ipv4Addr,
    pub ipv6_address: Option<Ipv6Addr>,
    #[serde(default = "ipv6_prefix")]
    pub ipv6_prefix: u8,
    #[serde(default = "prefix")]
    pub prefix: u8,
    #[serde(default = "mtu")]
    pub mtu: u16,
    /// Explicit CIDRs; empty leaves routing to the operator.
    #[serde(default)]
    pub routes: Vec<String>,
    /// Hosts/subnets routed through the original gateway (including proxy servers).
    #[serde(default)]
    pub bypass: Vec<String>,
    pub gateway: Option<IpAddr>,
    pub gateway6: Option<Ipv6Addr>,
    pub interface: Option<String>,
    /// None infers auto when no interface is supplied; an explicit interface
    /// remains a manual choice unless auto_interface=true is requested.
    pub auto_interface: Option<bool>,
    #[serde(default = "auto_gateway")]
    pub auto_gateway: bool,
    #[serde(default = "auto_gateway")]
    pub sniff: bool,
}
impl Default for Tun {
    fn default() -> Self {
        Self {
            name: None,
            address: address(),
            ipv6_address: None,
            ipv6_prefix: ipv6_prefix(),
            prefix: prefix(),
            mtu: mtu(),
            routes: vec!["0.0.0.0/1".into(), "128.0.0.0/1".into()],
            bypass: vec![],
            gateway: None,
            gateway6: None,
            interface: None,
            auto_interface: Some(true),
            auto_gateway: true,
            sniff: true,
        }
    }
}
fn auto_gateway() -> bool {
    true
}
fn address() -> Ipv4Addr {
    Ipv4Addr::new(198, 18, 0, 1)
}
fn prefix() -> u8 {
    30
}
fn ipv6_prefix() -> u8 {
    126
}
fn mtu() -> u16 {
    1500
}

/// Opt-in features that are not yet part of the supported surface.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Experimental {
    /// Journal system proxy, route and adapter changes and run a guardian that
    /// restores them if the core exits abruptly. Off: nothing is recorded and
    /// a crash leaves those settings as applied. Read at core startup.
    #[serde(default)]
    pub network_recovery: bool,
}
impl Experimental {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Transparent {
    pub listen: SocketAddr,
}

impl Tun {
    pub fn detects_interface(&self) -> bool {
        self.auto_interface.unwrap_or(self.interface.is_none())
    }
    pub fn gateway_for(&self, cidr: &str) -> Option<IpAddr> {
        if cidr.contains(':') {
            self.gateway6
                .map(IpAddr::V6)
                .or(self.gateway.filter(IpAddr::is_ipv6))
        } else {
            self.gateway.filter(IpAddr::is_ipv4)
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyConfig {
    #[serde(default)]
    pub auto_start: bool,
    #[serde(default)]
    pub rule_socket: String,
    #[serde(default)]
    pub standalone: bool,
    pub standalone_outbound: Option<String>,
    #[serde(default = "default_shutdown_timeout")]
    pub shutdown_timeout_secs: u64,
    pub socks5_listen: Option<String>,
    pub http_listen: Option<String>,
    pub mixed_listen: Option<String>,
    #[serde(default)]
    pub authentication: Vec<String>,
    #[serde(rename = "allow_lan")]
    pub _legacy_allow_lan: Option<bool>,
    #[serde(default)]
    pub lan_allowed_ips: Vec<ipnet::IpNet>,
    #[serde(default)]
    pub lan_disallowed_ips: Vec<ipnet::IpNet>,
    #[serde(default)]
    pub skip_auth_prefixes: Vec<ipnet::IpNet>,
    #[serde(default)]
    pub watch_config: bool,
    pub subscription: Option<super::subscription::Subscription>,
    #[serde(default)]
    pub subscriptions: BTreeMap<String, super::subscription::Subscription>,
    pub system_proxy: Option<SystemProxy>,
    pub tun: Option<Tun>,
    pub dns: Option<super::dns::Config>,
    pub tproxy: Option<Transparent>,
    pub redirect: Option<Transparent>,
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
    pub proxy_groups: BTreeMap<String, crate::groups::Group>,
    #[serde(default, skip_serializing_if = "Experimental::is_default")]
    pub experimental: Experimental,
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
    // Client sessions are mostly long-lived keep-alive tunnels that would hold
    // the full window; a short grace period keeps quitting responsive.
    2
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
    pub fn dns_only(&self) -> bool {
        self.socks5_listen.is_none()
            && self.http_listen.is_none()
            && self.mixed_listen.is_none()
            && self.tun.is_none()
            && self.tproxy.is_none()
            && self.redirect.is_none()
            && self
                .dns
                .as_ref()
                .is_some_and(|dns| dns.enable && !dns.respect_rules && !dns.listen.is_empty())
    }
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
        // Generated subscription groups all feed the default group. Their arrival
        // must not turn a previously valid standalone subscription into ambiguity.
        if !self.subscription_sources().is_empty() && self.proxy_groups.contains_key("PROXY") {
            return Ok(Some("PROXY".into()));
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

    pub fn subscription_sources(&self) -> BTreeMap<String, super::subscription::Subscription> {
        let mut sources = self.subscriptions.clone();
        if let Some(legacy) = &self.subscription {
            sources.insert("__legacy__".into(), legacy.clone());
        }
        sources
    }
}

impl ProxyConfig {
    /// Pure structure/reference validation. Does not read files, bind sockets or inspect the host.
    pub fn validate_structure(
        &self,
    ) -> Result<std::collections::HashMap<String, super::protocol::OutboundConfig>> {
        use super::protocol::OutboundConfig;
        use std::collections::HashMap;
        if let Some(dns) = &self.dns {
            dns.validate()?;
        }
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
        anyhow::ensure!(
            self.authentication.len() <= 64,
            "at most 64 inbound users are allowed"
        );
        anyhow::ensure!(
            self.lan_allowed_ips.len() <= 256
                && self.lan_disallowed_ips.len() <= 256
                && self.skip_auth_prefixes.len() <= 256,
            "inbound ACL limit is 256 prefixes per list"
        );
        let mut users = std::collections::HashSet::new();
        for credential in &self.authentication {
            let (user, password) = credential
                .split_once(':')
                .context("authentication entries require username:password")?;
            anyhow::ensure!(
                !user.is_empty()
                    && !password.is_empty()
                    && user.len() <= 255
                    && password.len() <= 255
                    && !credential.contains(['\r', '\n'])
                    && users.insert(user),
                "invalid or duplicate inbound credentials"
            );
        }
        self.validate_modes_structure()?;
        self.standalone_tag()?;
        if !self.standalone && !self.dns_only() && self.rule_socket.is_empty() {
            bail!("rule_socket is required unless standalone = true");
        }
        if self.http_listen.is_none()
            && self.mixed_listen.is_none()
            && self.socks5_listen.is_none()
            && self.tun.is_none()
            && self.tproxy.is_none()
            && self.redirect.is_none()
            && !self
                .dns
                .as_ref()
                .is_some_and(|dns| dns.enable && !dns.listen.is_empty())
        {
            bail!("at least one inbound listener must be configured");
        }
        if self.ipc_connections == 0
            || self.ipc_connections > 128
            || self.max_connections == 0
            || self.max_connections > u32::MAX as usize
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
                OutboundConfig::VlessTls { uuid, .. } => {
                    uuid::Uuid::parse_str(uuid)
                        .with_context(|| format!("invalid UUID for {}", ob.tag()))?;
                }
                _ => {}
            }
            if ob.tag().is_empty() || map.insert(ob.tag().to_owned(), ob.clone()).is_some() {
                bail!("empty or duplicate outbound tag: {}", ob.tag());
            }
        }
        crate::groups::validate(&self.proxy_groups, &map.keys().cloned().collect())
            .map_err(anyhow::Error::msg)?;
        if !self.proxy_groups.is_empty() && map.keys().any(|tag| crate::groups::builtin(tag)) {
            bail!("outbound tags DIRECT and REJECT are reserved when using proxy groups");
        }
        Ok(map)
    }
}

impl ProxyConfig {
    /// Listener and TUN shape checks independent of target operating system.
    pub fn validate_modes_structure(&self) -> Result<()> {
        use anyhow::ensure;
        let mut listeners = std::collections::HashSet::new();
        for address in self
            .socks5_listen
            .iter()
            .chain(&self.http_listen)
            .chain(&self.mixed_listen)
        {
            ensure!(
                listeners.insert(address.clone()),
                "inbound listeners must use distinct addresses"
            );
            if let Ok(address) = address.parse::<SocketAddr>() {
                ensure!(address.port() != 0, "listener port must be nonzero");
            }
        }
        for settings in self.tproxy.iter().chain(&self.redirect) {
            ensure!(
                settings.listen.port() != 0,
                "transparent listener port must be nonzero"
            );
            ensure!(
                listeners.insert(settings.listen.to_string()),
                "inbound listeners must use distinct addresses"
            );
        }
        if let Some(tun) = &self.tun {
            ensure!(
                tun.auto_interface != Some(false) || tun.interface.is_some(),
                "manual TUN egress requires tun.interface"
            );
            ensure!(
                tun.prefix <= 32 && tun.ipv6_prefix <= 128 && (1280..=9000).contains(&tun.mtu),
                "invalid TUN prefix/MTU (1280..9000)"
            );
            for cidr in tun.routes.iter().chain(&tun.bypass) {
                let (ip, prefix) = cidr
                    .split_once('/')
                    .ok_or_else(|| anyhow::anyhow!("route requires CIDR: {cidr}"))?;
                let ip: IpAddr = ip.parse()?;
                let prefix: u8 = prefix.parse()?;
                ensure!(
                    prefix <= if ip.is_ipv4() { 32 } else { 128 },
                    "invalid route prefix"
                );
            }
            ensure!(
                tun.routes.is_empty() || tun.interface.is_some(),
                "managed TUN routes require original egress interface (for direct connections)"
            );
            ensure!(
                tun.bypass.is_empty() || tun.gateway.is_some() || tun.interface.is_some(),
                "TUN bypass requires original gateway or interface"
            );
            ensure!(
                tun.gateway6.is_none() || tun.interface.is_some(),
                "tun.gateway6 requires the original egress interface"
            );
        }
        Ok(())
    }
}
