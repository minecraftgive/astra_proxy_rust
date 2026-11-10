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
use super::mode_config::Tun;
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr};

const NETWORKSETUP: &str = "/usr/sbin/networksetup";
const RESOLVECTL: &str = "resolvectl";
const NETSH: &str = "netsh";

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

/// Build the apply/restore pairs. `query` only reads OS state.
pub(crate) fn plan(
    os: Os,
    interface: &str,
    address: Ipv4Addr,
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
                let old: Vec<String> = before
                    .lines()
                    .map(str::trim)
                    .filter(|line| line.parse::<IpAddr>().is_ok())
                    .map(str::to_owned)
                    .collect();
                let mut restore = args(&[NETWORKSETUP, "-setdnsservers", &service]);
                if old.is_empty() {
                    restore.push("Empty".into());
                } else {
                    restore.extend(old);
                }
                changes.push(Change {
                    apply: args(&[NETWORKSETUP, "-setdnsservers", &service, &address]),
                    restore,
                });
            }
            Ok(changes)
        }
        Os::Linux => {
            // Per-link settings on the TUN vanish with the link; revert covers
            // a reload that keeps the device.
            query(&args(&[RESOLVECTL, "status", interface]))
                .context("system DNS takeover on Linux requires systemd-resolved (resolvectl)")?;
            let revert = args(&[RESOLVECTL, "revert", interface]);
            Ok([
                args(&[RESOLVECTL, "dns", interface, &address]),
                args(&[RESOLVECTL, "domain", interface, "~."]),
                args(&[RESOLVECTL, "default-route", interface, "yes"]),
            ]
            .into_iter()
            .map(|apply| Change {
                apply,
                restore: revert.clone(),
            })
            .collect())
        }
        Os::Windows => Ok(vec![Change {
            apply: args(&[
                NETSH,
                "interface",
                "ipv4",
                "set",
                "dnsservers",
                &format!("name={interface}"),
                "source=static",
                &format!("address={address}"),
                "register=none",
                "validate=no",
            ]),
            restore: args(&[
                NETSH,
                "interface",
                "ipv4",
                "set",
                "dnsservers",
                &format!("name={interface}"),
                "source=dhcp",
            ]),
        }]),
    }
}

/// Only these resolver commands may run from a (possibly edited) snapshot.
pub(crate) fn allowed(argv: &[String]) -> bool {
    let rest: Vec<&str> = argv.iter().skip(1).map(String::as_str).collect();
    match argv.first().map(String::as_str) {
        Some(NETWORKSETUP) => {
            rest.len() >= 3
                && rest[0] == "-setdnsservers"
                && !rest[1].is_empty()
                && rest[2..]
                    .iter()
                    .all(|value| *value == "Empty" || value.parse::<IpAddr>().is_ok())
        }
        Some(RESOLVECTL) => match rest.as_slice() {
            ["revert", link] => !link.is_empty(),
            ["dns", link, ip] => !link.is_empty() && ip.parse::<IpAddr>().is_ok(),
            ["domain", link, "~."] | ["default-route", link, "yes"] => !link.is_empty(),
            _ => false,
        },
        Some(NETSH) => {
            rest.len() >= 6
                && rest[..4] == ["interface", "ipv4", "set", "dnsservers"]
                && rest[4].starts_with("name=")
                && rest[5..].iter().all(|value| {
                    matches!(
                        *value,
                        "source=static" | "source=dhcp" | "register=none" | "validate=no"
                    ) || value
                        .strip_prefix("address=")
                        .is_some_and(|ip| ip.parse::<IpAddr>().is_ok())
                })
        }
        _ => false,
    }
}

