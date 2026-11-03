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
    lease: super::network_recovery::Lease,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) struct RouteIdentity {
    network: ipnet::IpNet,
    luid: u64,
    adapter: Option<[u8; 16]>,
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
            luid: unsafe { row.InterfaceLuid.Value },
            adapter: adapter_guid(row.InterfaceIndex).ok(),
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
        check(unsafe { ConvertInterfaceIndexToLuid(index, &mut row.InterfaceLuid) })?;
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
        let lease = super::network_recovery::Lease::prepare(
            super::network_recovery::Resource::WindowsRoute(identity.clone()),
        )?;
        check(unsafe { CreateIpForwardEntry2(&row) }).with_context(|| format!("add Windows route {cidr} via interface {index} (requires administrator; existing routes are never overwritten)"))?;
        let owner = std::sync::Arc::new(RouteOwner {
            row,
            identity,
            lease,
        });
        owner.lease.commit()?;
        owners()
            .lock()
            .unwrap()
            .push(std::sync::Arc::downgrade(&owner));
        Ok(Self { _owner: owner })
    }
}
impl Drop for RouteOwner {
    fn drop(&mut self) {
        match recover_route(&self.identity, true) {
            Ok(()) => {
                if let Err(error) = self.lease.complete() {
                    tracing::error!(%error, "route cleanup checkpoint failed");
                }
            }
            Err(error) => {
                self.lease.failed(&error);
                tracing::error!(%error, "owned route recovery remains pending");
            }
        }
    }
}
pub(super) fn recover_route(identity: &RouteIdentity, committed: bool) -> Result<()> {
    ensure!(
        identity.adapter.is_some(),
        "route adapter identity was not recorded"
    );
    let mut row = MIB_IPFORWARD_ROW2::default();
    unsafe {
        InitializeIpForwardEntry(&mut row);
        row.InterfaceLuid.Value = identity.luid;
    }
    row.InterfaceIndex = identity.interface;
    row.DestinationPrefix.Prefix = sockaddr(identity.network.network(), identity.interface);
    row.DestinationPrefix.PrefixLength = identity.network.prefix_len();
    row.NextHop = sockaddr(identity.next_hop, identity.scope);
    let found = unsafe { GetIpForwardEntry2(&mut row) };
    if found == 1168 || found == 2 {
        return Ok(());
    }
    check(found)?;
    ensure!(
        committed,
        "route creation outcome is unknown; refusing to remove an unconfirmed route"
    );
    ensure!(
        RouteIdentity::from_row(&row).as_ref() == Some(identity),
        "route changed externally; preserving it"
    );
    check(unsafe { DeleteIpForwardEntry2(&row) })
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct InterfaceParameters {
    family: u16,
    mtu: u32,
    metric: u32,
    automatic: bool,
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct AddressProperties {
    valid_lifetime: u32,
    preferred_lifetime: u32,
    prefix_origin: i32,
    suffix_origin: i32,
    skip_as_source: bool,
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct AdapterAddress {
    address: IpAddr,
    prefix: u8,
    #[serde(default)]
    properties: Option<AddressProperties>,
}
// Address lifetimes count down and DAD state changes without an external edit.
// Existing matching addresses are left untouched; captured properties are used
// only when recreating an original address removed by our IPv4 replacement.
impl PartialEq for AdapterAddress {
    fn eq(&self, other: &Self) -> bool {
        self.address == other.address && self.prefix == other.prefix
    }
}
impl Eq for AdapterAddress {}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct AdapterValues {
    parameters: Vec<InterfaceParameters>,
    addresses: Vec<AdapterAddress>,
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(super) struct AdapterSnapshot {
    index: u32,
    guid: [u8; 16],
    before: AdapterValues,
    expected: AdapterValues,
}
fn adapter_guid(index: u32) -> Result<[u8; 16]> {
    let mut row = MIB_IF_ROW2 {
        InterfaceIndex: index,
        ..Default::default()
    };
    check(unsafe { GetIfEntry2(&mut row) })?;
    Ok(unsafe { std::mem::transmute::<windows_sys::core::GUID, [u8; 16]>(row.InterfaceGuid) })
}
fn adapter_values(index: u32) -> Result<AdapterValues> {
    let mut parameters = vec![];
    for family in [AF_INET, AF_INET6] {
        let mut row = MIB_IPINTERFACE_ROW {
            Family: family,
            InterfaceIndex: index,
            ..Default::default()
        };
        check(unsafe { GetIpInterfaceEntry(&mut row) })?;
        parameters.push(InterfaceParameters {
            family,
            mtu: row.NlMtu,
            metric: row.Metric,
            automatic: row.UseAutomaticMetric,
        });
    }
    let mut table = std::ptr::null_mut();
    check(unsafe { GetUnicastIpAddressTable(AF_UNSPEC, &mut table) })?;
    let mut addresses = vec![];
    unsafe {
        for row in std::slice::from_raw_parts((*table).Table.as_ptr(), (*table).NumEntries as usize)
        {
            if row.InterfaceIndex == index {
                if let Some((address, _)) = address(row.Address) {
                    addresses.push(AdapterAddress {
                        address,
                        prefix: row.OnLinkPrefixLength,
                        properties: Some(AddressProperties {
                            valid_lifetime: row.ValidLifetime,
                            preferred_lifetime: row.PreferredLifetime,
                            prefix_origin: row.PrefixOrigin,
                            suffix_origin: row.SuffixOrigin,
                            skip_as_source: row.SkipAsSource,
                        }),
                    });
                }
            }
        }
        FreeMibTable(table.cast());
    }
    addresses.sort_by_key(|a| (a.address, a.prefix));
    Ok(AdapterValues {
        parameters,
        addresses,
    })
}
pub(super) struct AdapterGuard {
    snapshot: AdapterSnapshot,
    lease: super::network_recovery::Lease,
}
impl AdapterGuard {
    pub fn prepare(index: u32, config: &super::mode_config::Tun) -> Result<Self> {
        let guid = adapter_guid(index)?;
        let before = adapter_values(index)?;
        let mut expected = before.clone();
        for p in &mut expected.parameters {
            if p.family == AF_INET {
                p.mtu = config.mtu.into();
            }
            p.metric = 5;
            p.automatic = false;
        }
        expected.addresses.retain(|a| a.address.is_ipv6());
        expected.addresses.push(AdapterAddress {
            address: config.address.into(),
            prefix: config.prefix,
            properties: None,
        });
        if let Some(address) = config.ipv6_address {
            let value = AdapterAddress {
                address: address.into(),
                prefix: config.ipv6_prefix,
                properties: None,
            };
            if !expected.addresses.contains(&value) {
                expected.addresses.push(value);
            }
        }
        expected.addresses.sort_by_key(|a| (a.address, a.prefix));
        let snapshot = AdapterSnapshot {
            index,
            guid,
            before,
            expected,
        };
        let lease = super::network_recovery::Lease::prepare(
            super::network_recovery::Resource::WindowsAdapter(snapshot.clone()),
        )?;
        Ok(Self { snapshot, lease })
    }
    pub fn commit(&self) -> Result<()> {
        self.lease.commit()
    }
}
impl Drop for AdapterGuard {
    fn drop(&mut self) {
        match recover_adapter(&self.snapshot, true) {
            Ok(()) => {
                if let Err(error) = self.lease.complete() {
                    tracing::error!(%error, "adapter restoration checkpoint failed");
                }
            }
            Err(error) => {
                self.lease.failed(&error);
                tracing::error!(%error, "adapter restoration remains pending");
            }
        }
    }
}
fn validate_adapter_restoration(snapshot: &AdapterSnapshot, actual: &AdapterValues) -> Result<()> {
    ensure!(
        actual.parameters.len() == snapshot.before.parameters.len()
            && actual.parameters.len() == snapshot.expected.parameters.len(),
        "adapter address families changed externally"
    );
    // Compare each property independently so partial setup/restoration is retryable.
    for ((actual, before), expected) in actual
        .parameters
        .iter()
        .zip(&snapshot.before.parameters)
        .zip(&snapshot.expected.parameters)
    {
        ensure!(
            (actual.mtu == before.mtu || actual.mtu == expected.mtu)
                && (actual.metric == before.metric || actual.metric == expected.metric)
                && (actual.automatic == before.automatic || actual.automatic == expected.automatic),
            "adapter parameters changed externally"
        );
    }
    ensure!(
        actual
            .addresses
            .iter()
            .all(|a| snapshot.before.addresses.contains(a)
                || snapshot.expected.addresses.contains(a)),
        "adapter addresses changed externally"
    );
    ensure!(
        snapshot
            .before
            .addresses
            .iter()
            .filter(|a| snapshot.expected.addresses.contains(a))
            .all(|a| actual.addresses.contains(a)),
        "unchanged adapter address removed externally"
    );
    Ok(())
}
pub(super) fn recover_adapter(snapshot: &AdapterSnapshot, _committed: bool) -> Result<()> {
    let guid = match adapter_guid(snapshot.index) {
        Ok(value) => value,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| matches!(e.raw_os_error(), Some(1168 | 2))) =>
        {
            return Ok(())
        }
        Err(error) => return Err(error),
    };
    ensure!(
        guid == snapshot.guid,
        "adapter identity changed; preserving it"
    );
    let actual = adapter_values(snapshot.index)?;
    if actual == snapshot.before {
        return Ok(());
    }
    validate_adapter_restoration(snapshot, &actual)?;
    for before in &snapshot.before.parameters {
        let mut row = MIB_IPINTERFACE_ROW {
            Family: before.family,
            InterfaceIndex: snapshot.index,
            ..Default::default()
        };
        check(unsafe { GetIpInterfaceEntry(&mut row) })?;
        row.NlMtu = before.mtu;
        row.Metric = before.metric;
        row.UseAutomaticMetric = before.automatic;
        row.SitePrefixLength = 0;
        check(unsafe { SetIpInterfaceEntry(&mut row) })?;
    }
    for current in actual
        .addresses
        .iter()
        .filter(|a| !snapshot.before.addresses.contains(a))
    {
        let mut row = MIB_UNICASTIPADDRESS_ROW::default();
        unsafe {
            InitializeUnicastIpAddressEntry(&mut row);
        }
        row.InterfaceIndex = snapshot.index;
        row.Address = sockaddr(current.address, snapshot.index);
        check(unsafe { DeleteUnicastIpAddressEntry(&row) })?;
    }
    for before in snapshot
        .before
        .addresses
        .iter()
        .filter(|a| !actual.addresses.contains(a))
    {
        let mut row = MIB_UNICASTIPADDRESS_ROW::default();
        unsafe {
            InitializeUnicastIpAddressEntry(&mut row);
        }
        row.InterfaceIndex = snapshot.index;
        row.Address = sockaddr(before.address, snapshot.index);
        row.OnLinkPrefixLength = before.prefix;
        row.PrefixOrigin = IpPrefixOriginManual;
        row.SuffixOrigin = IpSuffixOriginManual;
        row.DadState = IpDadStatePreferred;
        if let Some(properties) = &before.properties {
            row.ValidLifetime = properties.valid_lifetime;
            row.PreferredLifetime = properties.preferred_lifetime;
            row.PrefixOrigin = properties.prefix_origin;
            row.SuffixOrigin = properties.suffix_origin;
            row.SkipAsSource = properties.skip_as_source;
        }
        let result = unsafe { CreateUnicastIpAddressEntry(&row) };
        if result != 5010 {
            check(result)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod recovery_tests {
    use super::*;
    #[test]
    fn partial_adapter_setup_is_recoverable_but_external_changes_are_preserved() {
        let before = AdapterValues {
            parameters: vec![InterfaceParameters {
                family: AF_INET,
                mtu: 1500,
                metric: 25,
                automatic: true,
            }],
            addresses: vec![AdapterAddress {
                address: "192.0.2.1".parse().unwrap(),
                prefix: 24,
                properties: None,
            }],
        };
        let expected = AdapterValues {
            parameters: vec![InterfaceParameters {
                family: AF_INET,
                mtu: 1400,
                metric: 5,
                automatic: false,
            }],
            addresses: vec![AdapterAddress {
                address: "198.18.0.1".parse().unwrap(),
                prefix: 30,
                properties: None,
            }],
        };
        let snapshot = AdapterSnapshot {
            index: 1,
            guid: [0; 16],
            before: before.clone(),
            expected: expected.clone(),
        };
        let mut partial = before.clone();
        partial.parameters[0].mtu = 1400;
        validate_adapter_restoration(&snapshot, &partial).unwrap();
        partial.addresses.clear();
        validate_adapter_restoration(&snapshot, &partial).unwrap();
        validate_adapter_restoration(&snapshot, &expected).unwrap();
        partial.parameters[0].metric = 17;
        assert!(validate_adapter_restoration(&snapshot, &partial).is_err());
        partial = before.clone();
        partial.addresses.push(AdapterAddress {
            address: "203.0.113.9".parse().unwrap(),
            prefix: 24,
            properties: None,
        });
        assert!(validate_adapter_restoration(&snapshot, &partial).is_err());
    }
}
