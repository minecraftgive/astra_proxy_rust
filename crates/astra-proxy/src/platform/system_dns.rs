//! System DNS takeover while TUN is active (DNS service enabled, hijack on).
//!
//! A) The OS resolver is pointed at an address inside the TUN subnet, so every
//!    system query enters the TUN and is answered by the local DNS module.
//! B/D) The DNS servers configured before takeover (IPv4, plus IPv6 when the
//!    TUN has an IPv6 address) get host routes into the TUN, so queries sent
//!    straight to them are hijacked too.
//!
//! Every OS change is a pair of argv lists (apply/restore) snapshotted into the
//! network recovery journal before it is applied, restored in reverse order
//! when TUN stops, and replayed by crash recovery. Only the exact resolver
//! commands below are accepted from a recovery document.
mod ownership;
mod search;
#[cfg(target_os = "macos")]
pub(super) use search::mac_service_id;
use super::mode_config::Tun;
use anyhow::{bail, ensure, Context, Result};
use astra_model::config::dns::valid_search_domain;
pub(super) use ownership::Setting;
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr};

const NETWORKSETUP: &str = "/usr/sbin/networksetup";
const RESOLVECTL: &str = "resolvectl";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Change {
    apply: Vec<String>,
    restore: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Snapshot {
    changes: Vec<Change>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Os {
    MacOs,
    Linux,
    Windows,
}
impl Os {
    fn current() -> Result<Self> {
        if cfg!(target_os = "macos") {
            Ok(Self::MacOs)
        } else if cfg!(target_os = "linux") {
            Ok(Self::Linux)
        } else if cfg!(windows) {
            Ok(Self::Windows)
        } else {
            bail!("system DNS takeover is unsupported on this platform")
        }
    }
}
fn args(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

fn mac_values(service: &str, kind: &str, raw: &str) -> Result<Vec<String>> {
    let raw = raw.trim();
    if raw == format!("There aren't any {kind} set on {service}.") {
        return Ok(vec!["Empty".into()]);
    }
    ensure!(!raw.is_empty(), "missing {kind} response for {service}");
    Ok(raw.lines().map(|v| v.trim().to_owned()).collect())
}

fn resolved_values(raw: &str) -> Result<Vec<String>> {
    let (_, values) = raw
        .trim()
        .split_once(':')
        .context("invalid resolvectl link response")?;
    let mut values: Vec<String> = values.split_whitespace().map(str::to_owned).collect();
    // resolvectl uses a single empty argument to clear just this property.
    if values.is_empty() {
        values.push(String::new());
    }
    Ok(values)
}

pub(crate) fn plan(
    os: Os,
    interface: &str,
    address: Ipv4Addr,
    search_domain: Option<&str>,
    mut query: impl FnMut(&[String]) -> Result<String>,
) -> Result<Vec<Change>> {
    let address = address.to_string();
    match os {
        Os::MacOs => {
            let services: Vec<String> = query(&args(&[NETWORKSETUP, "-listallnetworkservices"]))?
                .lines()
                .skip(1)
                .map(str::trim)
                .filter(|service| !service.is_empty() && !service.starts_with('*'))
                .map(str::to_owned)
                .collect();
            ensure!(
                !services.is_empty(),
                "no enabled macOS network services found"
            );
            let mut changes = Vec::new();
            for service in services {
                let before = query(&args(&[NETWORKSETUP, "-getdnsservers", &service]))?;
                let mut restore = args(&[NETWORKSETUP, "-setdnsservers", &service]);
                restore.extend(mac_values(&service, "DNS Servers", &before)?);
                changes.push(Change {
                    apply: args(&[NETWORKSETUP, "-setdnsservers", &service, &address]),
                    restore,
                });
                if let Some(suffix) = search_domain {
                    changes.extend(search::macos(&service, suffix, &mut query)?);
                }
            }
            Ok(changes)
        }
        Os::Linux => {
            // The route-only root domain already selects this link for all
            // unmatched names. Setting default-route explicitly would lose
            // resolved's automatic/explicit distinction (its getter is bool).
            let domains = search::linux(interface, search_domain, &mut query)
                .context("system DNS takeover on Linux requires systemd-resolved (resolvectl)")?;
            let mut restore = args(&[RESOLVECTL, "dns", interface]);
            let raw = query(&restore)?;
            restore.extend(resolved_values(&raw)?);
            Ok(vec![
                domains,
                Change {
                    apply: args(&[RESOLVECTL, "dns", interface, &address]),
                    restore,
                },
            ])
        }
        Os::Windows => {
            let raw = query(&args(&[search::WIN_SERVERS_READ, interface]))?;
            let (guid, servers, old_suffix): (uuid::Uuid, Vec<Ipv4Addr>, String) =
                serde_json::from_str(raw.trim())?;
            let guid = guid.to_string();
            // An empty manual list denotes automatic/DHCP. Preserve ordering.
            let mut restore = args(&[search::WIN_SERVERS_SET, &guid]);
            restore.extend(servers.iter().map(ToString::to_string));
            let mut changes = vec![Change {
                apply: args(&[search::WIN_SERVERS_SET, &guid, &address]),
                restore,
            }];
            if let Some(suffix) = search_domain {
                changes.extend(search::windows(&guid, suffix, &old_suffix)?);
            }
            Ok(changes)
        }
    }
}

// resolved also supports a nonstandard DNS port, interface scope and TLS name.
fn valid_resolved_server(value: &str) -> bool {
    let (endpoint, tls) = value
        .split_once('#')
        .map_or((value, None), |(v, s)| (v, Some(s)));
    if tls.is_some_and(|s| !valid_search_domain(s)) {
        return false;
    }
    let (address, scope) = endpoint
        .split_once('%')
        .map_or((endpoint, None), |(v, s)| (v, Some(s)));
    if scope.is_some_and(|s| {
        s.is_empty()
            || s.len() > 15
            || s.bytes().any(|b| {
                b.is_ascii_whitespace() || b.is_ascii_control() || matches!(b, b'%' | b'/' | b'#')
            })
    }) {
        return false;
    }
    address.parse::<IpAddr>().is_ok() || address.parse::<std::net::SocketAddr>().is_ok()
}

/// Only these resolver commands may run from a (possibly edited) snapshot.
pub(crate) fn allowed(argv: &[String]) -> bool {
    let rest: Vec<&str> = argv.iter().skip(1).map(String::as_str).collect();
    match argv.first().map(String::as_str) {
        Some(NETWORKSETUP) => match rest.as_slice() {
            ["-setdnsservers", service, values @ ..] => {
                !service.is_empty()
                    && !service.starts_with('-')
                    && !values.is_empty()
                    && (values == ["Empty"] || values.iter().all(|v| v.parse::<IpAddr>().is_ok()))
            }
            ["-setsearchdomains", service, values @ ..] => {
                !service.is_empty()
                    && !service.starts_with('-')
                    && !values.is_empty()
                    && (values == ["Empty"]
                        || values
                            .iter()
                            .all(|v| valid_search_domain(v) && !v.eq_ignore_ascii_case("Empty")))
            }
            _ => false,
        },
        Some(search::WIN_SET) => {
            matches!(rest.as_slice(), [guid, domain] if uuid::Uuid::parse_str(guid).is_ok() && (domain.is_empty() || valid_search_domain(domain)))
        }
        Some(search::WIN_SERVERS_SET) => {
            matches!(rest.as_slice(), [guid, ips @ ..] if uuid::Uuid::parse_str(guid).is_ok() && ips.iter().all(|v| v.parse::<Ipv4Addr>().is_ok()))
        }
        Some(RESOLVECTL) => match rest.as_slice() {
            [key, link, values @ ..]
                if !link.is_empty() && !link.starts_with('-') && !values.is_empty() =>
            {
                match *key {
                    "dns" => values == [""] || values.iter().all(|v| valid_resolved_server(v)),
                    "domain" => values == [""] || values.iter().all(|v| search::linux_domain(v)),
                    _ => false,
                }
            }
            _ => false,
        },
        _ => false,
    }
}

fn run(argv: &[String]) -> Result<String> {
    ensure!(!argv.is_empty(), "empty system DNS command");
    #[cfg(target_os = "macos")]
    if argv[0] == search::MAC_DYNAMIC {
        return search::mac_service(&argv[1]).map(|(_, domains)| domains);
    }
    let mut command = if matches!(
        argv[0].as_str(),
        search::WIN_SET | search::WIN_SERVERS_READ | search::WIN_SERVERS_SET | search::WIN_BY_GUID
    ) {
        search::windows_command(argv)?
    } else {
        let mut command = std::process::Command::new(&argv[0]);
        command.args(&argv[1..]);
        command
    };
    command.env("LC_ALL", "C").env("SYSTEMD_COLORS", "0");
    let output = astra_platform::command::output(&mut command)?;
    ensure!(
        output.status.success(),
        "system DNS command {} failed: {}{}",
        argv[..argv.len().min(3)].join(" "),
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
/// Best effort: drop answers cached from the previous resolver.
fn flush() {
    let commands: &[&[&str]] = if cfg!(target_os = "macos") {
        &[
            &["dscacheutil", "-flushcache"],
            &["killall", "-HUP", "mDNSResponder"],
        ]
    } else if cfg!(target_os = "linux") {
        &[&[RESOLVECTL, "flush-caches"]]
    } else if cfg!(windows) {
        &[&["ipconfig", "/flushdns"]]
    } else {
        &[]
    };
    for command in commands {
        let _ = run(&args(command));
    }
}

pub(crate) struct Guard {
    active: Vec<ownership::Active>,
}
impl Guard {
    pub(crate) fn apply(interface: &str, address: Ipv4Addr, domain: Option<&str>) -> Result<Self> {
        Self::apply_settings(plan(Os::current()?, interface, address, domain, run)?)
    }
    fn apply_settings(changes: Vec<Change>) -> Result<Self> {
        let mut guard = Self { active: Vec::new() };
        for change in changes {
            guard.active.push(ownership::Active::apply(change)?);
        }
        flush();
        Ok(guard)
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        while self.active.pop().is_some() {}
        flush();
    }
}

/// The TUN-side address the OS resolver is pointed at: the first host of the
/// TUN subnet that is not the TUN's own address (198.18.0.2 by default).
pub(crate) fn dns_address(tun: &Tun) -> Option<Ipv4Addr> {
    let subnet = ipnet::Ipv4Net::new(tun.address, tun.prefix).ok()?;
    subnet.hosts().find(|host| *host != tun.address)
}

/// Host routes (CIDRs) for pre-takeover DNS servers that can safely be sent
/// into the TUN. Skips local/link-local/multicast, the TUN subnets, synthetic
/// ranges, physical gateways (routing them would break the default route),
/// tun.bypass networks and IPv6 when the TUN has no IPv6 address.
pub(crate) fn server_routes(
    servers: &[IpAddr],
    tun: &Tun,
    fake_ranges: &[ipnet::IpNet],
) -> Vec<String> {
    let tun4 = ipnet::Ipv4Net::new(tun.address, tun.prefix).ok();
    let tun6 = tun
        .ipv6_address
        .and_then(|address| ipnet::Ipv6Net::new(address, tun.ipv6_prefix).ok());
    let bypass: Vec<ipnet::IpNet> = tun.bypass.iter().filter_map(|c| c.parse().ok()).collect();
    let mut routes = Vec::new();
    for ip in servers {
        let skip = ip.is_unspecified()
            || ip.is_loopback()
            || ip.is_multicast()
            || Some(*ip) == tun.gateway
            || tun.gateway6.map(IpAddr::V6) == Some(*ip)
            || bypass.iter().any(|net| net.contains(ip))
            || fake_ranges.iter().any(|net| net.contains(ip))
            || match ip {
                IpAddr::V4(v4) => {
                    v4.is_link_local()
                        || v4.is_broadcast()
                        || tun4.is_some_and(|net| net.contains(v4))
                        || astra_model::config::dns::BENCHMARK_NET.contains(v4)
                }
                IpAddr::V6(v6) => {
                    tun6.is_none()
                        || v6.is_unicast_link_local()
                        || tun6.is_some_and(|net| net.contains(v6))
                }
            };
        if skip {
            continue;
        }
        let cidr = match ip {
            IpAddr::V4(_) => format!("{ip}/32"),
            IpAddr::V6(_) => format!("{ip}/128"),
        };
        if !routes.contains(&cidr) {
            routes.push(cidr);
        }
    }
    routes
}

/// Non-TUN listener. A normal macOS user may bind port 53 on the wildcard
/// address but not on 127.0.0.1; [`local_only`] keeps it loopback-only.
pub(crate) const LOCAL_LISTEN: std::net::SocketAddr =
    std::net::SocketAddr::V4(std::net::SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 53));
fn local_wanted(config: &crate::config::ProxyConfig) -> bool {
    let wanted = config.tun.is_none()
        && config
            .dns
            .as_ref()
            .is_some_and(|dns| dns.enable && dns.system_takeover_without_tun);
    if wanted && !cfg!(target_os = "macos") {
        tracing::warn!(
            "dns.system_takeover_without_tun is supported on macOS only; enable TUN instead"
        );
        return false;
    }
    wanted
}
/// The extra DNS listener non-TUN takeover needs, if any.
pub(crate) fn local_listener(config: &crate::config::ProxyConfig) -> Option<std::net::SocketAddr> {
    local_wanted(config).then_some(LOCAL_LISTEN)
}
/// True when `local` is the takeover listener added implicitly (not listed in
/// dns.listen): it must answer only this machine, whatever allow_lan says.
pub(crate) fn local_only(app: &crate::App, local: std::net::SocketAddr) -> bool {
    local == LOCAL_LISTEN
        && app
            .dns
            .as_ref()
            .is_none_or(|dns| !dns.config.listen.contains(&local))
}

/// What decides whether an active takeover can be kept across a reload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Key {
    Tun {
        interface: String,
        tun: Tun,
        fake_ranges: Vec<ipnet::IpNet>,
        takeover: bool,
        route: bool,
        domain: Option<String>,
    },
    /// No TUN (macOS): the OS resolver points at 127.0.0.1 and is answered by
    /// [`LOCAL_LISTEN`].
    Local(Option<String>),
}
impl Key {
    /// None when takeover does not apply. `interface` is the live TUN device.
    pub(crate) fn wanted(
        config: &crate::config::ProxyConfig,
        interface: Option<String>,
    ) -> Option<Self> {
        let dns = config.dns.as_ref()?;
        if local_wanted(config) {
            return Some(Self::Local(dns.magic_dns.search_domain()));
        }
        let tun = config.tun.clone()?;
        if !(dns.enable && dns.hijack && (dns.system_takeover || dns.route_system_dns)) {
            return None;
        }
        Some(Self::Tun {
            interface: interface?,
            tun,
            fake_ranges: dns.active_fake_ranges().collect(),
            takeover: dns.system_takeover,
            route: dns.route_system_dns,
            domain: if dns.system_takeover {
                dns.magic_dns.search_domain()
            } else {
                None
            },
        })
    }
}
async fn apply_blocking(
    interface: String,
    address: Ipv4Addr,
    domain: Option<String>,
) -> Result<Guard> {
    tokio::task::spawn_blocking(move || Guard::apply(&interface, address, domain.as_deref()))
        .await?
}
/// Active takeover for one generation. Field order is drop order: the OS
/// resolver is restored before the server routes are removed.
pub(crate) struct Takeover {
    pub(crate) key: Key,
    _dns: Option<Guard>,
    _routes: super::tun::Routes,
}
impl Takeover {
    /// Never fails: each part degrades to a warning so the core stays up.
    pub(crate) async fn start(key: Key) -> Self {
        let mut routes = super::tun::Routes::default();
        let mut dns = None;
        match &key {
            Key::Local(domain) => {
                match apply_blocking(String::new(), Ipv4Addr::LOCALHOST, domain.clone()).await {
                    Ok(guard) => dns = Some(guard),
                    Err(error) => tracing::warn!(%error, "system DNS takeover without TUN failed"),
                }
            }
            Key::Tun {
                interface,
                tun,
                fake_ranges,
                takeover,
                route,
                domain,
            } => {
                let address = dns_address(tun);
                let mut cidrs = Vec::new();
                if let Some(address) = address.filter(|_| *takeover) {
                    // Some platforms (macOS utun) do not route the peer subnet.
                    cidrs.push(format!("{address}/32"));
                }
                if *route {
                    // Read before A changes the OS resolver.
                    let servers = astra_platform::network_snapshot::dns_servers().await;
                    cidrs.extend(server_routes(&servers, tun, fake_ranges));
                }
                for cidr in cidrs {
                    if let Err(error) = routes.add_into(&cidr, interface) {
                        tracing::warn!(%cidr, %error, "system DNS route not installed");
                    }
                }
                if *takeover {
                    match address {
                        Some(address) => match apply_blocking(interface.clone(), address, domain.clone()).await {
                            Ok(guard) => dns = Some(guard),
                            Err(error) => tracing::warn!(
                                %error,
                                "system DNS takeover failed; TUN port-53 hijack remains"
                            ),
                        },
                        None => tracing::warn!(
                            "tun.prefix leaves no spare address for system DNS; use a /30 or wider TUN subnet"
                        ),
                    }
                }
            }
        }
        Self {
            key,
            _dns: dns,
            _routes: routes,
        }
    }
}

#[cfg(test)]
#[path = "../test/system_dns_test.rs"]
mod tests;
