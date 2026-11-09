//! Local-only dual-IP/dual-port RFC 5780 service and stateful NAT emulator.
//! Compiled exclusively into tests; never installed with the product.
use crate::nettool::stun;
use astra_model::network_nat::Behavior;
use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{Arc, Mutex},
};
use tokio::net::UdpSocket;
use tokio::task::JoinSet;

type Transactions = HashMap<(SocketAddr, SocketAddr), HashSet<[u8; 12]>>;

pub struct Fixture {
    pub endpoints: [SocketAddr; 4],
    tasks: JoinSet<()>,
    pub contacts: Arc<Mutex<HashMap<SocketAddr, HashSet<SocketAddr>>>>,
}
pub struct Config {
    pub mapping: Behavior,
    pub filtering: Behavior,
    pub unstable: bool,
    pub supports_change: bool,
    pub malformed_first: bool,
    pub translate: bool,
    pub advertise_behavior: bool,
    pub filtering_fault: Option<FilteringFault>,
}
#[derive(Clone, Copy)]
pub enum FilteringFault {
    Rejected,
    InvalidPackets,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            mapping: Behavior::EndpointIndependent,
            filtering: Behavior::EndpointIndependent,
            unstable: false,
            supports_change: true,
            malformed_first: false,
            translate: true,
            advertise_behavior: true,
            filtering_fault: None,
        }
    }
}
pub async fn start(config: Config) -> Fixture {
    let first = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let second = if std::net::UdpSocket::bind("127.0.0.2:0").is_ok() {
        "127.0.0.2".parse().unwrap()
    } else {
        let snapshot = astra_platform::network_snapshot::Sampler::default()
            .sample("local-nat-fixture")
            .await;
        snapshot
            .interfaces
            .iter()
            .flat_map(|i| &i.addresses)
            .filter_map(|a| a.address.parse::<IpAddr>().ok())
            .find(|ip| ip.is_ipv4() && !ip.is_loopback() && !ip.is_unspecified())
            .expect("Local dual-IP STUN fixture requires a second local IPv4 address")
    };
    let a = UdpSocket::bind(SocketAddr::new(first, 0)).await.unwrap();
    let port_a = a.local_addr().unwrap().port();
    let b = UdpSocket::bind(SocketAddr::new(first, 0)).await.unwrap();
    let port_b = b.local_addr().unwrap().port();
    let c = UdpSocket::bind(SocketAddr::new(second, port_a))
        .await
        .unwrap();
    let d = UdpSocket::bind(SocketAddr::new(second, port_b))
        .await
        .unwrap();
    let sockets = Arc::new([a, b, c, d]);
    let endpoints = std::array::from_fn(|i| sockets[i].local_addr().unwrap());
    let contacts: Arc<Mutex<HashMap<SocketAddr, HashSet<SocketAddr>>>> = Default::default();
    let seen: Arc<Mutex<Transactions>> = Default::default();
    let config = Arc::new(config);
    let mut tasks = JoinSet::new();
    for index in 0..4 {
        let sockets = sockets.clone();
        let config = config.clone();
        let contacts = contacts.clone();
        let seen = seen.clone();
        tasks.spawn(async move {
            let mut buffer = [0; 1024];
            loop {
                let (len, client) = sockets[index].recv_from(&mut buffer).await.unwrap();
                if len < 20 || buffer[..2] != [0, 1] || buffer[4..8] != stun::COOKIE.to_be_bytes() {
                    continue;
                }
                let id: [u8; 12] = buffer[8..20].try_into().unwrap();
                let change = if len >= 28 && buffer[20..24] == [0, 3, 0, 4] {
                    u32::from_be_bytes(buffer[24..28].try_into().unwrap())
                } else {
                    0
                };
                let source_index = if config.supports_change {
                    index
                        ^ match change {
                            6 => 3,
                            2 => 1,
                            4 => 2,
                            _ => 0,
                        }
                } else {
                    index
                };
                let source = endpoints[source_index];
                let (allowed, fresh) = {
                    let mut map = contacts.lock().unwrap();
                    let touched = map.entry(client).or_default();
                    touched.insert(endpoints[index]);
                    let allowed = match config.filtering {
                        Behavior::EndpointIndependent => true,
                        Behavior::AddressDependent => touched.iter().any(|s| s.ip() == source.ip()),
                        _ => touched.contains(&source),
                    };
                    (allowed, touched.len() == 1)
                };
                // Capability checks prime all four endpoints; inject failures
                // only on the fresh filtering session, never on that control.
                if change != 0 && fresh {
                    if let Some(fault) = config.filtering_fault {
                        match fault {
                            FilteringFault::Rejected => {
                                let mut packet = vec![1, 0x11, 0, 8];
                                packet.extend(stun::COOKIE.to_be_bytes());
                                packet.extend(id);
                                packet.extend([0, 9, 0, 4, 0, 0, 4, 20]);
                                // Error replies may originate at the requested
                                // server rather than the requested alternate.
                                sockets[index].send_to(&packet, client).await.unwrap();
                            }
                            FilteringFault::InvalidPackets => {
                                let mut packet = response(id, client, Some(source), None);
                                packet[8] ^= 1;
                                for _ in 0..64 {
                                    sockets[source_index]
                                        .send_to(&packet, client)
                                        .await
                                        .unwrap();
                                }
                            }
                        }
                        continue;
                    }
                }
                if !allowed {
                    continue;
                }
                let instability = {
                    let mut map = seen.lock().unwrap();
                    let ids = map.entry((client, endpoints[index])).or_default();
                    ids.insert(id);
                    if config.unstable {
                        ids.len() as u16
                    } else {
                        0
                    }
                };
                let offset = match config.mapping {
                    Behavior::EndpointIndependent => 0,
                    Behavior::AddressDependent => (index / 2) * 100,
                    _ => index * 100,
                };
                let mapped = if config.translate {
                    SocketAddr::new(
                        "198.51.100.9".parse().unwrap(),
                        10000 + client.port() % 30000 + offset as u16 + instability,
                    )
                } else {
                    client
                };
                let other = endpoints[source_index ^ 3];
                let packet = response(
                    id,
                    mapped,
                    config.advertise_behavior.then_some(source),
                    config.advertise_behavior.then_some(other),
                );
                if config.malformed_first {
                    let mut wrong = packet.clone();
                    wrong[8] ^= 1;
                    sockets[source_index].send_to(&wrong, client).await.unwrap();
                    sockets[source_index]
                        .send_to(&packet[..packet.len() - 1], client)
                        .await
                        .unwrap();
                    sockets[source_index ^ 1]
                        .send_to(&packet, client)
                        .await
                        .unwrap();
                }
                sockets[source_index]
                    .send_to(&packet, client)
                    .await
                    .unwrap();
            }
        });
    }
    Fixture {
        endpoints,
        tasks,
        contacts,
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.tasks.abort_all();
    }
}
pub fn response(
    id: [u8; 12],
    mapped: SocketAddr,
    origin: Option<SocketAddr>,
    other: Option<SocketAddr>,
) -> Vec<u8> {
    let mut packet = vec![1, 1, 0, 0];
    packet.extend(stun::COOKIE.to_be_bytes());
    packet.extend(id);
    attribute(&mut packet, 0x0020, mapped, true, id);
    if let Some(origin) = origin {
        attribute(&mut packet, 0x802b, origin, false, id)
    }
    if let Some(other) = other {
        attribute(&mut packet, 0x802c, other, false, id)
    }
    let length = (packet.len() - 20) as u16;
    packet[2..4].copy_from_slice(&length.to_be_bytes());
    packet
}
fn attribute(packet: &mut Vec<u8>, kind: u16, address: SocketAddr, xor: bool, id: [u8; 12]) {
    packet.extend(kind.to_be_bytes());
    packet.extend(if address.is_ipv4() { 8u16 } else { 20u16 }.to_be_bytes());
    packet.extend([0, if address.is_ipv4() { 1 } else { 2 }]);
    packet
        .extend((address.port() ^ if xor { (stun::COOKIE >> 16) as u16 } else { 0 }).to_be_bytes());
    let mut mask = stun::COOKIE.to_be_bytes().to_vec();
    mask.extend(id);
    let bytes = match address.ip() {
        IpAddr::V4(ip) => ip.octets().to_vec(),
        IpAddr::V6(ip) => ip.octets().to_vec(),
    };
    for (i, byte) in bytes.into_iter().enumerate() {
        packet.push(byte ^ if xor { mask[i] } else { 0 });
    }
}
