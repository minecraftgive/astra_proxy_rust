//! Windows interface lookup, socket pinning and owned, non-persistent routes.
use anyhow::{ensure, Context, Result};
use std::{net::IpAddr, os::windows::io::AsRawSocket};
use windows_sys::Win32::{
    NetworkManagement::{IpHelper::*, Ndis::NET_LUID_LH},
    Networking::WinSock::*,
};

pub fn interface_index(interface: &str) -> Result<u32> {
    let mut luid = NET_LUID_LH::default();
    let mut index = 0;
    if let Ok(value) = interface.parse::<u32>() {
        ensure!(value != 0, "interface index must be nonzero");
        check(unsafe { ConvertInterfaceIndexToLuid(value, &mut luid) })
            .context("unknown Windows interface index")?;
        return Ok(value);
    }
    ensure!(!interface.contains('\0'), "invalid interface alias");
    let alias: Vec<u16> = interface.encode_utf16().chain(Some(0)).collect();
    check(unsafe { ConvertInterfaceAliasToLuid(alias.as_ptr(), &mut luid) })
        .with_context(|| format!("unknown Windows interface {interface}"))?;
    check(unsafe { ConvertInterfaceLuidToIndex(&luid, &mut index) })?;
    Ok(index)
}

fn check(code: u32) -> Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(std::io::Error::from_raw_os_error(code as i32).into())
    }
}

/// Choose a live hardware adapter with a default route. Never select a
/// virtual adapter merely because it has a lower metric or a split route.
pub fn physical_default_interface(exclude: Option<&str>) -> Result<String> {
    let mut pointer = std::ptr::null_mut();
    check(unsafe { GetIpForwardTable2(AF_UNSPEC, &mut pointer) })?;
    let table = RouteTable(pointer);
    let rows = unsafe {
        std::slice::from_raw_parts((*table.0).Table.as_ptr(), (*table.0).NumEntries as usize)
    };
    let mut candidates = Vec::new();
    for route in rows {
        if route.DestinationPrefix.PrefixLength != 0 {
            continue;
        }
        let mut adapter = MIB_IF_ROW2 {
            InterfaceIndex: route.InterfaceIndex,
            ..Default::default()
        };
        if unsafe { GetIfEntry2(&mut adapter) } != 0
            || adapter.OperStatus != windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp
            || adapter.InterfaceAndOperStatusFlags._bitfield & 1 == 0
        {
            continue;
        }
        let alias = String::from_utf16_lossy(
            &adapter.Alias[..adapter
                .Alias
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(adapter.Alias.len())],
        );
        if exclude.is_some_and(|name| name == alias || name == route.InterfaceIndex.to_string()) {
            continue;
        }
        let family = unsafe { route.DestinationPrefix.Prefix.si_family };
        let mut interface = MIB_IPINTERFACE_ROW::default();
        unsafe { InitializeIpInterfaceEntry(&mut interface) };
        interface.Family = family;
        interface.InterfaceIndex = route.InterfaceIndex;
        if unsafe { GetIpInterfaceEntry(&mut interface) } != 0 {
            continue;
        }
        // Prefer the IPv4 default; IPv6-only networks still have a candidate.
        candidates.push((
            family != AF_INET,
            u64::from(route.Metric) + u64::from(interface.Metric),
            route.InterfaceIndex,
            alias,
        ));
    }
    candidates.sort();
    candidates
        .into_iter()
        .next()
        .map(|(_, _, _, name)| name)
        .context("no active physical default route; configure tun.interface explicitly")
}

pub fn interface_alias(interface: &str) -> Result<String> {
    let mut row = MIB_IF_ROW2 {
        InterfaceIndex: interface_index(interface)?,
        ..Default::default()
    };
    check(unsafe { GetIfEntry2(&mut row) })?;
    Ok(String::from_utf16_lossy(
        &row.Alias[..row
            .Alias
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(row.Alias.len())],
    ))
}

