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

/// An unspecified bind address serves loopback too, but must never be written
/// into OS proxy settings as the destination address.
fn local_proxy_address(listener: &str) -> Result<SocketAddr> {
    let mut address: SocketAddr = listener.parse()?;
    match address.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => address.set_ip(Ipv4Addr::LOCALHOST.into()),
        IpAddr::V6(ip) if ip.is_unspecified() => address.set_ip(Ipv6Addr::LOCALHOST.into()),
        _ => {}
    }
    ensure!(address.ip().is_loopback() && address.port() != 0,
        "system proxy listener {listener} must bind a loopback IP or all interfaces and use a nonzero port");
    Ok(address)
}

pub(crate) fn system_proxy_address(config: &crate::config::ProxyConfig) -> Result<SocketAddr> {
    #[cfg(windows)]
    let listener = &config.http_listen;
    #[cfg(not(windows))]
    let listener = &config.socks5_listen;
    let listener = listener.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
        "system_proxy requires an HTTP listener on Windows, or a SOCKS5 listener on macOS/Linux"
    )
    })?;
    local_proxy_address(listener)
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
    /// None infers auto when no interface is supplied; an explicit interface
    /// remains a manual choice unless auto_interface=true is requested.
    pub auto_interface: Option<bool>,
    #[serde(default = "auto_gateway")]
    pub auto_gateway: bool,
    #[serde(default = "auto_gateway")]
    pub sniff: bool,
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
            tun.auto_interface != Some(false) || tun.interface.is_some(),
            "manual TUN egress requires tun.interface"
        );
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
        system_proxy_address(config)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_listeners_use_matching_loopback_family_for_os_proxy() {
        for (bind, expected) in [
            ("0.0.0.0:8080", "127.0.0.1:8080"),
            ("[::]:8080", "[::1]:8080"),
            ("127.0.0.1:8080", "127.0.0.1:8080"),
            ("[::1]:8080", "[::1]:8080"),
        ] {
            assert_eq!(local_proxy_address(bind).unwrap().to_string(), expected);
        }
        for invalid in ["0.0.0.0:0", "[::]:0", "127.0.0.1:0", "192.0.2.1:8080"] {
            assert!(local_proxy_address(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn validation_and_os_proxy_share_the_same_listener_resolution() {
        let config: crate::config::ProxyConfig = toml::from_str(
            "socks5_listen='0.0.0.0:1080'\nhttp_listen='0.0.0.0:8080'\n[system_proxy]\n",
        )
        .unwrap();
        validate(&config).unwrap();
        let expected_port = if cfg!(windows) { 8080 } else { 1080 };
        let target = system_proxy_address(&config).unwrap();
        assert_eq!(target.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(target.port(), expected_port);
        assert_eq!(config.http_listen.as_deref(), Some("0.0.0.0:8080"));
    }

    #[tokio::test]
    async fn mapped_target_reaches_the_wildcard_listener() {
        let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
        let target = local_proxy_address(&listener.local_addr().unwrap().to_string()).unwrap();
        let _client = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            tokio::net::TcpStream::connect(target),
        )
        .await
        .unwrap()
        .unwrap();
        let (_, peer) = listener.accept().await.unwrap();
        assert!(peer.ip().is_loopback());
    }
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
