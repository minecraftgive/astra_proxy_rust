use super::*;
use std::mem::MaybeUninit;

pub(super) fn available(ipv6: bool) -> io::Result<()> {
    Socket::new(
        if ipv6 { Domain::IPV6 } else { Domain::IPV4 },
        Type::RAW,
        Some(if ipv6 {
            Protocol::ICMPV6
        } else {
            Protocol::ICMPV4
        }),
    )
    .map(drop)
}
pub(super) fn probe(
    guard: &PhysicalGuard,
    target: IpAddr,
    hop: u8,
    seq: u16,
    timeout: Duration,
) -> io::Result<Reply> {
    let v4 = target.is_ipv4();
    // Open first: a non-root service must fail without sending a UDP packet.
    let receive = Socket::new(
        if v4 { Domain::IPV4 } else { Domain::IPV6 },
        Type::RAW,
        Some(if v4 {
            Protocol::ICMPV4
        } else {
            Protocol::ICMPV6
        }),
    )?;
    guard.bind_socket(&receive)?;
    let send = guard.udp_socket()?;
    if v4 {
        send.set_ttl_v4(u32::from(hop))?;
    } else {
        send.set_unicast_hops_v6(u32::from(hop))?;
    }
    let destination = guard.destination(SocketAddr::new(target, 33434 + seq % 1000))?;
    let source = send
        .local_addr()?
        .as_socket()
        .ok_or_else(|| io::Error::other("Missing bound UDP address"))?;
    let token = uuid::Uuid::new_v4();
    let start = Instant::now();
    send.send_to(token.as_bytes(), &SockAddr::from(destination))?;
    let mut bytes = [MaybeUninit::<u8>::uninit(); 4096];
    loop {
        let remaining = timeout.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            return Ok(Reply::silence());
        }
        receive.set_read_timeout(Some(remaining))?;
        let (size, offender) = match receive.recv_from(&mut bytes) {
            Ok(value) => value,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                return Ok(Reply::silence())
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        let data = unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast::<u8>(), size) };
        let offender = offender.as_socket().map(|address| address.ip());
        let Some((kind, code)) = matching_error(data, source, destination, token.as_bytes()) else {
            continue;
        };
        if let Some(outcome) = super::outcome(v4, kind, code, offender, target) {
            return Ok(Reply {
                from: offender,
                rtt_ms: Some(elapsed(start)),
                ttl: None,
                outcome,
                icmp_type: Some(kind),
                icmp_code: Some(code),
            });
        }
    }
}

