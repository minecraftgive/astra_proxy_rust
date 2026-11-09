//! Correlate only this private UDP socket's kernel error queue; never sniff traffic.
use super::*;
use std::{
    mem::{size_of, size_of_val, zeroed},
    os::fd::AsRawFd,
};

fn errors(socket: &Socket, ipv4: bool) -> io::Result<()> {
    let enabled: libc::c_int = 1;
    let (level, option) = if ipv4 {
        (libc::IPPROTO_IP, libc::IP_RECVERR)
    } else {
        (libc::IPPROTO_IPV6, libc::IPV6_RECVERR)
    };
    if unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            level,
            option,
            (&enabled as *const libc::c_int).cast(),
            size_of::<libc::c_int>() as _,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
pub(super) fn available(ipv6: bool) -> io::Result<()> {
    let socket = Socket::new(
        if ipv6 { Domain::IPV6 } else { Domain::IPV4 },
        Type::DGRAM,
        Some(Protocol::UDP),
    )?;
    errors(&socket, !ipv6)
}
pub(super) fn probe(
    guard: &PhysicalGuard,
    target: IpAddr,
    hop: u8,
    seq: u16,
    timeout: Duration,
) -> io::Result<Reply> {
    let socket = guard.udp_socket()?;
    let target = guard.destination(SocketAddr::new(target, 33434 + seq % 1000))?;
    probe_socket(socket, target, hop, timeout)
}
fn probe_socket(
    socket: Socket,
    target: SocketAddr,
    hop: u8,
    timeout: Duration,
) -> io::Result<Reply> {
    errors(&socket, target.is_ipv4())?;
    if target.is_ipv4() {
        socket.set_ttl_v4(u32::from(hop))?;
    } else {
        socket.set_unicast_hops_v6(u32::from(hop))?;
    }
    socket.connect(&SockAddr::from(target))?;
    socket.set_nonblocking(true)?;
    let token = uuid::Uuid::new_v4();
    let start = Instant::now();
    socket.send(token.as_bytes())?;
    loop {
        let remaining = timeout.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            return Ok(Reply::silence());
        }
        let mut descriptor = libc::pollfd {
            fd: socket.as_raw_fd(),
            events: libc::POLLERR | libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe {
            libc::poll(
                &mut descriptor,
                1,
                remaining.as_millis().clamp(1, i32::MAX as u128) as i32,
            )
        };
        if ready == 0 {
            return Ok(Reply::silence());
        }
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        let mut payload = [0u8; 256];
        let mut address: libc::sockaddr_storage = unsafe { zeroed() };
        let mut ancillary = [0u64; 64]; // cmsghdr alignment, bounded buffer.
        let mut vector = libc::iovec {
            iov_base: payload.as_mut_ptr().cast(),
            iov_len: payload.len(),
        };
        let mut message: libc::msghdr = unsafe { zeroed() };
        message.msg_name = (&mut address as *mut libc::sockaddr_storage).cast();
        message.msg_namelen = size_of::<libc::sockaddr_storage>() as _;
        message.msg_iov = &mut vector;
        message.msg_iovlen = 1;
        message.msg_control = ancillary.as_mut_ptr().cast();
        message.msg_controllen = size_of_val(&ancillary);
        let size = unsafe {
            libc::recvmsg(
                socket.as_raw_fd(),
                &mut message,
                libc::MSG_ERRQUEUE | libc::MSG_DONTWAIT,
            )
        };
        if size < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::WouldBlock {
                return Err(error);
            }
            // A normal UDP answer is also destination reachability, but only if
            // its full nonce and connected peer match this one probe.
            let mut data = [std::mem::MaybeUninit::<u8>::uninit(); 64];
            match socket.recv(&mut data) {
                Ok(length)
                    if length == 16
                        && unsafe {
                            std::slice::from_raw_parts(data.as_ptr().cast::<u8>(), 16)
                        } == token.as_bytes() =>
                {
                    return Ok(Reply {
                        from: Some(target.ip()),
                        rtt_ms: Some(elapsed(start)),
                        ttl: None,
                        outcome: PathOutcome::Reply,
                        icmp_type: None,
                        icmp_code: None,
                    })
                }
                Err(error) if error.kind() != io::ErrorKind::WouldBlock => return Err(error),
                _ => continue,
            }
        }
        if message.msg_flags & libc::MSG_CTRUNC != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Truncated ICMP error metadata",
            ));
        }
        let minimum = match address.ss_family as i32 {
            libc::AF_INET => size_of::<libc::sockaddr_in>(),
            libc::AF_INET6 => size_of::<libc::sockaddr_in6>(),
            _ => continue,
        };
        if (message.msg_namelen as usize) < minimum
            || (message.msg_namelen as usize) > size_of::<libc::sockaddr_storage>()
        {
            continue;
        }
        let mut storage = socket2::SockAddrStorage::zeroed();
        *unsafe { storage.view_as::<libc::sockaddr_storage>() } = address;
        let original = unsafe { SockAddr::new(storage, message.msg_namelen) }.as_socket();
        if original != Some(target) {
            continue;
        }
        let length = (size as usize).min(16);
        if payload[..length] != token.as_bytes()[..length] {
            continue;
        }
        let mut header = unsafe { libc::CMSG_FIRSTHDR(&message) };
        while !header.is_null() {
            let value = unsafe { &*header };
            let matches = (target.is_ipv4()
                && value.cmsg_level == libc::IPPROTO_IP
                && value.cmsg_type == libc::IP_RECVERR)
                || (target.is_ipv6()
                    && value.cmsg_level == libc::IPPROTO_IPV6
                    && value.cmsg_type == libc::IPV6_RECVERR);
            if matches
                && value.cmsg_len
                    >= unsafe { libc::CMSG_LEN(size_of::<libc::sock_extended_err>() as u32) }
                        as usize
            {
                let data = unsafe { libc::CMSG_DATA(header) };
                let error =
                    unsafe { std::ptr::read_unaligned(data.cast::<libc::sock_extended_err>()) };
                if error.ee_origin == 1 {
                    return Err(io::Error::from_raw_os_error(error.ee_errno as i32));
                }
                if (target.is_ipv4() && error.ee_origin != 2)
                    || (target.is_ipv6() && error.ee_origin != 3)
                {
                    break;
                }
                let bytes = value.cmsg_len - unsafe { libc::CMSG_LEN(0) } as usize;
                let offender = offender(unsafe {
                    std::slice::from_raw_parts(
                        data.add(size_of::<libc::sock_extended_err>()),
                        bytes.saturating_sub(size_of::<libc::sock_extended_err>()),
                    )
                });
                if let Some(outcome) = super::outcome(
                    target.is_ipv4(),
                    error.ee_type,
                    error.ee_code,
                    offender,
                    target.ip(),
                ) {
                    return Ok(Reply {
                        from: offender,
                        rtt_ms: Some(elapsed(start)),
                        ttl: None,
                        outcome,
                        icmp_type: Some(error.ee_type),
                        icmp_code: Some(error.ee_code),
                    });
                }
            }
            header = unsafe { libc::CMSG_NXTHDR(&message, header) };
        }
    }
}
fn offender(bytes: &[u8]) -> Option<IpAddr> {
    if bytes.len() < 2 {
        return None;
    }
    match u16::from_ne_bytes([bytes[0], bytes[1]]) as i32 {
        libc::AF_INET if bytes.len() >= size_of::<libc::sockaddr_in>() => Some(IpAddr::V4(
            std::net::Ipv4Addr::new(bytes[4], bytes[5], bytes[6], bytes[7]),
        )),
        libc::AF_INET6 if bytes.len() >= size_of::<libc::sockaddr_in6>() => Some(IpAddr::V6(
            std::net::Ipv6Addr::from(<[u8; 16]>::try_from(&bytes[8..24]).ok()?),
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_closed_udp_port_is_correlated_from_the_error_queue() {
        for literal in ["127.0.0.1:0", "[::1]:0"] {
            let reserved = match std::net::UdpSocket::bind(literal) {
                Ok(socket) => socket,
                Err(error) if error.kind() == io::ErrorKind::AddrNotAvailable => continue,
                Err(error) => panic!("{error}"),
            };
            let target = reserved.local_addr().unwrap();
            drop(reserved);
            let socket = Socket::new(
                if target.is_ipv4() {
                    Domain::IPV4
                } else {
                    Domain::IPV6
                },
                Type::DGRAM,
                Some(Protocol::UDP),
            )
            .unwrap();
            socket
                .bind(&SockAddr::from(literal.parse::<SocketAddr>().unwrap()))
                .unwrap();
            let result = probe_socket(socket, target, 1, Duration::from_secs(1)).unwrap();
            assert_eq!(result.outcome, PathOutcome::Reply);
            assert_eq!(result.from, Some(target.ip()));
        }
    }
}
