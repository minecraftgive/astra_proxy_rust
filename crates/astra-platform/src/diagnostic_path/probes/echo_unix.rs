use super::*;
use std::mem::MaybeUninit;

pub(super) fn available(ipv6: bool) -> io::Result<()> {
    Socket::new(
        if ipv6 { Domain::IPV6 } else { Domain::IPV4 },
        Type::DGRAM,
        Some(if ipv6 {
            Protocol::ICMPV6
        } else {
            Protocol::ICMPV4
        }),
    )
    .map(drop)
}
pub(super) fn checksum(bytes: &[u8]) -> u16 {
    let mut sum = bytes
        .chunks(2)
        .map(|pair| u16::from_be_bytes([pair[0], *pair.get(1).unwrap_or(&0)]) as u32)
        .sum::<u32>();
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}
pub(super) fn probe(
    guard: Option<&PhysicalGuard>,
    target: IpAddr,
    seq: u16,
    timeout: Duration,
) -> io::Result<Reply> {
    let v4 = target.is_ipv4();
    let socket = Socket::new(
        if v4 { Domain::IPV4 } else { Domain::IPV6 },
        Type::DGRAM,
        Some(if v4 {
            Protocol::ICMPV4
        } else {
            Protocol::ICMPV6
        }),
    )?;
    let target = if let Some(guard) = guard {
        guard.bind_socket(&socket)?;
        guard.destination(SocketAddr::new(target, 0))?
    } else {
        SocketAddr::new(target, 0)
    };
    let token = uuid::Uuid::new_v4();
    let mut packet = vec![if v4 { 8 } else { 128 }, 0, 0, 0];
    packet.extend_from_slice(&(std::process::id() as u16).to_be_bytes());
    packet.extend_from_slice(&seq.to_be_bytes());
    packet.extend_from_slice(token.as_bytes());
    if v4 {
        let sum = checksum(&packet);
        packet[2..4].copy_from_slice(&sum.to_be_bytes());
    }
    let start = Instant::now();
    socket.send_to(&packet, &SockAddr::from(target))?;
    let mut storage = [MaybeUninit::<u8>::uninit(); 2048];
    loop {
        let remaining = timeout.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            return Ok(Reply::silence());
        }
        socket.set_read_timeout(Some(remaining))?;
        let (size, source) = match socket.recv_from(&mut storage) {
            Ok(result) => result,
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
        let data = unsafe { std::slice::from_raw_parts(storage.as_ptr().cast::<u8>(), size) };
        let (body, ttl) = if v4 && data.first().is_some_and(|b| b >> 4 == 4) {
            let header = usize::from(data[0] & 15) * 4;
            if header < 20 || header > data.len() {
                continue;
            }
            (&data[header..], Some(data[8]))
        } else {
            (data, None)
        };
        if source.as_socket().map(|address| address.ip()) != Some(target.ip())
            || body.len() < 24
            || body[0] != if v4 { 0 } else { 129 }
            || body[1] != 0
            || body[6..8] != seq.to_be_bytes()
            || &body[8..24] != token.as_bytes()
        {
            continue;
        }
        return Ok(Reply {
            from: Some(target.ip()),
            rtt_ms: Some(elapsed(start)),
            ttl,
            outcome: PathOutcome::Reply,
            icmp_type: Some(body[0]),
            icmp_code: Some(0),
        });
    }
}