/// Accept only an ICMP error quoting this probe's exact IP/UDP tuple and, when
/// supplied by the router, its nonce. Ordinary echo replies are not trace proof.
fn matching_error(
    packet: &[u8],
    source: SocketAddr,
    destination: SocketAddr,
    token: &[u8],
) -> Option<(u8, u8)> {
    let body = if packet.first()? >> 4 == 4 {
        let size = usize::from(packet[0] & 15) * 4;
        if size < 20 || *packet.get(9)? != 1 {
            return None;
        }
        packet.get(size..)?
    } else if packet.first()? >> 4 == 6 {
        if *packet.get(6)? != 58 {
            return None;
        }
        packet.get(40..)?
    } else {
        packet
    };
    let kind = *body.first()?;
    let code = *body.get(1)?;
    if !matches!((source.is_ipv4(), kind), (true, 3 | 11) | (false, 1 | 3)) {
        return None;
    }
    let quoted = body.get(8..)?;
    let header = match (source.ip(), destination.ip(), quoted.first()? >> 4) {
        (IpAddr::V4(src), IpAddr::V4(dst), 4) => {
            let size = usize::from(quoted[0] & 15) * 4;
            if size < 20
                || *quoted.get(9)? != 17
                || quoted.get(12..16)? != src.octets()
                || quoted.get(16..20)? != dst.octets()
            {
                return None;
            }
            // A fragment after the first does not contain the UDP header.
            if u16::from_be_bytes([*quoted.get(6)?, *quoted.get(7)?]) & 0x1fff != 0 {
                return None;
            }
            size
        }
        (IpAddr::V6(src), IpAddr::V6(dst), 6) => {
            if quoted.get(8..24)? != src.octets() || quoted.get(24..40)? != dst.octets() {
                return None;
            }
            let mut next = *quoted.get(6)?;
            let mut position = 40;
            for _ in 0..8 {
                if next == 17 {
                    break;
                }
                let size = match next {
                    0 | 43 | 60 => (usize::from(*quoted.get(position + 1)?) + 1) * 8,
                    44 => {
                        if u16::from_be_bytes([
                            *quoted.get(position + 2)?,
                            *quoted.get(position + 3)?,
                        ]) & 0xfff8
                            != 0
                        {
                            return None;
                        }
                        8
                    }
                    _ => return None,
                };
                next = *quoted.get(position)?;
                position += size;
            }
            if next != 17 {
                return None;
            }
            position
        }
        _ => return None,
    };
    let udp = quoted.get(header..)?;
    if udp.get(..2)? != source.port().to_be_bytes()
        || udp.get(2..4)? != destination.port().to_be_bytes()
        || udp.len() < 8
    {
        return None;
    }
    let data = &udp[8..];
    let count = data.len().min(token.len());
    if data[..count] != token[..count] {
        return None;
    }
    Some((kind, code))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn raw_icmp_parser_rejects_unrelated_and_truncated_quotes() {
        let source: SocketAddr = "192.0.2.1:24000".parse().unwrap();
        let target: SocketAddr = "198.51.100.1:33435".parse().unwrap();
        let mut packet = vec![11, 0, 0, 0, 0, 0, 0, 0];
        let mut ip = vec![0u8; 20];
        ip[0] = 0x45;
        ip[9] = 17;
        ip[12..16].copy_from_slice(&[192, 0, 2, 1]);
        ip[16..20].copy_from_slice(&[198, 51, 100, 1]);
        packet.extend(ip);
        packet.extend(source.port().to_be_bytes());
        packet.extend(target.port().to_be_bytes());
        packet.extend([0, 8, 0, 0]);
        assert_eq!(
            matching_error(&packet, source, target, b"nonce"),
            Some((11, 0))
        );
        for length in 0..packet.len() {
            assert_eq!(
                matching_error(&packet[..length], source, target, b"nonce"),
                None
            );
        }
        packet[30] ^= 1;
        assert_eq!(matching_error(&packet, source, target, b"nonce"), None);
    }
    #[test]
    fn ipv6_quotes_require_exact_tuple_nonce_and_first_fragment() {
        let source: SocketAddr = "[2001:db8::1]:24000".parse().unwrap();
        let target: SocketAddr = "[2001:db8::2]:33435".parse().unwrap();
        let mut packet = vec![3, 0, 0, 0, 0, 0, 0, 0];
        let mut ip = vec![0u8; 40];
        ip[0] = 0x60;
        ip[6] = 44;
        let IpAddr::V6(src) = source.ip() else {
            unreachable!()
        };
        let IpAddr::V6(dst) = target.ip() else {
            unreachable!()
        };
        ip[8..24].copy_from_slice(&src.octets());
        ip[24..40].copy_from_slice(&dst.octets());
        packet.extend(ip);
        packet.extend([17, 0, 0, 0, 0, 0, 0, 0]);
        packet.extend(source.port().to_be_bytes());
        packet.extend(target.port().to_be_bytes());
        packet.extend([0, 13, 0, 0]);
        packet.extend(b"nonce");
        assert_eq!(
            matching_error(&packet, source, target, b"nonce"),
            Some((3, 0))
        );
        assert_eq!(matching_error(&packet, source, target, b"other"), None);
        packet[51] = 8;
        assert_eq!(matching_error(&packet, source, target, b"nonce"), None);
        packet[51] = 0;
        packet[55] = 1;
        assert_eq!(
            matching_error(&packet, source, target, b"nonce"),
            Some((3, 0))
        );
        for length in 0..64 {
            assert_eq!(
                matching_error(&packet[..length], source, target, b"nonce"),
                None
            );
        }
    }
    #[test]
    fn raw_socket_permission_is_reported_without_elevation() {
        for ipv6 in [false, true] {
            let result = available(ipv6);
            let capability = super::trace_capability(ipv6);
            assert_eq!(capability.supported, result.is_ok());
            assert_eq!(
                capability.permission_required.as_deref(),
                Some("root/raw ICMP socket permission")
            );
            if !capability.supported {
                assert!(capability.reason.is_some());
            }
        }
    }
}
