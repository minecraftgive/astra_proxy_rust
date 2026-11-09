//! Strict, bounded RFC 8489 Binding codec shared by UDP egress and NAT diagnostics.
use anyhow::{bail, ensure, Context, Result};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

pub(super) const COOKIE: u32 = 0x2112_a442;
pub(super) const MAX_PACKET: usize = 4096;
pub(super) type Transaction = [u8; 12];

pub(super) fn transaction() -> Transaction {
    // Skip UUID version/variant bytes so all 96 transaction bits are random.
    let uuid = uuid::Uuid::new_v4();
    let mut id = [0; 12];
    for (out, index) in [0, 1, 2, 3, 4, 5, 7, 9, 10, 11, 12, 13].iter().enumerate() {
        id[out] = uuid.as_bytes()[*index];
    }
    id
}
pub(super) fn request(id: Transaction, change: u32) -> Vec<u8> {
    let mut packet = vec![0, 1, 0, if change == 0 { 0 } else { 8 }];
    packet.extend(COOKIE.to_be_bytes());
    packet.extend(id);
    if change != 0 {
        packet.extend([0, 3, 0, 4]);
        packet.extend(change.to_be_bytes());
    }
    packet
}
pub(super) fn id_text(id: Transaction) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Clone, Debug)]
pub(super) struct Binding {
    pub mapped: SocketAddr,
    pub origin: Option<SocketAddr>,
    pub other: Option<SocketAddr>,
}
fn address(value: &[u8], xor: bool, id: Transaction) -> Result<SocketAddr> {
    ensure!(
        value.len() >= 4 && value[0] == 0,
        "Invalid STUN address header"
    );
    let port =
        u16::from_be_bytes([value[2], value[3]]) ^ if xor { (COOKIE >> 16) as u16 } else { 0 };
    ensure!(port != 0, "STUN address has zero port");
    let mut mask = [0; 16];
    mask[..4].copy_from_slice(&COOKIE.to_be_bytes());
    mask[4..].copy_from_slice(&id);
    let ip = match value[1] {
        1 => {
            ensure!(value.len() == 8, "Invalid IPv4 STUN address length");
            let mut bytes = [0; 4];
            for i in 0..4 {
                bytes[i] = value[i + 4] ^ if xor { mask[i] } else { 0 };
            }
            IpAddr::V4(Ipv4Addr::from(bytes))
        }
        2 => {
            ensure!(value.len() == 20, "Invalid IPv6 STUN address length");
            let mut bytes = [0; 16];
            for i in 0..16 {
                bytes[i] = value[i + 4] ^ if xor { mask[i] } else { 0 };
            }
            IpAddr::V6(Ipv6Addr::from(bytes))
        }
        _ => bail!("Unknown STUN address family"),
    };
    ensure!(
        !ip.is_unspecified() && !ip.is_multicast(),
        "Invalid STUN unicast address"
    );
    Ok(SocketAddr::new(ip, port))
}
pub(super) fn decode(
    packet: &[u8],
    id: Transaction,
    source: SocketAddr,
    expected: SocketAddr,
) -> Result<Binding> {
    ensure!(source == expected, "Unexpected STUN source");
    ensure!(
        (20..=MAX_PACKET).contains(&packet.len()),
        "Invalid STUN datagram length"
    );
    ensure!(packet[..2] == [1, 1], "Not a successful Binding response");
    let length = u16::from_be_bytes([packet[2], packet[3]]) as usize;
    ensure!(
        length.is_multiple_of(4) && length + 20 == packet.len(),
        "STUN length mismatch"
    );
    ensure!(
        packet[4..8] == COOKIE.to_be_bytes() && packet[8..20] == id,
        "STUN cookie or transaction mismatch"
    );
    let (mut mapped, mut legacy, mut origin, mut other) = (None, None, None, None);
    let mut cursor = 20;
    while cursor < packet.len() {
        ensure!(cursor + 4 <= packet.len(), "Truncated STUN attribute");
        let kind = u16::from_be_bytes([packet[cursor], packet[cursor + 1]]);
        let size = u16::from_be_bytes([packet[cursor + 2], packet[cursor + 3]]) as usize;
        let end = cursor + 4 + size;
        let padded = end
            .checked_add((4 - size % 4) % 4)
            .context("STUN attribute overflow")?;
        ensure!(padded <= packet.len(), "Truncated STUN attribute payload");
        let value = &packet[cursor + 4..end];
        match kind {
            0x0020 | 0x0001 | 0x802b | 0x802c => {
                let slot = match kind {
                    0x0020 => &mut mapped,
                    0x0001 => &mut legacy,
                    0x802b => &mut origin,
                    _ => &mut other,
                };
                ensure!(slot.is_none(), "Duplicate STUN address attribute");
                *slot = Some(address(value, kind == 0x0020, id)?);
            }
            // FINGERPRINT verifies the wire bytes before its own attribute.
            0x8028 => {
                ensure!(
                    size == 4 && padded == packet.len(),
                    "Invalid STUN fingerprint location"
                );
                let observed = u32::from_be_bytes(value.try_into().unwrap());
                ensure!(
                    observed == crc32(&packet[..cursor]) ^ 0x5354_554e,
                    "STUN fingerprint mismatch"
                );
            }
            0x8022 => {} // SOFTWARE is optional and never copied to a report.
            // Authentication is not negotiated for these public Binding probes.
            0x0008 | 0x001c => {
                bail!("Authenticated STUN response cannot be verified by this diagnostic")
            }
            value if value < 0x8000 => bail!("Unknown mandatory STUN attribute"),
            _ => {}
        }
        cursor = padded;
    }
    if let Some(origin) = origin {
        ensure!(origin == source, "RESPONSE-ORIGIN differs from UDP source");
    }
    let mapped = mapped
        .or(legacy)
        .context("STUN response has no mapped address")?;
    ensure!(
        mapped.is_ipv4() == source.is_ipv4(),
        "STUN mapped address family mismatch"
    );
    Ok(Binding {
        mapped,
        origin,
        other,
    })
}
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}
/// Return a numeric Binding error only after the same source/transaction/length
/// checks. Server-supplied reason text is not trusted or persisted.
pub(super) fn error_code(
    packet: &[u8],
    id: Transaction,
    source: SocketAddr,
    expected: SocketAddr,
) -> Option<u16> {
    if source != expected
        || !(20..=MAX_PACKET).contains(&packet.len())
        || packet[..2] != [1, 0x11]
        || packet[4..8] != COOKIE.to_be_bytes()
        || packet[8..20] != id
    {
        return None;
    }
    let length = u16::from_be_bytes([packet[2], packet[3]]) as usize;
    if !length.is_multiple_of(4) || length + 20 != packet.len() {
        return None;
    }
    let mut cursor = 20;
    let mut result = None;
    while cursor < packet.len() {
        if cursor + 4 > packet.len() {
            return None;
        }
        let kind = u16::from_be_bytes([packet[cursor], packet[cursor + 1]]);
        let len = u16::from_be_bytes([packet[cursor + 2], packet[cursor + 3]]) as usize;
        let end = cursor + 4 + len;
        if end > packet.len() {
            return None;
        }
        if kind == 9 {
            if len < 4
                || result.is_some()
                || packet[cursor + 4..cursor + 6] != [0, 0]
                || !(3..=6).contains(&packet[cursor + 6])
                || packet[cursor + 7] > 99
            {
                return None;
            }
            result = Some(u16::from(packet[cursor + 6]) * 100 + u16::from(packet[cursor + 7]));
        }
        cursor = end + (4 - len % 4) % 4;
    }
    (cursor == packet.len()).then_some(result).flatten()
}
/// Parse an endpoint without resolving it or retaining URL credentials.
pub(super) fn endpoint(server: &str) -> Result<(String, u16)> {
    ensure!(
        server.len() <= 1024 && !server.chars().any(char::is_whitespace),
        "Invalid STUN endpoint"
    );
    let text = server
        .strip_prefix("stun://")
        .or_else(|| server.strip_prefix("stun:"))
        .unwrap_or(server);
    ensure!(
        !text.contains(['@', '/', '?', '#']),
        "STUN endpoint cannot include credentials, paths or queries"
    );
    if let Ok(address) = text.parse::<SocketAddr>() {
        ensure!(address.port() != 0, "STUN endpoint port must be nonzero");
        return Ok((address.ip().to_string(), address.port()));
    }
    if let Ok(ip) = text.parse::<IpAddr>() {
        return Ok((ip.to_string(), 3478));
    }
    let (host, port) = text.rsplit_once(':').map_or((text, 3478), |(host, port)| {
        (host, port.parse().unwrap_or(0))
    });
    ensure!(
        !host.is_empty() && !host.contains(':') && port != 0,
        "Invalid STUN host or port"
    );
    Ok((host.to_owned(), port))
}

