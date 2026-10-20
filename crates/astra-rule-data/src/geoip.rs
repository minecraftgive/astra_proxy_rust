//! V2Ray GeoIPList protobuf and indexed IP category matching.
use anyhow::{ensure, Context, Result};
use prost::{
    encoding::{decode_key, decode_varint, skip_field, DecodeContext, WireType},
    Message,
};
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

// Decode metadata without materializing the repeated CIDR messages.
#[derive(Clone, PartialEq, Message)]
struct GroupMetadata {
    #[prost(string, tag = "1")]
    country_code: String,
    #[prost(bool, tag = "3")]
    inverse_match: bool,
}

fn visit_messages(
    mut bytes: &[u8],
    field: u32,
    mut visit: impl FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    while !bytes.is_empty() {
        let (tag, wire) = decode_key(&mut bytes)?;
        if tag == field {
            ensure!(
                wire == WireType::LengthDelimited,
                "invalid GEOIP message wire type"
            );
            let len = decode_varint(&mut bytes)?;
            ensure!(len <= bytes.len() as u64, "truncated GEOIP message");
            let (message, rest) = bytes.split_at(len as usize);
            visit(message)?;
            bytes = rest;
        } else {
            skip_field(wire, tag, &mut bytes, DecodeContext::default())?;
        }
    }
    Ok(())
}

impl Database {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        Self::decode_filtered(bytes, None)
    }

    /// Validate the entire database, but retain indexes only for requested categories.
    /// Stream individual CIDRs so unused groups do not create a temporary object graph.
    pub fn decode_selected(bytes: &[u8], categories: &[String]) -> Result<Self> {
        let selected = categories
            .iter()
            .map(|name| category(name))
            .collect::<Result<HashSet<_>>>()?;
        let database = Self::decode_filtered(bytes, Some(&selected))?;
        for name in &selected {
            database.group(name)?;
        }
        Ok(database)
    }

    fn decode_filtered(bytes: &[u8], selected: Option<&HashSet<String>>) -> Result<Self> {
        let mut groups = HashMap::new();
        let mut seen = HashSet::new();
        let mut count = 0usize;
        let mut cidr = Cidr::default();
        visit_messages(bytes, 1, |bytes| {
            let group = GroupMetadata::decode(bytes)?;
            let key = category(&group.country_code)?;
            ensure!(seen.insert(key.clone()), "duplicate GEOIP category");
            let retain = selected.is_none_or(|names| names.contains(&key));
            let mut v4: BTreeMap<u8, HashSet<u32>> = BTreeMap::new();
            let mut v6: BTreeMap<u8, HashSet<u128>> = BTreeMap::new();
            visit_messages(bytes, 2, |bytes| {
                count += 1;
                ensure!(count <= 5_000_000, "geoip.dat exceeds CIDR limit");
                cidr.clear();
                cidr.merge(bytes)?;
                match cidr.ip.len() {
                    4 => {
                        ensure!(cidr.prefix <= 32, "invalid IPv4 prefix in GEOIP {key}");
                        if retain {
                            let ip = Ipv4Addr::from(<[u8; 4]>::try_from(cidr.ip.as_slice())?);
                            let bits = cidr.prefix as u8;
                            v4.entry(bits)
                                .or_default()
                                .insert(u32::from(ip) & mask4(bits));
                        }
                    }
                    16 => {
                        ensure!(cidr.prefix <= 128, "invalid IPv6 prefix in GEOIP {key}");
                        if retain {
                            let ip = Ipv6Addr::from(<[u8; 16]>::try_from(cidr.ip.as_slice())?);
                            let bits = cidr.prefix as u8;
                            v6.entry(bits)
                                .or_default()
                                .insert(u128::from(ip) & mask6(bits));
                        }
                    }
                    _ => anyhow::bail!("invalid IP bytes in GEOIP {key}"),
                }
                Ok(())
            })?;
            if retain {
                groups.insert(
                    key,
                    Group {
                        v4: v4.into_iter().collect(),
                        v6: v6.into_iter().collect(),
                        inverse: group.inverse_match,
                    },
                );
            }
            Ok(())
        })
        .context("invalid geoip.dat protobuf")?;
        ensure!(!seen.is_empty(), "geoip.dat contains no groups");
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
    fn selected_categories_keep_matching_semantics_without_other_indexes() {
        for inverse in [false, true] {
            let mut list = GeoIpList::decode(data(inverse).as_slice()).unwrap();
            let mut unused = list.entry[0].clone();
            unused.country_code = "unused".into();
            list.entry.push(unused);
            let bytes = list.encode_to_vec();
            let full = Database::decode(&bytes).unwrap();
            let selected =
                Database::decode_selected(&bytes, &["TeLeGrAm".into(), "telegram".into()]).unwrap();
            assert_eq!(selected.groups.len(), 1);
            assert!(selected.group("unused").is_err());
            for ip in [
                "149.154.160.1",
                "::ffff:149.154.160.1",
                "2001:db8::1",
                "149.154.176.1",
                "2001:db9::1",
            ] {
                let ip = ip.parse().unwrap();
                assert_eq!(
                    selected.matches("TELEGRAM", ip),
                    full.matches("TELEGRAM", ip)
                );
            }
            assert!(Database::decode_selected(&bytes, &["missing".into()]).is_err());
            assert!(Database::decode_selected(&bytes, &["bad/name".into()]).is_err());
            assert!(Database::decode_selected(&bytes, &[])
                .unwrap()
                .groups
                .is_empty());
        }
    }

    #[test]
    fn selected_loading_still_validates_unselected_groups() {
        let original = GeoIpList::decode(data(false).as_slice()).unwrap();
        for (name, ip, prefix) in [
            ("TELEGRAM", vec![1, 2, 3, 4], 24), // duplicate, including case
            ("bad/name", vec![1, 2, 3, 4], 24),
            ("unused", vec![1, 2, 3, 4], 33),
            ("unused", vec![0; 16], 129),
            ("unused", vec![1, 2, 3], 24),
        ] {
            let mut list = original.clone();
            list.entry.push(GeoIp {
                country_code: name.into(),
                inverse_match: false,
                cidr: vec![Cidr { ip, prefix }],
            });
            assert!(
                Database::decode_selected(&list.encode_to_vec(), &["telegram".into()]).is_err(),
                "{name}, prefix {prefix}"
            );
        }
    }

    #[test]
    fn streamed_messages_handle_unknown_fields_order_and_truncation() {
        // CIDR precedes metadata, and unknown fields occur at each level.
        let mut cidr = Cidr {
            ip: vec![10, 0, 0, 0],
            prefix: 8,
        }
        .encode_to_vec();
        cidr.extend_from_slice(&[0x48, 1]);
        let mut group = vec![0x12, cidr.len() as u8];
        group.extend(cidr);
        group.extend_from_slice(&[0x0a, 2, b'C', b'N', 0x18, 1, 0x48, 1]);
        let mut bytes = vec![0x0a, group.len() as u8];
        bytes.extend(group);
        let end = bytes.len();
        bytes.extend_from_slice(&[0x48, 1]);
        let db = Database::decode_selected(&bytes, &["cn".into()]).unwrap();
        assert!(!db.matches("CN", "10.1.2.3".parse().unwrap()));
        assert!(db.matches("CN", "11.1.2.3".parse().unwrap()));
        for len in 0..end {
            assert!(Database::decode_selected(&bytes[..len], &["cn".into()]).is_err());
        }
        for bytes in [vec![0x08, 1], vec![0x0a, 0xff], vec![0x0a, 2, 0x10, 1]] {
            assert!(Database::decode_selected(&bytes, &[]).is_err());
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
