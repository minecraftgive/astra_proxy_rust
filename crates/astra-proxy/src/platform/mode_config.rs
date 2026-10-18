use anyhow::{bail, ensure, Result};
use serde::Deserialize;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SystemProxy {
    /// macOS network service names; empty discovers all enabled services.
    #[serde(default)]
    pub services: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Transparent {
    pub listen: SocketAddr,
}

pub fn validate(config: &crate::config::ProxyConfig) -> Result<()> {
    let mut listeners = std::collections::HashSet::new();
    for address in config.socks5_listen.iter().chain(&config.http_listen) {
        ensure!(
            listeners.insert(address.clone()),
            "inbound listeners must use distinct addresses"
        );
        if let Ok(address) = address.parse::<SocketAddr>() {
            ensure!(address.port() != 0, "listener port must be nonzero");
        }
    }
    for settings in config.tproxy.iter().chain(&config.redirect) {
        ensure!(
            settings.listen.port() != 0,
            "transparent listener port must be nonzero"
        );
        ensure!(
            listeners.insert(settings.listen.to_string()),
            "inbound listeners must use distinct addresses"
        );
    }
    if let Some(tun) = &config.tun {
        ensure!(
            cfg!(any(target_os = "linux", target_os = "macos", windows)),
            "TUN is supported on Linux/macOS/Windows"
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
        #[cfg(windows)]
        {
            ensure!(
                tun.bypass.is_empty() || tun.interface.is_some(),
                "Windows TUN bypass requires the original interface alias or index"
            );
            ensure!(
                !tun.routes.iter().any(|v| v.contains(':')) || tun.ipv6_address.is_some(),
                "Windows IPv6 TUN routes require tun.ipv6_address"
            );
            ensure!(tun.routes.iter().all(|v| v.parse::<ipnet::IpNet>().is_ok_and(|net| net.prefix_len() != 0)), "Use split default routes (0.0.0.0/1, 128.0.0.0/1 or ::/1, 8000::/1) on Windows to retain the physical default route");
            for cidr in &tun.bypass {
                ensure!(
                    !cidr.contains(':')
                        || !tun.gateway.is_some_and(|v| v.is_ipv4())
                        || tun.gateway6.is_some(),
                    "IPv6 TUN bypass with an IPv4 gateway requires tun.gateway6"
                );
                ensure!(
                    cidr.contains(':') || !tun.gateway.is_some_and(|v| v.is_ipv6()),
                    "IPv4 TUN bypass requires an IPv4 tun.gateway"
                );
            }
        }
    }
    if config.tproxy.is_some() || config.redirect.is_some() {
        ensure!(
            cfg!(target_os = "linux"),
            "TPROXY/REDIRECT require Linux; use TUN on macOS/Windows"
        );
    }
    if config.system_proxy.is_some() {
        #[cfg(windows)]
        let listener = config.http_listen.as_ref();
        #[cfg(not(windows))]
        let listener = config.socks5_listen.as_ref();
        ensure!(
            listener.is_some(),
            "system_proxy requires a loopback HTTP listener on Windows, or a SOCKS5 listener on macOS/Linux"
        );
        let addr: SocketAddr = listener.unwrap().parse()?;
        ensure!(
            addr.ip().is_loopback() && addr.port() != 0,
            "system proxy listener must be a loopback IP with a nonzero port"
        );
        if !cfg!(any(target_os = "macos", target_os = "linux", windows)) {
            bail!("system_proxy unsupported on this OS");
        }
        if !cfg!(target_os = "macos") {
            ensure!(
                config.system_proxy.as_ref().unwrap().services.is_empty(),
                "system_proxy.services is macOS-only"
            );
        }
    }
    Ok(())
}

impl Tun {
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