pub(super) fn physical_egress(
    selected: Option<&str>,
    exclude: Option<&str>,
) -> Result<super::egress::PhysicalEgress> {
    let interface = match selected {
        Some(interface) => interface_alias(interface)?,
        None => physical_default_interface(exclude)?,
    };
    let index = interface_index(&interface)?;
    let mut adapter = MIB_IF_ROW2 {
        InterfaceIndex: index,
        ..Default::default()
    };
    check(unsafe { GetIfEntry2(&mut adapter) })?;
    ensure!(
        adapter.OperStatus == windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp,
        "egress interface {interface} is disconnected"
    );
    let mut pointer = std::ptr::null_mut();
    check(unsafe { GetIpForwardTable2(AF_UNSPEC, &mut pointer) })?;
    let table = RouteTable(pointer);
    let rows = unsafe {
        std::slice::from_raw_parts((*table.0).Table.as_ptr(), (*table.0).NumEntries as usize)
    };
    let mut defaults: Vec<_> = rows
        .iter()
        .filter(|row| row.InterfaceIndex == index && row.DestinationPrefix.PrefixLength == 0)
        .collect();
    defaults.sort_by_key(|row| row.Metric);
    let mut result = super::egress::PhysicalEgress {
        interface,
        gateway: None,
        gateway6: None,
    };
    let mut found = false;
    for row in defaults {
        let Some((gateway, _)) = address(row.NextHop) else {
            continue;
        };
        found = true;
        match gateway {
            IpAddr::V4(ip) if result.gateway.is_none() && !ip.is_unspecified() => {
                result.gateway = Some(ip)
            }
            IpAddr::V6(ip) if result.gateway6.is_none() && !ip.is_unspecified() => {
                result.gateway6 = Some(ip)
            }
            _ => {}
        }
    }
    ensure!(
        found,
        "no active default gateway on egress interface {}",
        result.interface
    );
    Ok(result)
}

#[cfg(test)]
mod egress_tests {
    use super::*;
    fn row(cidr: &str, index: u32, gateway: &str) -> MIB_IPFORWARD_ROW2 {
        let net: ipnet::IpNet = cidr.parse().unwrap();
        MIB_IPFORWARD_ROW2 {
            InterfaceIndex: index,
            DestinationPrefix: IP_ADDRESS_PREFIX {
                Prefix: sockaddr(net.network(), index),
                PrefixLength: net.prefix_len(),
            },
            NextHop: sockaddr(gateway.parse().unwrap(), index),
            Metric: 5,
            Protocol: MIB_IPPROTO_NETMGMT,
            ..Default::default()
        }
    }
    #[test]
    fn interface_handoff_allows_only_exact_process_owned_routes() {
        let prefix = "172.67.166.176/32".parse().unwrap();
        let wired = row("172.67.166.176/32", 26, "192.0.2.1");
        let wifi = row("172.67.166.176/32", 7, "192.0.2.254");
        let owned = vec![RouteIdentity::from_row(&wired).unwrap()];
        assert!(validate_prefix_rows(prefix, &[wired], &[]).is_err());
        assert!(validate_prefix_rows(prefix, &[wired], &owned).is_ok());
        assert!(validate_prefix_rows(prefix, &[wired, wifi], &owned).is_err());
        let both = vec![
            RouteIdentity::from_row(&wired).unwrap(),
            RouteIdentity::from_row(&wifi).unwrap(),
        ];
        assert!(validate_prefix_rows(prefix, &[wired, wifi], &both).is_ok());
        let mut foreign = wired;
        foreign.Metric += 1;
        assert!(validate_prefix_rows(prefix, &[foreign], &owned).is_err());
        foreign = wired;
        foreign.NextHop = sockaddr("192.0.2.99".parse().unwrap(), 26);
        assert!(validate_prefix_rows(prefix, &[foreign], &owned).is_err());
        let unrelated = row("203.0.113.8/32", 26, "192.0.2.1");
        assert!(validate_prefix_rows(prefix, &[unrelated], &[]).is_ok());
        let v6 = row("2001:db8::8/128", 26, "fe80::1");
        assert!(validate_prefix_rows(
            "2001:db8::8/128".parse().unwrap(),
            &[v6],
            &[RouteIdentity::from_row(&v6).unwrap()]
        )
        .is_ok());
    }

    #[test]
    fn discovered_default_is_live_hardware_and_can_be_excluded() {
        // A disconnected CI host may legitimately have no physical default.
        let Ok(alias) = physical_default_interface(None) else {
            return;
        };
        let index = interface_index(&alias).unwrap();
        let mut row = MIB_IF_ROW2 {
            InterfaceIndex: index,
            ..Default::default()
        };
        check(unsafe { GetIfEntry2(&mut row) }).unwrap();
        assert_ne!(row.InterfaceAndOperStatusFlags._bitfield & 1, 0);
        assert_eq!(
            row.OperStatus,
            windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp
        );
        assert_eq!(interface_alias(&index.to_string()).unwrap(), alias);
        assert_ne!(
            physical_default_interface(Some(&alias)).ok(),
            Some(alias.clone())
        );
        eprintln!("physical egress: {alias} (index {index})");
    }
}

