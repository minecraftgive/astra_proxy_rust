//! Unprivileged ICMP echo. Windows uses the ICMP helper API pinned by source
//! address; Unix uses datagram ICMP sockets bound to the device.
use anyhow::Result;
use std::{net::IpAddr, time::Duration};

pub(super) struct Reply {
    pub from: IpAddr,
    pub rtt_ms: f64,
    pub ttl: Option<u8>,
}
const PAYLOAD: &[u8; 32] = b"astra-dual-core nettool payload!";

#[cfg(unix)]
pub(super) use unix::echo;
#[cfg(windows)]
pub(super) use windows::{echo, source_address};

#[cfg(windows)]
mod windows {
    use super::*;
    use anyhow::{bail, ensure, Context};
    use std::net::{Ipv4Addr, Ipv6Addr};
    use windows_sys::Win32::{
        Foundation::{GetLastError, INVALID_HANDLE_VALUE},
        NetworkManagement::IpHelper::*,
        Networking::WinSock::*,
    };

    /// Preferred unicast address of an adapter; the ICMP API binds by source, not by index.
    pub fn source_address(interface: &str, ipv4: bool) -> Result<IpAddr> {
        let index = crate::platform::windows_network::interface_index(interface)?;
        let mut pointer = std::ptr::null_mut();
        let status = unsafe {
            GetUnicastIpAddressTable(if ipv4 { AF_INET } else { AF_INET6 }, &mut pointer)
        };
        ensure!(
            status == 0,
            "read adapter addresses: {}",
            std::io::Error::from_raw_os_error(status as i32)
        );
        struct Table(*mut MIB_UNICASTIPADDRESS_TABLE);
        impl Drop for Table {
            fn drop(&mut self) {
                unsafe { FreeMibTable(self.0.cast()) };
            }
        }
        let table = Table(pointer);
        let rows = unsafe {
            std::slice::from_raw_parts((*table.0).Table.as_ptr(), (*table.0).NumEntries as usize)
        };
        let mut candidates: Vec<IpAddr> = rows
            .iter()
            .filter(|row| {
                row.InterfaceIndex == index
                    && !row.SkipAsSource
                    && row.DadState == IpDadStatePreferred
            })
            .filter_map(|row| unsafe {
                match row.Address.si_family {
                    AF_INET => Some(IpAddr::V4(Ipv4Addr::from(
                        row.Address.Ipv4.sin_addr.S_un.S_addr.to_ne_bytes(),
                    ))),
                    AF_INET6 => Some(IpAddr::V6(Ipv6Addr::from(
                        row.Address.Ipv6.sin6_addr.u.Byte,
                    ))),
                    _ => None,
                }
            })
            .collect();
        // Global addresses first: link-local sources cannot reach the internet.
        candidates.sort_by_key(|ip| match ip {
            IpAddr::V6(v6) => v6.is_unicast_link_local(),
            IpAddr::V4(v4) => v4.is_link_local(),
        });
        candidates.into_iter().next().with_context(|| {
            format!(
                "adapter {interface} has no usable IPv{} address",
                if ipv4 { 4 } else { 6 }
            )
        })
    }

