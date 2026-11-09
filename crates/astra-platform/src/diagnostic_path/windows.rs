use super::*;
use std::{
    net::{Ipv4Addr, Ipv6Addr},
    os::windows::io::AsRawSocket,
};
use windows_sys::Win32::{
    NetworkManagement::{IpHelper::*, Ndis::*},
    Networking::WinSock::*,
};

fn checked(status: u32) -> io::Result<()> {
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(status as i32))
    }
}
pub(super) fn inspect(input: &str) -> io::Result<Interface> {
    let index = if let Ok(index) = input.parse::<u32>() {
        index
    } else {
        let alias: Vec<u16> = input.encode_utf16().chain([0]).collect();
        let mut luid = NET_LUID_LH::default();
        let mut index = 0;
        checked(unsafe { ConvertInterfaceAliasToLuid(alias.as_ptr(), &mut luid) })?;
        checked(unsafe { ConvertInterfaceLuidToIndex(&luid, &mut index) })?;
        index
    };
    let mut row = MIB_IF_ROW2 {
        InterfaceIndex: index,
        ..Default::default()
    };
    checked(unsafe { GetIfEntry2(&mut row) })?;
    if row.InterfaceAndOperStatusFlags._bitfield & 1 == 0
        || !matches!(row.Type, MIB_IF_TYPE_ETHERNET | IF_TYPE_IEEE80211)
        || row.OperStatus != IfOperStatusUp
        || row.AdminStatus != 1
        || row.TunnelType != 0
    {
        return Err(invalid(
            "Selected adapter is virtual, disconnected or not confirmed physical hardware",
        ));
    }
    let mut pointer = std::ptr::null_mut();
    checked(unsafe { GetUnicastIpAddressTable(AF_UNSPEC, &mut pointer) })?;
    if pointer.is_null() {
        return Err(invalid("Adapter address table unavailable"));
    }
    struct Table(*mut MIB_UNICASTIPADDRESS_TABLE);
    impl Drop for Table {
        fn drop(&mut self) {
            unsafe { FreeMibTable(self.0.cast()) };
        }
    }
    let table = Table(pointer);
    let count = unsafe { (*table.0).NumEntries } as usize;
    if count > 65536 {
        return Err(invalid("Adapter address table exceeds limit"));
    }
    let rows = unsafe { std::slice::from_raw_parts((*table.0).Table.as_ptr(), count) };
    let mut addresses: Vec<_> = rows
        .iter()
        .filter(|value| {
            value.InterfaceIndex == index
                && !value.SkipAsSource
                && value.DadState == IpDadStatePreferred
        })
        .filter_map(|value| sockaddr_ip(&value.Address))
        .collect();
    addresses.sort();
    addresses.dedup();
    let name = String::from_utf16_lossy(
        &row.Alias[..row
            .Alias
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(row.Alias.len())],
    );
    let fingerprint = format!(
        "{}:{:?}",
        unsafe { row.InterfaceLuid.Value },
        &row.PermanentPhysicalAddress[..(row.PhysicalAddressLength as usize).min(32)]
    );
    Ok(Interface {
        name,
        index,
        addresses,
        local_addresses: rows
            .iter()
            .filter_map(|value| sockaddr_ip(&value.Address))
            .collect(),
        fingerprint,
    })
}
pub(super) fn sockaddr_ip(address: &SOCKADDR_INET) -> Option<IpAddr> {
    unsafe {
        match address.si_family {
            AF_INET => Some(IpAddr::V4(Ipv4Addr::from(
                address.Ipv4.sin_addr.S_un.S_addr.to_ne_bytes(),
            ))),
            AF_INET6 => Some(IpAddr::V6(Ipv6Addr::from(address.Ipv6.sin6_addr.u.Byte))),
            _ => None,
        }
    }
}
pub(super) fn sockaddr(address: SocketAddr) -> SOCKADDR_INET {
    match address {
        SocketAddr::V4(address) => SOCKADDR_INET {
            Ipv4: SOCKADDR_IN {
                sin_family: AF_INET,
                sin_port: address.port().to_be(),
                sin_addr: IN_ADDR {
                    S_un: IN_ADDR_0 {
                        S_addr: u32::from_ne_bytes(address.ip().octets()),
                    },
                },
                ..Default::default()
            },
        },
        SocketAddr::V6(address) => SOCKADDR_INET {
            Ipv6: SOCKADDR_IN6 {
                sin6_family: AF_INET6,
                sin6_port: address.port().to_be(),
                sin6_addr: IN6_ADDR {
                    u: IN6_ADDR_0 {
                        Byte: address.ip().octets(),
                    },
                },
                Anonymous: SOCKADDR_IN6_0 {
                    sin6_scope_id: address.scope_id(),
                },
                ..Default::default()
            },
        },
    }
}
fn option(ipv4: bool, index: u32) -> (i32, i32, u32) {
    if ipv4 {
        (IPPROTO_IP, IP_UNICAST_IF, index.to_be())
    } else {
        (IPPROTO_IPV6, IPV6_UNICAST_IF, index)
    }
}
pub(super) fn bind(socket: &Socket, interface: &Interface, ipv4: bool) -> io::Result<()> {
    let (level, option, value) = option(ipv4, interface.index);
    if unsafe {
        setsockopt(
            socket.as_raw_socket() as _,
            level,
            option,
            (&value as *const u32).cast(),
            4,
        )
    } != 0
    {
        return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
    }
    Ok(())
}
pub(super) fn verify_binding(socket: &Socket, interface: &Interface, ipv4: bool) -> io::Result<()> {
    // IP_UNICAST_IF is asymmetric: setsockopt takes network byte order,
    // getsockopt returns a host-order index. IPV6_UNICAST_IF is host-order both ways.
    // https://learn.microsoft.com/en-us/windows/win32/winsock/ipproto-ip-socket-options
    let (level, option, _) = option(ipv4, interface.index);
    let expected = interface.index;
    let mut value = 0u32;
    let mut size = 4;
    if unsafe {
        getsockopt(
            socket.as_raw_socket() as _,
            level,
            option,
            (&mut value as *mut u32).cast(),
            &mut size,
        )
    } != 0
    {
        return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
    }
    if size != 4 || value != expected {
        return Err(invalid(
            "Adapter binding readback differs from selected physical interface",
        ));
    }
    Ok(())
}
pub(super) fn route(guard: &PhysicalGuard, target: SocketAddr) -> io::Result<()> {
    route_at(guard, target, guard.index())
}
pub(super) fn icmp_route(guard: &PhysicalGuard, target: SocketAddr) -> io::Result<()> {
    // ICMP helper handles expose a source address, not IP_UNICAST_IF. Refuse
    // when the ordinary route from that exact source would use another adapter.
    route_at(guard, target, 0)
}
fn route_at(guard: &PhysicalGuard, target: SocketAddr, index: u32) -> io::Result<()> {
    let source = sockaddr(guard.destination(SocketAddr::new(guard.source, 0))?);
    let destination = sockaddr(target);
    let mut row = MIB_IPFORWARD_ROW2::default();
    let mut actual = SOCKADDR_INET::default();
    checked(unsafe {
        GetBestRoute2(
            std::ptr::null(),
            index,
            &source,
            &destination,
            0,
            &mut row,
            &mut actual,
        )
    })?;
    if row.InterfaceIndex != guard.index()
        || row.Loopback
        || sockaddr_ip(&actual) != Some(guard.source)
    {
        return Err(invalid(
            "Kernel route does not preserve the selected physical interface/source",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_interface_options_round_trip_with_each_apis_byte_order() {
        let mut pointer = std::ptr::null_mut();
        checked(unsafe { GetIfTable2(&mut pointer) }).unwrap();
        assert!(!pointer.is_null());
        struct Table(*mut MIB_IF_TABLE2);
        impl Drop for Table {
            fn drop(&mut self) {
                unsafe { FreeMibTable(self.0.cast()) };
            }
        }
        let table = Table(pointer);
        let count = unsafe { (*table.0).NumEntries } as usize;
        assert!(count < 65536);
        let rows = unsafe { std::slice::from_raw_parts((*table.0).Table.as_ptr(), count) };
        for ipv4 in [true, false] {
            let mut verified = false;
            for row in rows
                .iter()
                .filter(|row| row.InterfaceIndex != 0 && row.OperStatus == IfOperStatusUp)
                .take(128)
            {
                let interface = Interface {
                    name: "test".into(),
                    index: row.InterfaceIndex,
                    addresses: vec![],
                    local_addresses: vec![],
                    fingerprint: "test".into(),
                };
                let socket = Socket::new(
                    if ipv4 { Domain::IPV4 } else { Domain::IPV6 },
                    Type::DGRAM,
                    Some(Protocol::UDP),
                )
                .unwrap();
                if let Err(error) = bind(&socket, &interface, ipv4) {
                    eprintln!(
                        "Interface option unavailable for index {} ipv4={ipv4}: {error}",
                        interface.index
                    );
                    continue;
                }
                verify_binding(&socket, &interface, ipv4).unwrap_or_else(|error| {
                    panic!("index {} ipv4={ipv4}: {error}", interface.index)
                });
                verified = true;
                break;
            }
            assert!(
                verified,
                "No available interface accepted native binding for ipv4={ipv4}"
            );
        }
    }
}
