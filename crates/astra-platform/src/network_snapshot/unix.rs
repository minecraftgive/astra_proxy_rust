use super::*;
use std::{
    collections::BTreeMap,
    ffi::CStr,
    net::{Ipv4Addr, Ipv6Addr},
};

pub(super) fn interfaces() -> io::Result<Vec<Interface>> {
    let mut pointer = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut pointer) } != 0 {
        return Err(io::Error::last_os_error());
    }
    struct Addresses(*mut libc::ifaddrs);
    impl Drop for Addresses {
        fn drop(&mut self) {
            unsafe { libc::freeifaddrs(self.0) }
        }
    }
    let owner = Addresses(pointer);
    let mut current = owner.0;
    let mut result = BTreeMap::<String, Interface>::new();
    while !current.is_null() {
        let row = unsafe { &*current };
        if !row.ifa_name.is_null() {
            let name = unsafe { CStr::from_ptr(row.ifa_name) }
                .to_string_lossy()
                .into_owned();
            let flags = row.ifa_flags as i32;
            let item = result.entry(name.clone()).or_insert_with(|| Interface {
                id: name.clone(),
                name,
                index: unsafe { libc::if_nametoindex(row.ifa_name) },
                kind: if flags & libc::IFF_LOOPBACK != 0 {
                    InterfaceKind::Loopback
                } else {
                    InterfaceKind::Unknown
                },
                connected: flags & libc::IFF_UP != 0 && flags & libc::IFF_RUNNING != 0,
                addresses: vec![],
                source: "getifaddrs".into(),
            });
            if let Some(address) = unsafe { address(row.ifa_addr, row.ifa_netmask) } {
                if !item.addresses.contains(&address) {
                    item.addresses.push(address);
                }
            }
        }
        current = row.ifa_next;
    }
    Ok(result.into_values().collect())
}
unsafe fn address(pointer: *const libc::sockaddr, mask: *const libc::sockaddr) -> Option<Address> {
    if pointer.is_null() {
        return None;
    }
    // getifaddrs supplies appropriately sized sockaddr storage for its family.
    let family = unsafe { (*pointer).sa_family as i32 };
    if family == libc::AF_INET {
        let addr = unsafe { &*pointer.cast::<libc::sockaddr_in>() };
        let prefix = if mask.is_null() {
            None
        } else {
            Some(unsafe { (*mask.cast::<libc::sockaddr_in>()).sin_addr.s_addr }.count_ones() as u8)
        };
        Some(Address {
            address: Ipv4Addr::from(addr.sin_addr.s_addr.to_ne_bytes()).to_string(),
            prefix_len: prefix,
        })
    } else if family == libc::AF_INET6 {
        let addr = unsafe { &*pointer.cast::<libc::sockaddr_in6>() };
        let ip = Ipv6Addr::from(addr.sin6_addr.s6_addr);
        let text = if addr.sin6_scope_id != 0 {
            format!("{ip}%{}", addr.sin6_scope_id)
        } else {
            ip.to_string()
        };
        let prefix = if mask.is_null() {
            None
        } else {
            Some(
                unsafe { (*mask.cast::<libc::sockaddr_in6>()).sin6_addr.s6_addr }
                    .iter()
                    .map(|b| b.count_ones())
                    .sum::<u32>() as u8,
            )
        };
        Some(Address {
            address: text,
            prefix_len: prefix,
        })
    } else {
        None
    }
}
