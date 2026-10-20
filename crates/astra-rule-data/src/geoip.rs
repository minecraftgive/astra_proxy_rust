//! V2Ray GeoIPList protobuf and indexed IP category matching.
use anyhow::{ensure, Context, Result};
use prost::Message;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
};

#[derive(Clone, PartialEq, Message)]
pub struct GeoIpList {
    #[prost(message, repeated, tag = "1")]
    pub entry: Vec<GeoIp>,
}
#[derive(Clone, PartialEq, Message)]
pub struct GeoIp {
    #[prost(string, tag = "1")]
    pub country_code: String,
    #[prost(message, repeated, tag = "2")]
    pub cidr: Vec<Cidr>,
    #[prost(bool, tag = "3")]
    pub inverse_match: bool,
}
#[derive(Clone, PartialEq, Message)]
pub struct Cidr {
    #[prost(bytes = "vec", tag = "1")]
    pub ip: Vec<u8>,
    #[prost(uint32, tag = "2")]
    pub prefix: u32,
}

pub fn category(input: &str) -> Result<String> {
    ensure!(
        !input.is_empty()
            && input.len() <= 128
            && input
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)),
        "invalid GEOIP category: {input}"
    );
    Ok(input.to_ascii_uppercase())
}

#[derive(Debug)]
pub struct Group {
    v4: Vec<(u8, HashSet<u32>)>,
    v6: Vec<(u8, HashSet<u128>)>,
    inverse: bool,
}
fn mask4(bits: u8) -> u32 {
    if bits == 0 {
        0
    } else {
        u32::MAX << (32 - bits)
    }
}
fn mask6(bits: u8) -> u128 {
    if bits == 0 {
        0
    } else {
        u128::MAX << (128 - bits)
    }
}
impl Group {
    pub fn contains(&self, ip: IpAddr) -> bool {
        let found = match ip.to_canonical() {
            IpAddr::V4(ip) => self
                .v4
                .iter()
                .any(|(bits, nets)| nets.contains(&(u32::from(ip) & mask4(*bits)))),
            IpAddr::V6(ip) => self
                .v6
                .iter()
                .any(|(bits, nets)| nets.contains(&(u128::from(ip) & mask6(*bits)))),
        };
        found != self.inverse
    }
}
#[derive(Debug)]
pub struct Database {
    groups: HashMap<String, Group>,
}
impl Database {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let list = GeoIpList::decode(bytes).context("invalid geoip.dat protobuf")?;
        ensure!(!list.entry.is_empty(), "geoip.dat contains no groups");
        let mut groups = HashMap::new();
        let mut count = 0usize;
        for group in list.entry {
            let key = category(&group.country_code)?;
            count += group.cidr.len();
            ensure!(count <= 5_000_000, "geoip.dat exceeds CIDR limit");
            let mut v4: BTreeMap<u8, HashSet<u32>> = BTreeMap::new();
            let mut v6: BTreeMap<u8, HashSet<u128>> = BTreeMap::new();
            for cidr in group.cidr {
                match cidr.ip.len() {
                    4 => {
                        ensure!(cidr.prefix <= 32, "invalid IPv4 prefix in GEOIP {key}");
                        let ip = Ipv4Addr::from(<[u8; 4]>::try_from(cidr.ip.as_slice())?);
                        let bits = cidr.prefix as u8;
                        v4.entry(bits)
                            .or_default()
                            .insert(u32::from(ip) & mask4(bits));
                    }
                    16 => {
                        ensure!(cidr.prefix <= 128, "invalid IPv6 prefix in GEOIP {key}");
                        let ip = Ipv6Addr::from(<[u8; 16]>::try_from(cidr.ip.as_slice())?);
                        let bits = cidr.prefix as u8;
                        v6.entry(bits)
                            .or_default()
                            .insert(u128::from(ip) & mask6(bits));
                    }
                    _ => anyhow::bail!("invalid IP bytes in GEOIP {key}"),
                }
            }
            ensure!(
                groups
                    .insert(
                        key,
                        Group {
                            v4: v4.into_iter().collect(),
                            v6: v6.into_iter().collect(),
                            inverse: group.inverse_match
                        }
                    )
                    .is_none(),
                "duplicate GEOIP category"
            );
        }
        Ok(Self { groups })
    }
    pub fn group(&self, name: &str) -> Result<&Group> {
        self.groups
            .get(&category(name)?)
            .with_context(|| format!("GEOIP category not found: {name}"))
    }
    /// The engine stores validated uppercase keys; avoid allocating per request.
    pub fn matches(&self, normalized_name: &str, ip: IpAddr) -> bool {
        self.groups
            .get(normalized_name)
            .is_some_and(|group| group.contains(ip))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn data(inverse: bool) -> Vec<u8> {
        GeoIpList {
            entry: vec![GeoIp {
                country_code: "telegram".into(),
                inverse_match: inverse,
                cidr: vec![
                    Cidr {
                        ip: vec![149, 154, 160, 0],
                        prefix: 20,
                    },
                    Cidr {
                        ip: "2001:db8::".parse::<Ipv6Addr>().unwrap().octets().to_vec(),
                        prefix: 32,
                    },
                ],
            }],
        }
        .encode_to_vec()
    }
    #[test]
    fn categories_match_v4_v6_mapped_and_inverse() {
        for inverse in [false, true] {
            let db = Database::decode(&data(inverse)).unwrap();
            let group = db.group("TeLeGrAm").unwrap();
            for ip in ["149.154.160.1", "::ffff:149.154.160.1", "2001:db8::1"] {
                assert_eq!(group.contains(ip.parse().unwrap()), !inverse);
            }
            for ip in ["149.154.176.1", "2001:db9::1"] {
                assert_eq!(group.contains(ip.parse().unwrap()), inverse);
            }
            assert!(db.group("missing").is_err());
        }
    }
    #[test]
    fn corrupt_databases_and_selectors_are_rejected() {
        for bytes in [
            vec![],
            b"not a database".to_vec(),
            GeoIpList {
                entry: vec![GeoIp {
                    country_code: "cn".into(),
                    cidr: vec![Cidr {
                        ip: vec![1, 2, 3, 4],
                        prefix: 33,
                    }],
                    inverse_match: false,
                }],
            }
            .encode_to_vec(),
        ] {
            assert!(Database::decode(&bytes).is_err());
        }
        for name in ["", "CN,x", "bad name", "../cn", "cn@foo"] {
            assert!(category(name).is_err());
        }
    }
}