pub fn bind_socket(socket: &impl AsRawSocket, interface: &str, ipv4: bool) -> Result<()> {
    let index = interface_index(interface)?;
    // Windows IP_UNICAST_IF takes network byte order; IPV6_UNICAST_IF takes host order.
    let value = if ipv4 { index.to_be() } else { index };
    let result = unsafe {
        setsockopt(
            socket.as_raw_socket() as SOCKET,
            if ipv4 { IPPROTO_IP } else { IPPROTO_IPV6 },
            if ipv4 { IP_UNICAST_IF } else { IPV6_UNICAST_IF },
            (&value as *const u32).cast(),
            std::mem::size_of_val(&value) as i32,
        )
    };
    ensure!(
        result != SOCKET_ERROR,
        "bind Windows egress interface: {}",
        std::io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
    );
    Ok(())
}

fn sockaddr(address: IpAddr, scope: u32) -> SOCKADDR_INET {
    match address {
        IpAddr::V4(ip) => SOCKADDR_INET {
            Ipv4: SOCKADDR_IN {
                sin_family: AF_INET,
                sin_addr: IN_ADDR {
                    S_un: IN_ADDR_0 {
                        S_addr: u32::from_ne_bytes(ip.octets()),
                    },
                },
                ..Default::default()
            },
        },
        IpAddr::V6(ip) => SOCKADDR_INET {
            Ipv6: SOCKADDR_IN6 {
                sin6_family: AF_INET6,
                sin6_addr: IN6_ADDR {
                    u: IN6_ADDR_0 { Byte: ip.octets() },
                },
                Anonymous: SOCKADDR_IN6_0 {
                    sin6_scope_id: if ip.is_unicast_link_local() { scope } else { 0 },
                },
                ..Default::default()
            },
        },
    }
}

pub struct Route {
    _owner: std::sync::Arc<RouteOwner>,
}
struct RouteOwner {
    row: MIB_IPFORWARD_ROW2,
    identity: RouteIdentity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RouteIdentity {
    network: ipnet::IpNet,
    interface: u32,
    next_hop: IpAddr,
    scope: u32,
    metric: u32,
    protocol: i32,
}
fn address(value: SOCKADDR_INET) -> Option<(IpAddr, u32)> {
    unsafe {
        match value.si_family {
            AF_INET => Some((
                std::net::Ipv4Addr::from(value.Ipv4.sin_addr.S_un.S_addr.to_ne_bytes()).into(),
                0,
            )),
            AF_INET6 => Some((
                std::net::Ipv6Addr::from(value.Ipv6.sin6_addr.u.Byte).into(),
                value.Ipv6.Anonymous.sin6_scope_id,
            )),
            _ => None,
        }
    }
}
impl RouteIdentity {
    fn from_row(row: &MIB_IPFORWARD_ROW2) -> Option<Self> {
        let (prefix, _) = address(row.DestinationPrefix.Prefix)?;
        let (next_hop, scope) = address(row.NextHop)?;
        Some(Self {
            network: ipnet::IpNet::new(prefix, row.DestinationPrefix.PrefixLength)
                .ok()?
                .trunc(),
            interface: row.InterfaceIndex,
            next_hop,
            scope,
            metric: row.Metric,
            protocol: row.Protocol,
        })
    }
}
fn owners() -> &'static std::sync::Mutex<Vec<std::sync::Weak<RouteOwner>>> {
    static OWNERS: std::sync::OnceLock<std::sync::Mutex<Vec<std::sync::Weak<RouteOwner>>>> =
        std::sync::OnceLock::new();
    OWNERS.get_or_init(Default::default)
}
fn validate_prefix_rows(
    network: ipnet::IpNet,
    rows: &[MIB_IPFORWARD_ROW2],
    owned: &[RouteIdentity],
) -> Result<()> {
    for row in rows {
        let Some(identity) = RouteIdentity::from_row(row) else {
            continue;
        };
        if identity.network != network.trunc() {
            continue;
        }
        ensure!(owned.iter().any(|old|old==&identity),
            "Windows route prefix {} already exists on interface {}; route is not owned by this proxy instance (or was changed externally)",network.trunc(),row.InterfaceIndex);
    }
    Ok(())
}