fn run(argv: &[String]) -> Result<String> {
    ensure!(!argv.is_empty(), "empty system DNS command");
    let output =
        astra_platform::command::output(std::process::Command::new(&argv[0]).args(&argv[1..]))?;
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

/// Applied OS resolver settings; restored on drop.
pub(crate) struct Guard {
    changes: Vec<Change>,
    applied: usize,
    lease: Option<super::network_recovery::Lease>,
}
impl Guard {
    pub(crate) fn apply(interface: &str, address: Ipv4Addr) -> Result<Self> {
        let changes = plan(Os::current()?, interface, address, run)?;
        ensure!(
            changes
                .iter()
                .all(|change| allowed(&change.apply) && allowed(&change.restore)),
            "unexpected system DNS command"
        );
        let lease = super::network_recovery::Lease::prepare(
            super::network_recovery::Resource::SystemDns(Snapshot {
                changes: changes.clone(),
            }),
        )?;
        let mut guard = Self {
            changes,
            applied: 0,
            lease: Some(lease),
        };
        for index in 0..guard.changes.len() {
            run(&guard.changes[index].apply)?;
            guard.applied = index + 1;
        }
        flush();
        if let Some(lease) = &guard.lease {
            lease.commit()?;
        }
        if interface.is_empty() {
            tracing::info!(%address, "system DNS now points at the local DNS listener");
        } else {
            tracing::info!(%interface, %address, "system DNS now points at the TUN");
        }
        Ok(guard)
    }
    fn restore(&mut self) -> Result<()> {
        let mut error = None;
        for change in self.changes[..self.applied].iter().rev() {
            if let Err(e) = run(&change.restore) {
                error = Some(e);
            }
        }
        if self.applied > 0 {
            flush();
        }
        if let Some(error) = error {
            return Err(error);
        }
        if let Some(lease) = self.lease.take() {
            lease.complete()?;
        }
        self.applied = 0;
        self.changes.clear();
        Ok(())
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            if let Some(lease) = &self.lease {
                lease.failed(&error);
            }
            tracing::error!(%error, "failed to restore system DNS settings");
        }
    }
}
/// Crash recovery: replay restore commands, newest first. Restores are
/// idempotent, so a partially applied snapshot is safe to replay.
pub(super) fn recover(snapshot: &Snapshot) -> Result<()> {
    for change in snapshot.changes.iter().rev() {
        ensure!(
            allowed(&change.restore),
            "unexpected system DNS recovery command"
        );
        run(&change.restore)?;
    }
    flush();
    Ok(())
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
pub(crate) fn server_routes(servers: &[IpAddr], tun: &Tun) -> Vec<String> {
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
            || match ip {
                IpAddr::V4(v4) => {
                    v4.is_link_local()
                        || v4.is_broadcast()
                        || tun4.is_some_and(|net| net.contains(v4))
                        || (u32::from(*v4) & 0xfffe_0000) == u32::from(Ipv4Addr::new(198, 18, 0, 0))
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
        takeover: bool,
        route: bool,
    },
    /// No TUN (macOS): the OS resolver points at 127.0.0.1 and is answered by
    /// [`LOCAL_LISTEN`].
    Local,
}
impl Key {
    /// None when takeover does not apply. `interface` is the live TUN device.
    pub(crate) fn wanted(
        config: &crate::config::ProxyConfig,
        interface: Option<String>,
    ) -> Option<Self> {
        if local_wanted(config) {
            return Some(Self::Local);
        }
        let tun = config.tun.clone()?;
        let dns = config.dns.as_ref()?;
        if !(dns.enable && dns.hijack && (dns.system_takeover || dns.route_system_dns)) {
            return None;
        }
        Some(Self::Tun {
            interface: interface?,
            tun,
            takeover: dns.system_takeover,
            route: dns.route_system_dns,
        })
    }
}
async fn apply_blocking(interface: String, address: Ipv4Addr) -> Result<Guard> {
    tokio::task::spawn_blocking(move || Guard::apply(&interface, address)).await?
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
            Key::Local => match apply_blocking(String::new(), Ipv4Addr::LOCALHOST).await {
                Ok(guard) => dns = Some(guard),
                Err(error) => tracing::warn!(%error, "system DNS takeover without TUN failed"),
            },
            Key::Tun {
                interface,
                tun,
                takeover,
                route,
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
                    cidrs.extend(server_routes(&servers, tun));
                }
                for cidr in cidrs {
                    if let Err(error) = routes.add_into(&cidr, interface) {
                        tracing::warn!(%cidr, %error, "system DNS route not installed");
                    }
                }
                if *takeover {
                    match address {
                        Some(address) => match apply_blocking(interface.clone(), address).await {
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
