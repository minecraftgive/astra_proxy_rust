use anyhow::{bail, ensure, Result};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

pub use astra_model::config::proxy::{SystemProxy, Transparent, Tun};

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
    let (http, socks) = system_proxy_addresses(config)?;
    http.or(socks)
        .ok_or_else(|| anyhow::anyhow!("system_proxy requires a mixed, HTTP or SOCKS5 listener"))
}

/// Mixed is preferred for every protocol; dedicated listeners are fallbacks.
pub(crate) fn system_proxy_addresses(
    config: &crate::config::ProxyConfig,
) -> Result<(Option<SocketAddr>, Option<SocketAddr>)> {
    let http = config.mixed_listen.as_ref().or(config.http_listen.as_ref());
    let socks = config
        .mixed_listen
        .as_ref()
        .or(config.socks5_listen.as_ref());
    Ok((
        http.map(|v| local_proxy_address(v)).transpose()?,
        socks.map(|v| local_proxy_address(v)).transpose()?,
    ))
}

/// Tables contain fixed network parameters, never on/off state. Defaults are
/// created in memory when the user enables a mode through runtime control.
pub(crate) fn runtime_modes(
    config: &mut crate::config::ProxyConfig,
    state: &astra_model::runtime::Overrides,
) {
    if state.system_proxy == Some(true) {
        config.system_proxy.get_or_insert_with(SystemProxy::default);
    } else {
        config.system_proxy = None;
    }
    if state.tun == Some(true) {
        config.tun.get_or_insert_with(Tun::default);
    } else {
        config.tun = None;
    }
}

pub fn validate(config: &crate::config::ProxyConfig) -> Result<()> {
    config.validate_modes_structure()?;
    if let Some(tun) = &config.tun {
        ensure!(
            cfg!(any(target_os = "linux", target_os = "macos", windows)),
            "TUN is supported on Linux/macOS/Windows"
        );
        #[cfg(not(windows))]
        let _ = tun;
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
    if let Some(system_proxy) = &config.system_proxy {
        system_proxy_address(config)?;
        if !cfg!(any(target_os = "macos", target_os = "linux", windows)) {
            bail!("system_proxy unsupported on this OS");
        }
        if !cfg!(target_os = "macos") {
            ensure!(
                system_proxy.services.is_empty(),
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
        let expected_port = 8080;
        let target = system_proxy_address(&config).unwrap();
        assert_eq!(target.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(target.port(), expected_port);
        assert_eq!(config.http_listen.as_deref(), Some("0.0.0.0:8080"));
    }

    #[test]
    fn mixed_port_is_preferred_for_all_system_proxy_protocols() {
        let config = toml::from_str("mixed_listen='0.0.0.0:7890'\nhttp_listen='127.0.0.1:8080'\nsocks5_listen='127.0.0.1:1080'\n").unwrap();
        let (http, socks) = system_proxy_addresses(&config).unwrap();
        assert_eq!(http.unwrap().to_string(), "127.0.0.1:7890");
        assert_eq!(http, socks);
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

#[cfg(test)]
mod runtime_mode_tests {
    use super::*;
    #[test]
    fn fixed_tables_never_enable_runtime_modes_and_defaults_need_no_tables() {
        let mut config:crate::config::ProxyConfig=toml::from_str("socks5_listen='127.0.0.1:1080'\nhttp_listen='127.0.0.1:8080'\n[system_proxy]\n[tun]\nname='fixed-network-parameters'\n").unwrap();
        let state = astra_model::runtime::Overrides::default();
        runtime_modes(&mut config, &state);
        assert!(config.system_proxy.is_none());
        assert!(config.tun.is_none());
        let enabled = astra_model::runtime::Overrides {
            system_proxy: Some(true),
            tun: Some(true),
            ..Default::default()
        };
        runtime_modes(&mut config, &enabled);
        assert!(config.system_proxy.is_some());
        assert_eq!(system_proxy_address(&config).unwrap().port(), 8080);
        let tun = config.tun.as_ref().unwrap();
        assert_eq!(tun.routes, ["0.0.0.0/1", "128.0.0.0/1"]);
        assert!(tun.detects_interface());
        runtime_modes(&mut config, &state);
        assert!(config.system_proxy.is_none());
        assert!(config.tun.is_none());
    }
    #[test]
    fn explicit_fixed_tun_parameters_are_kept_only_while_enabled() {
        let mut config: crate::config::ProxyConfig =
            toml::from_str("[tun]\nname='my-device'\nmtu=1400\n").unwrap();
        runtime_modes(
            &mut config,
            &astra_model::runtime::Overrides {
                tun: Some(true),
                ..Default::default()
            },
        );
        assert_eq!(
            config.tun.as_ref().unwrap().name.as_deref(),
            Some("my-device")
        );
        assert_eq!(config.tun.as_ref().unwrap().mtu, 1400);
        let without_listener: crate::config::ProxyConfig =
            toml::from_str("standalone=true\n").unwrap();
        assert!(system_proxy_address(&without_listener).is_err());
    }
}