struct RouteTable(*mut MIB_IPFORWARD_TABLE2);
impl Drop for RouteTable {
    fn drop(&mut self) {
        unsafe {
            FreeMibTable(self.0.cast());
        }
    }
}
fn reject_existing_prefix(network: ipnet::IpNet) -> Result<()> {
    // Keep owners alive while reading the table, so a concurrently ending old
    // session cannot make our snapshot appear to contain an unowned route.
    let alive = {
        let mut registry = owners().lock().unwrap();
        registry.retain(|owner| owner.strong_count() > 0);
        registry
            .iter()
            .filter_map(std::sync::Weak::upgrade)
            .collect::<Vec<_>>()
    };
    let mut pointer = std::ptr::null_mut();
    check(unsafe { GetIpForwardTable2(AF_UNSPEC, &mut pointer) })
        .context("read Windows route table")?;
    let table = RouteTable(pointer);
    let rows = unsafe {
        std::slice::from_raw_parts((*table.0).Table.as_ptr(), (*table.0).NumEntries as usize)
    };
    let owned: Vec<_> = alive.iter().map(|owner| owner.identity.clone()).collect();
    validate_prefix_rows(network, rows, &owned)
}
impl Route {
    pub fn create(cidr: &str, interface: &str, gateway: Option<IpAddr>) -> Result<Self> {
        let network: ipnet::IpNet = cidr.parse()?;
        let index = interface_index(interface)?;
        let next = gateway.unwrap_or_else(|| {
            if network.addr().is_ipv4() {
                IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)
            } else {
                IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED)
            }
        });
        ensure!(
            next.is_ipv4() == network.addr().is_ipv4(),
            "route and gateway address families differ"
        );
        let mut row = MIB_IPFORWARD_ROW2::default();
        unsafe {
            InitializeIpForwardEntry(&mut row);
        }
        row.InterfaceIndex = index;
        row.DestinationPrefix.Prefix = sockaddr(network.network(), index);
        row.DestinationPrefix.PrefixLength = network.prefix_len();
        row.NextHop = sockaddr(next, index);
        row.Protocol = MIB_IPPROTO_NETMGMT;
        row.Metric = 5;
        let identity =
            RouteIdentity::from_row(&row).context("invalid owned Windows route identity")?;
        let existing = {
            let mut registry = owners().lock().unwrap();
            registry.retain(|owner| owner.strong_count() > 0);
            registry
                .iter()
                .filter_map(std::sync::Weak::upgrade)
                .find(|owner| owner.identity == identity)
        };
        if let Some(owner) = existing {
            let mut current = owner.row;
            let found = unsafe { GetIpForwardEntry2(&mut current) };
            if found == 1168 {
                reject_existing_prefix(network)?;
                check(unsafe { CreateIpForwardEntry2(&owner.row) })
                    .context("restore a missing owned route")?;
            } else {
                check(found)?;
                ensure!(
                    RouteIdentity::from_row(&current).as_ref() == Some(&identity),
                    "owned route changed externally; refusing to reuse it"
                );
            }
            return Ok(Self { _owner: owner });
        }
        reject_existing_prefix(network)?;
        check(unsafe { CreateIpForwardEntry2(&row) }).with_context(|| format!("add Windows route {cidr} via interface {index} (requires administrator; existing routes are never overwritten)"))?;
        let owner = std::sync::Arc::new(RouteOwner { row, identity });
        owners()
            .lock()
            .unwrap()
            .push(std::sync::Arc::downgrade(&owner));
        Ok(Self { _owner: owner })
    }
}
impl Drop for RouteOwner {
    fn drop(&mut self) {
        let mut current = self.row;
        let found = unsafe { GetIpForwardEntry2(&mut current) };
        if found == 1168 {
            return;
        } // ERROR_NOT_FOUND: interface/route already removed.
        if found != 0 || current.Metric != self.row.Metric || current.Protocol != self.row.Protocol
        {
            tracing::warn!(
                error = found,
                "owned Windows route changed externally; preserving it"
            );
            return;
        }
        if let Err(error) = check(unsafe { DeleteIpForwardEntry2(&self.row) }) {
            tracing::error!(%error, "failed to remove owned Windows TUN route");
        }
    }
}
