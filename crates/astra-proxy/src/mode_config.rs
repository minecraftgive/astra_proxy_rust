use anyhow::{bail, ensure, Result};
use serde::Deserialize;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

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
    pub interface: Option<String>,
}
fn address() -> Ipv4Addr {
    Ipv4Addr::new(198, 18, 0, 1)
}
fn prefix() -> u8 {
    30
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
            cfg!(any(target_os = "linux", target_os = "macos")),
            "TUN is supported on Linux/macOS"
        );
        ensure!(
            tun.prefix <= 32 && (1280..=9000).contains(&tun.mtu),
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
    }
    if config.tproxy.is_some() || config.redirect.is_some() {
        ensure!(
            cfg!(target_os = "linux"),
            "TPROXY/REDIRECT require Linux; use TUN on macOS"
        );
    }
    if config.system_proxy.is_some() {
        ensure!(
            config.socks5_listen.is_some(),
            "system_proxy requires a SOCKS5 listener"
        );
        let addr: SocketAddr = config.socks5_listen.as_ref().unwrap().parse()?;
        ensure!(
            addr.ip().is_loopback() && addr.port() != 0,
            "system proxy listener must be a loopback IP with a nonzero port"
        );
        // HTTP CONNECT does not support absolute-form HTTP requests: use SOCKS for
        // system integration rather than incorrectly advertising an HTTP proxy.
        if !cfg!(any(target_os = "macos", target_os = "linux")) {
            bail!("system_proxy unsupported on this OS");
        }
    }
    Ok(())
}