use astra_model::{
    config::protocol::OutboundConfig,
    nettool::{DetailedRequest, Egress, Event, Summary},
    network_nat::{Observation, StunResult},
    Target,
};
use std::time::{Duration, Instant};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpSocket, TcpStream, UdpSocket},
};

enum Session {
    Direct {
        socket: UdpSocket,
        target: SocketAddr,
    },
    Socks {
        socket: UdpSocket,
        relay: SocketAddr,
        control: TcpStream,
        target: Target,
    },
}
fn udp_socket(family: bool, interface: Option<&str>) -> Result<UdpSocket> {
    let socket = std::net::UdpSocket::bind(if family { "0.0.0.0:0" } else { "[::]:0" })?;
    socket.set_nonblocking(true)?;
    crate::egress::bind_interface(&socket, interface, family)?;
    Ok(UdpSocket::from_std(socket)?)
}
async fn tcp_socket(address: SocketAddr, interface: Option<&str>) -> Result<TcpStream> {
    let address = crate::egress::scoped_destination(address, interface)?;
    let socket = if address.is_ipv4() {
        TcpSocket::new_v4()?
    } else {
        TcpSocket::new_v6()?
    };
    crate::egress::bind_interface(&socket, interface, address.is_ipv4())?;
    Ok(socket.connect(address).await?)
}
impl Session {
    async fn open(
        request: &DetailedRequest,
        host: &str,
        port: u16,
        seq: u32,
        emit: &impl Fn(Event) -> Result<()>,
    ) -> Result<Self> {
        let route = super::detailed::path::Route::new(request)?;
        let node = if matches!(request.egress, Egress::Outbound(_)) {
            request.pinned.as_ref().and_then(|p| p.node.as_ref())
        } else {
            None
        };
        if let Some(node) = node {
            let OutboundConfig::Socks5 {
                server,
                username,
                password,
                ..
            } = node
            else {
                bail!("Unsupported: diagnostic proxy UDP currently verifies SOCKS5 only; no DIRECT fallback")
            };
            let entry = astra_protocols::http::authority_target(server)?;
            let endpoint = route
                .address(
                    seq,
                    "proxy_entry",
                    &entry.host,
                    entry.port,
                    None,
                    true,
                    emit,
                )
                .await?;
            let mut control = tcp_socket(endpoint, route.interface.as_deref()).await?;
            let method = if username.is_some() { 2 } else { 0 };
            ensure!(
                username.is_some() == password.is_some()
                    && username
                        .as_ref()
                        .is_none_or(|v| !v.is_empty() && v.len() <= 255)
                    && password.as_ref().is_none_or(|v| v.len() <= 255),
                "Invalid SOCKS authentication fields"
            );
            control.write_all(&[5, 1, method]).await?;
            let mut reply = [0; 2];
            control.read_exact(&mut reply).await?;
            ensure!(
                reply == [5, method],
                "SOCKS UDP authentication method rejected"
            );
            if let (Some(user), Some(password)) = (username, password) {
                let mut auth = vec![1, user.len() as u8];
                auth.extend(user.as_bytes());
                auth.push(password.len() as u8);
                auth.extend(password.as_bytes());
                control.write_all(&auth).await?;
                control.read_exact(&mut reply).await?;
                ensure!(reply == [1, 0], "SOCKS UDP authentication rejected");
            }
            let socket = udp_socket(endpoint.is_ipv4(), route.interface.as_deref())?;
            let mut associate = vec![5, 3, 0];
            astra_protocols::outbound::append_socks_address(
                &mut associate,
                &Target {
                    host: socket.local_addr()?.ip().to_string(),
                    port: socket.local_addr()?.port(),
                },
            )?;
            control.write_all(&associate).await?;
            let mut header = [0; 4];
            control.read_exact(&mut header).await?;
            ensure!(header[..3] == [5, 0, 0], "SOCKS UDP ASSOCIATE rejected");
            let ip = match header[3] {
                1 => { let mut bytes=[0;4]; control.read_exact(&mut bytes).await?; IpAddr::V4(bytes.into()) },
                4 => { let mut bytes=[0;16]; control.read_exact(&mut bytes).await?; IpAddr::V6(bytes.into()) },
                _ => bail!("Unsupported: SOCKS UDP relay must provide a literal endpoint; no hidden DNS fallback"),
            };
            let port_number = control.read_u16().await?;
            ensure!(port_number != 0, "Invalid SOCKS UDP relay port");
            let relay = SocketAddr::new(
                if ip.is_unspecified() {
                    endpoint.ip()
                } else {
                    ip
                },
                port_number,
            );
            ensure!(
                relay.is_ipv4() == endpoint.is_ipv4() && !relay.ip().is_multicast(),
                "Unsupported SOCKS UDP relay address family"
            );
            let relay = crate::egress::scoped_destination(relay, route.interface.as_deref())?;
            let host = if request.family.is_some() && host.parse::<IpAddr>().is_err() {
                route
                    .address(seq, "stun_service", host, port, None, true, emit)
                    .await?
                    .ip()
                    .to_string()
            } else {
                host.to_owned()
            };
            if let Ok(ip) = host.parse::<IpAddr>() {
                ensure!(
                    request.family.is_none_or(|f| f.matches(ip)),
                    "STUN target address family mismatch"
                );
            }
            emit(Event::Resolution {
                seq,
                purpose: "stun_service".into(),
                host: host.clone(),
                method: if host.parse::<IpAddr>().is_ok() {
                    "literal"
                } else {
                    "remote_proxy"
                }
                .into(),
                resolver: None,
                addresses: vec![],
                interface: route.interface.clone(),
            })?;
            Ok(Self::Socks {
                socket,
                relay,
                control,
                target: Target { host, port },
            })
        } else {
            let target = route
                .address(
                    seq,
                    "stun_service",
                    host,
                    port,
                    None,
                    matches!(request.egress, Egress::Outbound(_)),
                    emit,
                )
                .await?;
            ensure!(
                request.family.is_none_or(|f| f.matches(target.ip())),
                "STUN target address family mismatch"
            );
            let target = crate::egress::scoped_destination(target, route.interface.as_deref())?;
            Ok(Self::Direct {
                socket: udp_socket(target.is_ipv4(), route.interface.as_deref())?,
                target,
            })
        }
    }
    fn local(&self) -> Result<SocketAddr> {
        Ok(match self {
            Self::Direct { socket, .. } | Self::Socks { socket, .. } => socket.local_addr()?,
        })
    }
    async fn send(&self, packet: &[u8]) -> Result<()> {
        match self {
            Self::Direct { socket, target } => {
                socket.send_to(packet, target).await?;
            }
            Self::Socks {
                socket,
                relay,
                target,
                ..
            } => {
                let mut frame = vec![0, 0, 0];
                astra_protocols::outbound::append_socks_address(&mut frame, target)?;
                frame.extend(packet);
                socket.send_to(&frame, relay).await?;
            }
        }
        Ok(())
    }
    async fn receive(&self, buffer: &mut [u8]) -> Result<(usize, SocketAddr)> {
        match self {
            Self::Direct { socket, target } => {
                let (len, source) = socket.recv_from(buffer).await?;
                ensure!(source == *target, "Unexpected STUN peer");
                Ok((len, source))
            }
            Self::Socks {
                socket,
                relay,
                control,
                target,
            } => {
                let (len, from) = loop {
                    tokio::select! {
                        result=socket.recv_from(buffer)=>break result?,
                        ready=control.readable()=>{
                            ready?;
                            let mut byte=[0];
                            match control.try_read(&mut byte) {
                                Err(error) if error.kind()==std::io::ErrorKind::WouldBlock=>continue,
                                _=>bail!("SOCKS UDP control session ended or sent unexpected data"),
                            }
                        }
                    }
                };
                ensure!(from == *relay, "Unexpected SOCKS UDP relay source");
                let (peer, payload) = astra_protocols::socks5::udp::decode(&buffer[..len])?;
                let source = SocketAddr::new(
                    peer.host
                        .parse()
                        .context("SOCKS relay response omitted source IP")?,
                    peer.port,
                );
                ensure!(
                    source.port() == target.port
                        && target
                            .host
                            .parse::<IpAddr>()
                            .map_or(true, |ip| ip == source.ip()),
                    "Unexpected relayed STUN source"
                );
                let offset = len - payload.len();
                let count = payload.len();
                buffer.copy_within(offset..len, 0);
                Ok((count, source))
            }
        }
    }
}
pub async fn run(
    request: &DetailedRequest,
    server: &str,
    emit: &impl Fn(Event) -> Result<()>,
) -> Result<Summary> {
    let (host, port) = endpoint(server)?;
    let server_label = host.parse::<IpAddr>().map_or_else(
        |_| format!("{host}:{port}"),
        |ip| SocketAddr::new(ip, port).to_string(),
    );
    ensure!(
        port != 0 && (1..=50).contains(&request.count),
        "Invalid STUN target or count"
    );
    let mut summary = Summary::default();
    for seq in 1..=request.count {
        let id = transaction();
        let packet = request_packet(id);
        let started = Instant::now();
        let mut observation = Observation {
            phase: "udp_stun_egress".into(),
            server: server_label.clone(),
            expected_source: server_label.clone(),
            source: None,
            local_endpoint: "unconfirmed".into(),
            mapped_endpoint: None,
            other_address: None,
            response_origin: None,
            transaction_id: id_text(id),
            response_received: false,
            attempts: 0,
            discarded_packets: 0,
            elapsed_ms: 0,
            error: None,
        };
        let mut proxy = false;
        let result = tokio::time::timeout(
            Duration::from_millis(request.timeout_ms.clamp(100, 30_000)),
            async {
                // One independently owned UDP/control session per attempt/target.
                let session = Session::open(request, &host, port, seq, emit).await?;
                proxy = matches!(session, Session::Socks { .. });
                observation.local_endpoint = session.local()?.to_string();
                let mut buffer = vec![0; 65536];
                for wait in [500, 1000, 2000] {
                    session.send(&packet).await?;
                    observation.attempts += 1;
                    let until = tokio::time::Instant::now() + Duration::from_millis(wait);
                    loop {
                        match tokio::time::timeout_at(until, session.receive(&mut buffer)).await {
                            Err(_) => break,
                            Ok(Err(_)) => {
                                observation.discarded_packets += 1;
                            }
                            Ok(Ok((len, source))) => {
                                match decode(&buffer[..len], id, source, source) {
                                    Ok(reply) => {
                                        observation.response_received = true;
                                        observation.source = Some(source.to_string());
                                        observation.mapped_endpoint =
                                            Some(reply.mapped.to_string());
                                        observation.response_origin =
                                            reply.origin.map(|s| s.to_string());
                                        observation.other_address =
                                            reply.other.map(|s| s.to_string());
                                        return Ok::<_, anyhow::Error>(());
                                    }
                                    Err(_) => {
                                        if let Some(code) =
                                            error_code(&buffer[..len], id, source, source)
                                        {
                                            observation.response_received = true;
                                            observation.source = Some(source.to_string());
                                            bail!("STUN server rejected the request (code {code})");
                                        }
                                        observation.discarded_packets += 1;
                                    }
                                }
                            }
                        }
                        ensure!(
                            observation.discarded_packets < 64,
                            "Invalid UDP/STUN packet budget exhausted"
                        );
                    }
                }
                bail!("No valid STUN response within bounded retransmissions")
            },
        )
        .await;
        observation.elapsed_ms = started.elapsed().as_millis() as u64;
        observation.error = match result {
            Err(_) => Some("UDP/STUN attempt deadline reached".into()),
            Ok(Err(error)) => Some(error.to_string()),
            Ok(Ok(())) => None,
        };
        summary.sent += 1;
        if observation.mapped_endpoint.is_some() {
            summary.received += 1;
        }
        emit(Event::Stun(StunResult { server:server_label.clone(),egress:request.egress.to_string(),proxy_session:proxy,observation,note:"One server observation on one independent UDP session; not a NAT classification or cross-server mapping comparison".into() }))?;
        if seq < request.count {
            tokio::time::sleep(Duration::from_millis(
                request.interval_ms.clamp(100, 10_000),
            ))
            .await;
        }
    }
    Ok(summary)
}
fn request_packet(id: Transaction) -> Vec<u8> {
    request(id, 0)
}

#[cfg(test)]
#[path = "../test/network_stun_test.rs"]
mod tests;
