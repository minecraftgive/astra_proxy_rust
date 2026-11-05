//! A raw TUN packet carries no IPv6 zone/interface identifier.
//! Without an explicit egress, link-local traffic may select the TUN again.

use astra_model::Network;
use std::{
    collections::HashMap,
    net::SocketAddr,
    time::{Duration, Instant},
};

#[derive(Default)]
pub(super) struct SelfExclusion {
    // Only non-self/unknown UDP flows are cached. Never retain a positive PID
    // match after its socket closes and another process reuses the port.
    // An active flow keeps its entry alive, so a steady stream (QUIC, games)
    // is checked once rather than every 500 ms; 500 ms of silence ends it.
    udp_allowed: HashMap<(SocketAddr, SocketAddr), Instant>,
}

impl SelfExclusion {
    pub(super) async fn excludes(&mut self, packet: &[u8]) -> bool {
        let Ok(parsed) = etherparse::SlicedPacket::from_ip(packet) else {
            return false;
        };
        let (source, destination) = match parsed.net {
            Some(etherparse::NetSlice::Ipv4(ip)) => (
                ip.header().source_addr().into(),
                ip.header().destination_addr().into(),
            ),
            Some(etherparse::NetSlice::Ipv6(ip)) => (
                ip.header().source_addr().into(),
                ip.header().destination_addr().into(),
            ),
            None => return false,
        };
        let (network, source_port, destination_port) = match parsed.transport {
            Some(etherparse::TransportSlice::Tcp(tcp)) if tcp.syn() && !tcp.ack() => {
                (Network::Tcp, tcp.source_port(), tcp.destination_port())
            }
            Some(etherparse::TransportSlice::Udp(udp)) => {
                (Network::Udp, udp.source_port(), udp.destination_port())
            }
            _ => return false,
        };
        let endpoints = (
            SocketAddr::new(source, source_port),
            SocketAddr::new(destination, destination_port),
        );
        if network == Network::Udp {
            if let Some(expires) = self.udp_allowed.get_mut(&endpoints) {
                let now = Instant::now();
                if *expires > now {
                    *expires = now + Duration::from_millis(500);
                    return false;
                }
            }
        }
        if super::process::is_self(network, endpoints.0, endpoints.1).await {
            return true;
        }
        if network == Network::Udp {
            if self.udp_allowed.len() >= 4096 {
                let now = Instant::now();
                self.udp_allowed.retain(|_, expires| *expires > now);
                if self.udp_allowed.len() >= 4096 {
                    self.udp_allowed.clear();
                }
            }
            self.udp_allowed
                .insert(endpoints, Instant::now() + Duration::from_millis(500));
        }
        false
    }
}

pub(super) fn accepts(packet: &[u8], has_egress_interface: bool) -> bool {
    if packet.first().map(|byte| byte >> 4) != Some(6) {
        return true;
    }
    let Ok(header) = etherparse::Ipv6HeaderSlice::from_slice(packet) else {
        return false;
    };
    !header.destination_addr().is_unicast_link_local() || has_egress_interface
}

#[cfg(test)]
#[path = "../test/platform_tun_ingress_test.rs"]
mod tests;
