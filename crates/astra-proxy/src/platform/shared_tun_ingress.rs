//! A raw TUN packet carries no IPv6 zone/interface identifier.
//! Without an explicit egress, link-local traffic may select the TUN again.

use astra_ipc::Network;
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
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use std::{net::Ipv6Addr, time::Duration};

    fn syn(destination: &str, port: u16) -> Vec<u8> {
        let mut packet = Vec::new();
        etherparse::PacketBuilder::ipv6(
            "fe80::1".parse::<Ipv6Addr>().unwrap().octets(),
            destination.parse::<Ipv6Addr>().unwrap().octets(),
            64,
        )
        .tcp(port, 53, 1, 65535)
        .syn()
        .write(&mut packet, &[])
        .unwrap();
        packet
    }

    fn owned_packet(network: Network, source: SocketAddr, destination: SocketAddr) -> Vec<u8> {
        let builder = match (source.ip(), destination.ip()) {
            (std::net::IpAddr::V4(source), std::net::IpAddr::V4(destination)) => {
                etherparse::PacketBuilder::ipv4(source.octets(), destination.octets(), 64)
            }
            (std::net::IpAddr::V6(source), std::net::IpAddr::V6(destination)) => {
                etherparse::PacketBuilder::ipv6(source.octets(), destination.octets(), 64)
            }
            _ => panic!("IP family mismatch"),
        };
        let mut packet = Vec::new();
        match network {
            Network::Tcp => builder
                .tcp(source.port(), destination.port(), 1, 65535)
                .syn()
                .write(&mut packet, &[])
                .unwrap(),
            Network::Udp => builder
                .udp(source.port(), destination.port())
                .write(&mut packet, b"query")
                .unwrap(),
        }
        packet
    }

    #[cfg(any(windows, target_os = "linux"))]
    #[tokio::test]
    async fn own_tcp_and_udp_are_excluded_at_raw_ingress_for_both_ip_families() {
        let mut exclusion = SelfExclusion::default();
        for address in ["127.0.0.1:0", "[::1]:0"] {
            let listener = tokio::net::TcpListener::bind(address).await.unwrap();
            let destination = listener.local_addr().unwrap();
            let client = tokio::net::TcpStream::connect(destination).await.unwrap();
            let (_server, _) = listener.accept().await.unwrap();
            let udp = tokio::net::UdpSocket::bind(address).await.unwrap();
            // Ingress uses the shared owner snapshot (100 ms on Windows) and
            // never dumps the table per packet; let it catch up.
            tokio::time::sleep(Duration::from_millis(150)).await;
            let packet = owned_packet(Network::Tcp, client.local_addr().unwrap(), destination);
            assert!(exclusion.excludes(&packet).await);
            let packet = owned_packet(Network::Udp, udp.local_addr().unwrap(), destination);
            for _ in 0..4 {
                assert!(exclusion.excludes(&packet).await);
            }
            // Positive PID matches are deliberately not cached across port reuse.
            assert!(exclusion.udp_allowed.is_empty());
            let unknown = owned_packet(
                Network::Udp,
                "203.0.113.1:12345".parse().unwrap(),
                "203.0.113.2:53".parse().unwrap(),
            );
            assert!(!exclusion.excludes(&unknown).await);
            assert!(!exclusion.excludes(&unknown).await);
            assert_eq!(exclusion.udp_allowed.len(), 1);
            exclusion.udp_allowed.clear();
        }
    }

    #[test]
    fn link_local_requires_egress_but_global_and_ula_remain_supported() {
        for destination in ["fe80::123", "febf::123"] {
            let packet = syn(destination, 50000);
            assert!(!accepts(&packet, false));
            assert!(accepts(&packet, true));
            // TCP and UDP use the same IP-level protection.
            let packet = {
                let mut packet = packet;
                packet[6] = 17;
                packet
            };
            assert!(!accepts(&packet, false));
        }
        for destination in ["2001:db8::123", "fd00::123", "::1"] {
            assert!(accepts(&syn(destination, 50000), false));
        }
        assert!(!accepts(&[0x60], false));
    }

    #[tokio::test]
    async fn tcp_syn_retries_and_connection_churn_have_bounded_allocations() {
        let (stack, runner, _, tcp) = netstack_smoltcp::StackBuilder::default()
            .enable_tcp(true)
            .tcp_socket_limit(4)
            .tcp_recv_buffer_size(4096)
            .tcp_send_buffer_size(4096)
            .build()
            .unwrap();
        let runner = tokio::spawn(runner.unwrap());
        let (mut sink, mut packets) = stack.split();
        let drain = tokio::spawn(async move { while packets.next().await.is_some() {} });
        let mut tcp = tcp.unwrap();
        tokio::time::timeout(Duration::from_secs(15), async {
            // Repeat full capacity/drop cycles: slots must be reclaimed promptly,
            // even for connections that never complete the TCP handshake.
            for round in 0..32u16 {
                let mut streams = Vec::new();
                for offset in 0..4 {
                    let packet = syn("2001:db8::2", 40000 + round * 8 + offset);
                    sink.send(packet.clone()).await.unwrap();
                    streams.push(tcp.next().await.unwrap());
                    for _ in 0..32 {
                        sink.send(packet.clone()).await.unwrap();
                    }
                }
                // Distinct excess SYNs must also be rejected before allocation.
                for offset in 4..8 {
                    sink.send(syn("2001:db8::2", 40000 + round * 8 + offset))
                        .await
                        .unwrap();
                }
                assert!(
                    tokio::time::timeout(Duration::from_millis(20), tcp.next())
                        .await
                        .is_err(),
                    "retransmission or capacity overflow allocated another socket"
                );
                drop(streams);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("dropped TCP sockets must release capacity promptly");
        runner.abort();
        drain.abort();
    }

    #[tokio::test]
    async fn link_local_syn_burst_is_filtered_before_tcp_stream_allocation() {
        // Real smoltcp listener, no privileged adapter or external network.
        // Hold accepted streams to reproduce the allocation behind a recapture burst.
        for protected in [false, true] {
            let (mut stack, runner, _, tcp) = netstack_smoltcp::StackBuilder::default()
                .enable_tcp(true)
                .enable_udp(false)
                .enable_icmp(false)
                .tcp_recv_buffer_size(256 * 1024)
                .tcp_send_buffer_size(256 * 1024)
                .build()
                .unwrap();
            let runner = tokio::spawn(runner.unwrap());
            let mut tcp = tcp.unwrap();
            let mut streams = Vec::new();
            let mut admitted = 0;
            for port in 50000..50032 {
                let packet = syn("fe80::123", port);
                if !protected || accepts(&packet, false) {
                    stack.send(packet).await.unwrap();
                    streams.push(
                        tokio::time::timeout(Duration::from_secs(3), tcp.next())
                            .await
                            .unwrap()
                            .unwrap(),
                    );
                    admitted += 1;
                }
            }
            assert_eq!(admitted, if protected { 0 } else { 32 });
            if protected {
                assert!(tokio::time::timeout(Duration::from_millis(50), tcp.next())
                    .await
                    .is_err());
            }
            runner.abort();
        }
    }
}
