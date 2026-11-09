use super::*;
use std::{
    ffi::{CStr, CString},
    net::{Ipv4Addr, Ipv6Addr},
    os::fd::AsRawFd,
};

pub(super) fn inspect(input: &str) -> io::Result<Interface> {
    let name = if let Ok(index) = input.parse::<u32>() {
        let mut name = [0 as libc::c_char; libc::IF_NAMESIZE];
        if unsafe { libc::if_indextoname(index, name.as_mut_ptr()) }.is_null() {
            return Err(io::Error::last_os_error());
        }
        unsafe { CStr::from_ptr(name.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    } else {
        input.to_owned()
    };
    let c_name = CString::new(name.as_bytes()).map_err(|_| invalid("Invalid interface name"))?;
    let index = unsafe { libc::if_nametoindex(c_name.as_ptr()) };
    if index == 0 {
        return Err(invalid("Selected interface is unavailable"));
    }
    let fingerprint = physical(&name)?;
    let mut head = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return Err(io::Error::last_os_error());
    }
    struct List(*mut libc::ifaddrs);
    impl Drop for List {
        fn drop(&mut self) {
            unsafe { libc::freeifaddrs(self.0) };
        }
    }
    let list = List(head);
    let mut row = list.0;
    let mut addresses = vec![];
    let mut local_addresses = vec![];
    while !row.is_null() {
        let value = unsafe { &*row };
        if !value.ifa_addr.is_null() {
            let family = unsafe { (*value.ifa_addr).sa_family as i32 };
            if family == libc::AF_INET {
                let address = unsafe { &*value.ifa_addr.cast::<libc::sockaddr_in>() };
                local_addresses.push(IpAddr::V4(Ipv4Addr::from(
                    address.sin_addr.s_addr.to_ne_bytes(),
                )));
            } else if family == libc::AF_INET6 {
                let address = unsafe { &*value.ifa_addr.cast::<libc::sockaddr_in6>() };
                local_addresses.push(IpAddr::V6(Ipv6Addr::from(address.sin6_addr.s6_addr)));
            }
        }
        if !value.ifa_name.is_null()
            && unsafe { CStr::from_ptr(value.ifa_name) }.to_bytes() == name.as_bytes()
        {
            let flags = value.ifa_flags as i32;
            if flags & libc::IFF_LOOPBACK != 0
                || flags & libc::IFF_UP == 0
                || flags & libc::IFF_RUNNING == 0
            {
                return Err(invalid("Selected interface is loopback or disconnected"));
            }
            if !value.ifa_addr.is_null() {
                let family = unsafe { (*value.ifa_addr).sa_family as i32 };
                if family == libc::AF_INET {
                    let address = unsafe { &*value.ifa_addr.cast::<libc::sockaddr_in>() };
                    addresses.push(IpAddr::V4(Ipv4Addr::from(
                        address.sin_addr.s_addr.to_ne_bytes(),
                    )));
                } else if family == libc::AF_INET6 {
                    let address = unsafe { &*value.ifa_addr.cast::<libc::sockaddr_in6>() };
                    addresses.push(IpAddr::V6(Ipv6Addr::from(address.sin6_addr.s6_addr)));
                }
            }
        }
        row = value.ifa_next;
    }
    addresses.sort();
    addresses.dedup();
    if addresses.is_empty() {
        return Err(invalid("Selected interface has no current addresses"));
    }
    Ok(Interface {
        name,
        index,
        addresses,
        local_addresses,
        fingerprint,
    })
}

#[cfg(target_os = "linux")]
fn physical(name: &str) -> io::Result<String> {
    let root = std::path::Path::new("/sys/class/net").join(name);
    let device = std::fs::canonicalize(root.join("device")).map_err(|_| {
        invalid("Interface is virtual or its physical hardware cannot be confirmed")
    })?;
    if std::fs::read_to_string(root.join("type"))?.trim() != "1" {
        return Err(invalid(
            "Interface is not a confirmed physical Ethernet/Wi-Fi device",
        ));
    }
    Ok(format!(
        "{}:{}",
        device.display(),
        std::fs::read_to_string(root.join("address"))?.trim()
    ))
}

#[cfg(target_os = "macos")]
fn physical(name: &str) -> io::Result<String> {
    use std::ffi::c_void;
    type Ref = *const c_void;
    #[link(name = "SystemConfiguration", kind = "framework")]
    extern "C" {
        fn SCNetworkInterfaceCopyAll() -> Ref;
        fn SCNetworkInterfaceGetBSDName(interface: Ref) -> Ref;
        fn SCNetworkInterfaceGetInterfaceType(interface: Ref) -> Ref;
        fn SCNetworkInterfaceGetHardwareAddressString(interface: Ref) -> Ref;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFArrayGetCount(array: Ref) -> isize;
        fn CFArrayGetValueAtIndex(array: Ref, index: isize) -> Ref;
        fn CFStringGetCString(string: Ref, buffer: *mut i8, size: isize, encoding: u32) -> bool;
        fn CFRelease(value: Ref);
    }
    fn string(value: Ref) -> Option<String> {
        if value.is_null() {
            return None;
        }
        let mut buffer = [0i8; 1024];
        if !unsafe {
            CFStringGetCString(
                value,
                buffer.as_mut_ptr(),
                buffer.len() as isize,
                0x08000100,
            )
        } {
            return None;
        }
        Some(
            unsafe { CStr::from_ptr(buffer.as_ptr()) }
                .to_string_lossy()
                .into_owned(),
        )
    }
    struct Array(Ref);
    impl Drop for Array {
        fn drop(&mut self) {
            unsafe { CFRelease(self.0) };
        }
    }
    let raw = unsafe { SCNetworkInterfaceCopyAll() };
    if raw.is_null() {
        return Err(invalid(
            "OS physical interface classification is unavailable",
        ));
    }
    let array = Array(raw);
    let count = unsafe { CFArrayGetCount(array.0) };
    if !(0..=1024).contains(&count) {
        return Err(invalid("OS interface classification exceeded its bound"));
    }
    for index in 0..count {
        let interface = unsafe { CFArrayGetValueAtIndex(array.0, index) };
        if string(unsafe { SCNetworkInterfaceGetBSDName(interface) }).as_deref() != Some(name) {
            continue;
        }
        let kind =
            string(unsafe { SCNetworkInterfaceGetInterfaceType(interface) }).unwrap_or_default();
        if !matches!(kind.as_str(), "Ethernet" | "IEEE80211") {
            return Err(invalid(
                "Selected interface is not a physical Ethernet/Wi-Fi port",
            ));
        }
        let mac = string(unsafe { SCNetworkInterfaceGetHardwareAddressString(interface) })
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid("Physical port hardware identity is unavailable"))?;
        return Ok(format!("{kind}:{mac}"));
    }
    Err(invalid(
        "Interface is virtual or its physical hardware cannot be confirmed",
    ))
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn physical(_: &str) -> io::Result<String> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Physical diagnostics are unsupported on this OS",
    ))
}

