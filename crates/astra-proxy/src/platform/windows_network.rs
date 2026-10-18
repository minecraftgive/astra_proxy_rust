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
    row: MIB_IPFORWARD_ROW2,
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
    let mut pointer = std::ptr::null_mut();
    check(unsafe { GetIpForwardTable2(AF_UNSPEC, &mut pointer) })
        .context("read Windows route table")?;
    let table = RouteTable(pointer);
    let rows = unsafe {
        std::slice::from_raw_parts((*table.0).Table.as_ptr(), (*table.0).NumEntries as usize)
    };
    for row in rows {
        if row.DestinationPrefix.PrefixLength != network.prefix_len() {
            continue;
        }
        let address = unsafe {
            let prefix = row.DestinationPrefix.Prefix;
            match prefix.si_family {
                AF_INET => IpAddr::V4(std::net::Ipv4Addr::from(
                    prefix.Ipv4.sin_addr.S_un.S_addr.to_ne_bytes(),
                )),
                AF_INET6 => IpAddr::V6(std::net::Ipv6Addr::from(prefix.Ipv6.sin6_addr.u.Byte)),
                _ => continue,
            }
        };
        ensure!(address != network.network(), "Windows route prefix {} already exists on interface {}; refusing to compete with an existing route", network.trunc(), row.InterfaceIndex);
    }
    Ok(())
}
impl Route {
    pub fn create(cidr: &str, interface: &str, gateway: Option<IpAddr>) -> Result<Self> {
        let network: ipnet::IpNet = cidr.parse()?;
        let index = interface_index(interface)?;
        reject_existing_prefix(network)?;
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
        check(unsafe { CreateIpForwardEntry2(&row) }).with_context(|| format!("add Windows route {cidr} via interface {index} (requires administrator; existing routes are never overwritten)"))?;
        Ok(Self { row })
    }
}
impl Drop for Route {
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
