//! V2Ray GeoIPList protobuf and indexed IP category matching.
use anyhow::{ensure, Context, Result};
use prost::{
    encoding::{decode_key, decode_varint, skip_field, DecodeContext, WireType},
    Message,
};
use std::{
    collections::{HashMap, HashSet},
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

pub use astra_model::config::ruleset::geoip_category as category;
#[derive(Debug)]
pub struct Group {
    v4: Vec<(u32, u32)>,
    v6: Vec<(u128, u128)>,
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
fn merge_ranges<T: Copy + Ord>(mut ranges: Vec<(T, T)>, next: impl Fn(T) -> T) -> Vec<(T, T)> {
    ranges.sort_unstable();
    let mut merged: Vec<(T, T)> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        if let Some(last) = merged.last_mut() {
            if start <= next(last.1) {
                last.1 = last.1.max(end);
                continue;
            }
        }
        merged.push((start, end));
    }
    merged.shrink_to_fit();
    merged
}
fn contains<T: Copy + Ord>(ranges: &[(T, T)], ip: T) -> bool {
    let Some(&(last_start, last_end)) = ranges.last() else {
        return false;
    };
    // Most unrelated destinations lie outside a category's bounds. Avoid
    // searching the whole index for those, or for its final interval.
    if ip >= last_start {
        return ip <= last_end;
    }
    if ip < ranges[0].0 {
        return false;
    }
    let after = ranges.partition_point(|(start, _)| *start <= ip);
    ip <= ranges[after - 1].1
}
impl Group {
    pub fn contains(&self, ip: IpAddr) -> bool {
        let found = match ip.to_canonical() {
            IpAddr::V4(ip) => contains(&self.v4, u32::from(ip)),
            IpAddr::V6(ip) => contains(&self.v6, u128::from(ip)),
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
            let mut v4 = Vec::new();
            let mut v6 = Vec::new();
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
                            let start = u32::from(ip) & mask4(bits);
                            v4.push((start, start | !mask4(bits)));
                        }
                    }
                    16 => {
                        ensure!(cidr.prefix <= 128, "invalid IPv6 prefix in GEOIP {key}");
                        if retain {
                            let ip = Ipv6Addr::from(<[u8; 16]>::try_from(cidr.ip.as_slice())?);
                            let bits = cidr.prefix as u8;
                            let start = u128::from(ip) & mask6(bits);
                            v6.push((start, start | !mask6(bits)));
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
                        v4: merge_ranges(v4, |end| end.saturating_add(1)),
                        v6: merge_ranges(v6, |end| end.saturating_add(1)),
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
#[path = "test/geoip_test.rs"]
mod tests;