pub(super) fn bind(socket: &Socket, interface: &Interface, ipv4: bool) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let _ = ipv4;
        let name =
            CString::new(interface.name.as_bytes()).map_err(|_| invalid("Invalid interface"))?;
        if unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_BINDTODEVICE,
                name.as_ptr().cast(),
                name.as_bytes_with_nul().len() as _,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    #[cfg(target_os = "macos")]
    {
        let (level, option) = if ipv4 {
            (libc::IPPROTO_IP, libc::IP_BOUND_IF)
        } else {
            (libc::IPPROTO_IPV6, libc::IPV6_BOUND_IF)
        };
        if unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                level,
                option,
                (&interface.index as *const u32).cast(),
                4,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Device binding is not implemented",
    ));
    #[allow(unreachable_code)]
    Ok(())
}

pub(super) fn verify_binding(socket: &Socket, interface: &Interface, ipv4: bool) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let _ = ipv4;
        let mut value = [0u8; libc::IF_NAMESIZE + 1];
        let mut length = value.len() as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_BINDTODEVICE,
                value.as_mut_ptr().cast(),
                &mut length,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        if value[..(length as usize).min(value.len())]
            .split(|b| *b == 0)
            .next()
            != Some(interface.name.as_bytes())
        {
            return Err(invalid(
                "Device binding readback differs from selected physical interface",
            ));
        }
    }
    #[cfg(target_os = "macos")]
    {
        let (level, option) = if ipv4 {
            (libc::IPPROTO_IP, libc::IP_BOUND_IF)
        } else {
            (libc::IPPROTO_IPV6, libc::IPV6_BOUND_IF)
        };
        let mut index = 0u32;
        let mut length = 4;
        if unsafe {
            libc::getsockopt(
                socket.as_raw_fd(),
                level,
                option,
                (&mut index as *mut u32).cast(),
                &mut length,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        if length != 4 || index != interface.index {
            return Err(invalid(
                "Device binding readback differs from selected physical interface",
            ));
        }
    }
    Ok(())
}

pub(super) fn route(_: &PhysicalGuard, _: SocketAddr) -> io::Result<()> {
    // On Unix the source+device constrained UDP connect in recheck performs the
    // kernel route lookup. Unlike an unbound route command it cannot pick TUN.
    Ok(())
}
