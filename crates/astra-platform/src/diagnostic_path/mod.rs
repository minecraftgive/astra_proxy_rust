//! Read-only native physical-path checks. No helper processes or route changes.
use astra_model::network_path::PhysicalPathEvidence;
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::{
    io,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
};
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;
#[cfg(unix)]
use unix as os;
#[cfg(windows)]
use windows as os;
pub mod probes;

#[derive(Clone, Debug)]
pub(super) struct Interface {
    name: String,
    index: u32,
    addresses: Vec<IpAddr>,
    local_addresses: Vec<IpAddr>,
    fingerprint: String,
}

#[derive(Clone, Debug)]
pub struct PhysicalGuard {
    interface: Interface,
    source: IpAddr,
    verified: Arc<Mutex<Verification>>,
}
#[derive(Debug)]
struct Verification {
    route: Option<String>,
    binding: bool,
    sampled_at: String,
}
fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message.into())
}
fn now() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .to_string()
}

impl PhysicalGuard {
    /// Call after explicit user selection, both at admission and at actual start.
    pub async fn new(interface_id: &str, ipv6: bool) -> io::Result<Self> {
        if interface_id.is_empty()
            || interface_id.len() > 256
            || interface_id.chars().any(char::is_control)
        {
            return Err(invalid("Select a physical interface on the target Agent"));
        }
        let interface = os::inspect(interface_id)?;
        let source = interface
            .addresses
            .iter()
            .copied()
            .filter(|ip| ip.is_ipv6() == ipv6)
            .filter(|ip| !ip.is_loopback() && !ip.is_unspecified() && !ip.is_multicast())
            .min_by_key(|ip| match ip {
                IpAddr::V4(ip) => ip.is_link_local(),
                IpAddr::V6(ip) => ip.is_unicast_link_local(),
            })
            .ok_or_else(|| {
                invalid("Selected physical interface has no usable address in this family")
            })?;
        let guard = Self {
            interface,
            source,
            verified: Arc::new(Mutex::new(Verification {
                route: None,
                binding: true,
                sampled_at: now(),
            })),
        };
        // A name/source match is insufficient: the OS must accept and echo the binding.
        let _socket = guard.udp_socket()?;
        Ok(guard)
    }
    pub fn local_ip(&self) -> IpAddr {
        self.source
    }
    pub fn name(&self) -> &str {
        &self.interface.name
    }
    pub fn index(&self) -> u32 {
        self.interface.index
    }
    pub async fn for_family(&self, ipv6: bool) -> io::Result<Self> {
        let next = Self::new(self.name(), ipv6).await?;
        if next.index() != self.index() || next.interface.fingerprint != self.interface.fingerprint
        {
            return Err(invalid(
                "Physical interface changed while selecting auxiliary address family",
            ));
        }
        Ok(next)
    }
    pub fn evidence(&self) -> PhysicalPathEvidence {
        let verified = self.verified.lock().unwrap();
        PhysicalPathEvidence {
            interface_id: self.name().into(), interface_name: self.name().into(), interface_index: self.index(),
            source_address: self.source.to_string(), family: if self.source.is_ipv4() { "ipv4" } else { "ipv6" }.into(),
            sampled_at_ms: verified.sampled_at.clone(),
            binding_verified: verified.binding, route_verified: verified.route.is_some(), target: verified.route.clone(),
            method: "native physical classification + source/device binding readback + constrained kernel route lookup (no payload)".into(),
        }
    }
    pub fn destination(&self, mut target: SocketAddr) -> io::Result<SocketAddr> {
        if target.is_ipv4() != self.source.is_ipv4() {
            return Err(invalid(
                "Address family differs from selected physical path",
            ));
        }
        if let SocketAddr::V6(address) = &mut target {
            if address.ip().is_unicast_link_local() {
                if address.scope_id() != 0 && address.scope_id() != self.index() {
                    return Err(invalid(
                        "IPv6 scope differs from selected physical interface",
                    ));
                }
                address.set_scope_id(self.index());
            }
        }
        Ok(target)
    }
    /// Every sender calls this immediately before send and after receiving evidence.
    /// UDP connect performs route selection without emitting a UDP payload.
    pub async fn recheck(&self, target: SocketAddr) -> io::Result<()> {
        *self.verified.lock().unwrap() = Verification {
            route: None,
            binding: false,
            sampled_at: now(),
        };
        let current = os::inspect(self.name())?;
        if current.index != self.index()
            || current.fingerprint != self.interface.fingerprint
            || !current.addresses.contains(&self.source)
        {
            return Err(invalid(
                "Selected physical interface changed; probe stopped without fallback",
            ));
        }
        let target = self.destination(target)?;
        if target.ip().is_loopback()
            || target.ip().is_unspecified()
            || target.ip().is_multicast()
            || current.local_addresses.contains(&target.ip())
        {
            return Err(invalid(
                "Target is local/multicast, not a physical path destination",
            ));
        }
        if matches!((self.source,target.ip()),(IpAddr::V6(source),IpAddr::V6(destination)) if source.is_unicast_link_local() && !destination.is_unicast_link_local())
            || matches!((self.source,target.ip()),(IpAddr::V4(source),IpAddr::V4(destination)) if source.is_link_local() && !destination.is_link_local())
        {
            return Err(invalid(
                "Selected source address has only link-local scope for this destination",
            ));
        }
        let socket = self.udp_socket()?;
        os::route(self, target)?;
        socket.connect(&SockAddr::from(target))?;
        if socket.local_addr()?.as_socket().map(|value| value.ip()) != Some(self.source) {
            return Err(invalid("Kernel changed the pinned source address"));
        }
        os::verify_binding(&socket, &self.interface, self.source.is_ipv4())?;
        *self.verified.lock().unwrap() = Verification {
            route: Some(target.to_string()),
            binding: true,
            sampled_at: now(),
        };
        Ok(())
    }
    pub fn bind_udp(&self) -> io::Result<tokio::net::UdpSocket> {
        let socket = self.udp_socket()?;
        socket.set_nonblocking(true)?;
        tokio::net::UdpSocket::from_std(socket.into())
    }
    pub(super) fn udp_socket(&self) -> io::Result<Socket> {
        let socket = Socket::new(
            if self.source.is_ipv4() {
                Domain::IPV4
            } else {
                Domain::IPV6
            },
            Type::DGRAM,
            Some(Protocol::UDP),
        )?;
        self.bind_socket(&socket)?;
        Ok(socket)
    }
    pub(super) fn bind_socket(&self, socket: &Socket) -> io::Result<()> {
        os::bind(socket, &self.interface, self.source.is_ipv4())?;
        socket.bind(&SockAddr::from(
            self.destination(SocketAddr::new(self.source, 0))?,
        ))?;
        os::verify_binding(socket, &self.interface, self.source.is_ipv4())
    }
}

#[cfg(test)]
mod tests;
