use super::*;
use windows_sys::Win32::{
    Foundation::{GetLastError, HANDLE, INVALID_HANDLE_VALUE},
    NetworkManagement::IpHelper::*,
};

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { IcmpCloseHandle(self.0) };
    }
}
fn handle(ipv6: bool) -> io::Result<Handle> {
    let handle = unsafe {
        if ipv6 {
            Icmp6CreateFile()
        } else {
            IcmpCreateFile()
        }
    };
    if handle == INVALID_HANDLE_VALUE || handle.is_null() {
        return Err(io::Error::from_raw_os_error(
            unsafe { GetLastError() } as i32
        ));
    }
    Ok(Handle(handle))
}
pub(super) fn available(ipv6: bool) -> io::Result<()> {
    handle(ipv6).map(drop)
}
fn status(code: u32) -> io::Result<PathOutcome> {
    match code {
        0 => Ok(PathOutcome::Reply),
        11010 => Ok(PathOutcome::NoResponse),
        11013 => Ok(PathOutcome::TimeExceeded),
        11002..=11005 | 11040 => Ok(PathOutcome::Unreachable),
        _ => Err(io::Error::from_raw_os_error(code as i32)),
    }
}
pub(super) fn probe(
    guard: Option<&PhysicalGuard>,
    target: IpAddr,
    hop: u8,
    _seq: u16,
    timeout: Duration,
) -> io::Result<Reply> {
    let handle = handle(target.is_ipv6())?;
    if let Some(guard) = guard {
        super::super::windows::icmp_route(guard, guard.destination(SocketAddr::new(target, 0))?)?;
    }
    let options = IP_OPTION_INFORMATION {
        Ttl: hop,
        ..Default::default()
    };
    let data = uuid::Uuid::new_v4();
    let mut buffer = [0u64; 256];
    let timeout = timeout.as_millis().clamp(1, u32::MAX as u128) as u32;
    let (count, from, rtt, ttl, code) = match target {
        IpAddr::V4(ip) => {
            let source = match guard.map(PhysicalGuard::local_ip) {
                Some(IpAddr::V4(ip)) => u32::from_ne_bytes(ip.octets()),
                Some(_) => return Err(invalid("ICMP source family mismatch")),
                None => 0,
            };
            let count = unsafe {
                IcmpSendEcho2Ex(
                    handle.0,
                    std::ptr::null_mut(),
                    None,
                    std::ptr::null(),
                    source,
                    u32::from_ne_bytes(ip.octets()),
                    data.as_bytes().as_ptr().cast(),
                    16,
                    &options,
                    buffer.as_mut_ptr().cast(),
                    std::mem::size_of_val(&buffer) as u32,
                    timeout,
                )
            };
            let reply: ICMP_ECHO_REPLY =
                unsafe { std::ptr::read_unaligned(buffer.as_ptr().cast()) };
            (
                count,
                Some(IpAddr::V4(std::net::Ipv4Addr::from(
                    reply.Address.to_ne_bytes(),
                ))),
                reply.RoundTripTime as f64,
                Some(reply.Options.Ttl),
                reply.Status,
            )
        }
        IpAddr::V6(ip) => {
            let source = match guard {
                Some(guard) => guard.destination(SocketAddr::new(guard.local_ip(), 0))?,
                None => "[::]:0".parse().unwrap(),
            };
            let destination = match guard {
                Some(guard) => guard.destination(SocketAddr::new(IpAddr::V6(ip), 0))?,
                None => SocketAddr::new(IpAddr::V6(ip), 0),
            };
            let source = super::super::windows::sockaddr(source);
            let destination = super::super::windows::sockaddr(destination);
            let count = unsafe {
                Icmp6SendEcho2(
                    handle.0,
                    std::ptr::null_mut(),
                    None,
                    std::ptr::null(),
                    &source.Ipv6,
                    &destination.Ipv6,
                    data.as_bytes().as_ptr().cast(),
                    16,
                    &options,
                    buffer.as_mut_ptr().cast(),
                    std::mem::size_of_val(&buffer) as u32,
                    timeout,
                )
            };
            let reply: ICMPV6_ECHO_REPLY_LH =
                unsafe { std::ptr::read_unaligned(buffer.as_ptr().cast()) };
            let words = reply.Address.sin6_addr;
            let mut octets = [0u8; 16];
            for (index, value) in words.iter().enumerate() {
                octets[index * 2..index * 2 + 2].copy_from_slice(&value.to_ne_bytes());
            }
            (
                count,
                Some(IpAddr::V6(std::net::Ipv6Addr::from(octets))),
                reply.RoundTripTime as f64,
                None,
                reply.Status,
            )
        }
    };
    if let Some(guard) = guard {
        super::super::windows::icmp_route(guard, guard.destination(SocketAddr::new(target, 0))?)?;
    }
    if count == 0 {
        let outcome = status(unsafe { GetLastError() })?;
        return Ok(Reply {
            outcome,
            ..Reply::silence()
        });
    }
    let outcome = status(code)?;
    if outcome == PathOutcome::Reply && from != Some(target) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "ICMP reply did not match the requested destination",
        ));
    }
    Ok(Reply {
        from: from.filter(|ip| !ip.is_unspecified()),
        rtt_ms: (outcome != PathOutcome::NoResponse).then_some(rtt),
        ttl,
        outcome,
        icmp_type: None,
        icmp_code: None,
    })
}