    struct Handle(windows_sys::Win32::Foundation::HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe { IcmpCloseHandle(self.0) };
        }
    }

    pub fn echo(
        address: IpAddr,
        interface: Option<&str>,
        _seq: u16,
        timeout: Duration,
    ) -> Result<Reply> {
        let source = interface
            .map(|name| source_address(name, address.is_ipv4()))
            .transpose()?;
        let timeout = timeout.as_millis().clamp(1, u32::MAX as u128) as u32;
        let mut buffer = vec![0u8; 1024];
        match address {
            IpAddr::V4(destination) => {
                let handle = Handle(unsafe { IcmpCreateFile() });
                ensure!(handle.0 != INVALID_HANDLE_VALUE, "IcmpCreateFile failed");
                let source = match source {
                    Some(IpAddr::V4(ip)) => u32::from_ne_bytes(ip.octets()),
                    _ => 0,
                };
                let count = unsafe {
                    IcmpSendEcho2Ex(
                        handle.0,
                        std::ptr::null_mut(),
                        None,
                        std::ptr::null(),
                        source,
                        u32::from_ne_bytes(destination.octets()),
                        PAYLOAD.as_ptr().cast(),
                        PAYLOAD.len() as u16,
                        std::ptr::null(),
                        buffer.as_mut_ptr().cast(),
                        buffer.len() as u32,
                        timeout,
                    )
                };
                if count == 0 {
                    bail!("{}", status(unsafe { GetLastError() }));
                }
                let reply: ICMP_ECHO_REPLY =
                    unsafe { std::ptr::read_unaligned(buffer.as_ptr().cast()) };
                ensure!(reply.Status == 0, "{}", status(reply.Status));
                Ok(Reply {
                    from: IpAddr::V4(Ipv4Addr::from(reply.Address.to_ne_bytes())),
                    rtt_ms: reply.RoundTripTime as f64,
                    ttl: Some(reply.Options.Ttl),
                })
            }
            IpAddr::V6(destination) => {
                let handle = Handle(unsafe { Icmp6CreateFile() });
                ensure!(handle.0 != INVALID_HANDLE_VALUE, "Icmp6CreateFile failed");
                let source = SOCKADDR_IN6 {
                    sin6_family: AF_INET6,
                    sin6_addr: IN6_ADDR {
                        u: IN6_ADDR_0 {
                            Byte: match source {
                                Some(IpAddr::V6(ip)) => ip.octets(),
                                _ => [0; 16],
                            },
                        },
                    },
                    ..Default::default()
                };
                let target = SOCKADDR_IN6 {
                    sin6_family: AF_INET6,
                    sin6_addr: IN6_ADDR {
                        u: IN6_ADDR_0 {
                            Byte: destination.octets(),
                        },
                    },
                    ..Default::default()
                };
                let count = unsafe {
                    Icmp6SendEcho2(
                        handle.0,
                        std::ptr::null_mut(),
                        None,
                        std::ptr::null(),
                        &source,
                        &target,
                        PAYLOAD.as_ptr().cast(),
                        PAYLOAD.len() as u16,
                        std::ptr::null(),
                        buffer.as_mut_ptr().cast(),
                        buffer.len() as u32,
                        timeout,
                    )
                };
                if count == 0 {
                    bail!("{}", status(unsafe { GetLastError() }));
                }
                let reply: ICMPV6_ECHO_REPLY_LH =
                    unsafe { std::ptr::read_unaligned(buffer.as_ptr().cast()) };
                ensure!(reply.Status == 0, "{}", status(reply.Status));
                let words = reply.Address.sin6_addr;
                let mut octets = [0u8; 16];
                for (index, word) in words.iter().enumerate() {
                    // Stored in network order, read as native u16.
                    octets[index * 2..index * 2 + 2].copy_from_slice(&word.to_ne_bytes());
                }
                Ok(Reply {
                    from: IpAddr::V6(Ipv6Addr::from(octets)),
                    rtt_ms: reply.RoundTripTime as f64,
                    ttl: None,
                })
            }
        }
    }

    fn status(code: u32) -> String {
        match code {
            11002 => "destination network unreachable".into(),
            11003 => "destination host unreachable".into(),
            11004 => "destination protocol unreachable".into(),
            11005 => "destination port unreachable".into(),
            11010 => "request timed out".into(),
            11013 => "TTL expired in transit".into(),
            11050 => "general failure (no route from this source)".into(),
            11040 => "destination unreachable".into(),
            code => format!(
                "ICMP error {code}: {}",
                std::io::Error::from_raw_os_error(code as i32)
            ),
        }
    }
}

#[cfg(unix)]
mod unix {
    use super::*;
    use anyhow::{bail, Context};
    use socket2::{Domain, Protocol, SockAddr, Socket, Type};
    use std::{io::Read, time::Instant};

    fn checksum(bytes: &[u8]) -> u16 {
        let mut sum = bytes
            .chunks(2)
            .map(|pair| u16::from_be_bytes([pair[0], *pair.get(1).unwrap_or(&0)]) as u32)
            .sum::<u32>();
        while sum >> 16 != 0 {
            sum = (sum & 0xffff) + (sum >> 16);
        }
        !(sum as u16)
    }

    pub fn echo(
        address: IpAddr,
        interface: Option<&str>,
        seq: u16,
        timeout: Duration,
    ) -> Result<Reply> {
        let v4 = address.is_ipv4();
        let socket = if v4 {
            Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::ICMPV4))
        } else {
            Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::ICMPV6))
        }
        .context("open ICMP socket (Linux: allow the group in net.ipv4.ping_group_range)")?;
        crate::egress::bind_interface(&socket, interface, v4)?;
        let destination =
            crate::egress::scoped_destination(std::net::SocketAddr::new(address, 0), interface)?;
        let mut packet = vec![if v4 { 8 } else { 128 }, 0, 0, 0];
        packet.extend_from_slice(&(std::process::id() as u16).to_be_bytes());
        packet.extend_from_slice(&seq.to_be_bytes());
        packet.extend_from_slice(PAYLOAD);
        // ICMPv6 checksums are kernel-computed (pseudo-header).
        if v4 {
            let sum = checksum(&packet);
            packet[2..4].copy_from_slice(&sum.to_be_bytes());
        }
        let start = Instant::now();
        socket.send_to(&packet, &SockAddr::from(destination))?;
        let mut buffer = [0u8; 1500];
        loop {
            let left = timeout.saturating_sub(start.elapsed());
            if left.is_zero() {
                bail!("request timed out");
            }
            socket.set_read_timeout(Some(left))?;
            let size = match (&socket).read(&mut buffer) {
                Ok(size) => size,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    bail!("request timed out")
                }
                Err(error) => return Err(error.into()),
            };
            let mut data = &buffer[..size];
            let mut ttl = None;
            // macOS datagram ICMPv4 sockets deliver the IP header too.
            if v4 && size >= 20 && data[0] >> 4 == 4 {
                ttl = Some(data[8]);
                data = &data[((data[0] & 0x0f) as usize * 4).min(size)..];
            }
            let reply_type = if v4 { 0 } else { 129 };
            if data.len() >= 8 && data[0] == reply_type && data[6..8] == seq.to_be_bytes() {
                return Ok(Reply {
                    from: address,
                    rtt_ms: (start.elapsed().as_micros() as f64 / 10.0).round() / 100.0,
                    ttl,
                });
            }
        }
    }
}
