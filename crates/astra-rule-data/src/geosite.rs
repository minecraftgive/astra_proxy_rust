//! V2Ray GeoSiteList protobuf schema and selector expansion.
use crate::MatchSpec;
use anyhow::{ensure, Context, Result};
use prost::{
    encoding::{decode_key, decode_varint, skip_field, DecodeContext, WireType},
    Message,
};
use std::collections::{HashMap, HashSet};

#[derive(Clone, PartialEq, Message)]
pub struct GeoSiteList {
    #[prost(message, repeated, tag = "1")]
    pub entry: Vec<GeoSite>,
}
#[derive(Clone, PartialEq, Message)]
pub struct GeoSite {
    #[prost(string, tag = "1")]
    pub country_code: String,
    #[prost(message, repeated, tag = "2")]
    pub domain: Vec<Domain>,
}
#[derive(Clone, PartialEq, Message)]
pub struct Domain {
    #[prost(int32, tag = "1")]
    pub kind: i32,
    #[prost(string, tag = "2")]
    pub value: String,
    #[prost(message, repeated, tag = "3")]
    pub attribute: Vec<Attribute>,
}
#[derive(Clone, PartialEq, Message)]
pub struct Attribute {
    #[prost(string, tag = "1")]
    pub key: String,
    #[prost(oneof = "attribute::Value", tags = "2,3")]
    pub value: Option<attribute::Value>,
}
pub mod attribute {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Value {
        #[prost(bool, tag = "2")]
        Bool(bool),
        #[prost(int64, tag = "3")]
        Int(i64),
    }
}

struct CompactDomain {
    kind: u8,
    value: Box<str>,
    attributes: Box<[Box<str>]>,
}

pub struct Database {
    groups: HashMap<Box<str>, Vec<CompactDomain>>,
}

#[derive(Clone, PartialEq, Message)]
struct GroupMetadata {
    #[prost(string, tag = "1")]
    country_code: String,
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
                "invalid GEOSITE message wire type"
            );
            let len = decode_varint(&mut bytes)?;
            ensure!(len <= bytes.len() as u64, "truncated GEOSITE message");
            let (message, rest) = bytes.split_at(len as usize);
            visit(message)?;
            bytes = rest;
        } else {
            skip_field(wire, tag, &mut bytes, DecodeContext::default())?;
        }
    }
    Ok(())
}

/// Group names may contain ! (e.g. geolocation-!cn); only an attribute's
/// leading ! negates its presence. Multiple attributes are intersected.
pub use astra_model::config::ruleset::geosite_selector as selector;
impl Database {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        Self::decode_filtered(bytes, None)
    }

    /// Validate every group and domain, retaining only groups used by the selectors.
    pub fn decode_selected(bytes: &[u8], groups: &[String]) -> Result<Self> {
        let selected = groups
            .iter()
            .map(|name| selector(name).map(|(group, _)| group))
            .collect::<Result<HashSet<_>>>()?;
        // Missing groups are diagnosed when a selector is expanded. This keeps
        // selector validation in rule order: an earlier malformed regex must
        // not be hidden by a later missing group treated as unavailable.
        Self::decode_filtered(bytes, Some(&selected))
    }

    fn decode_filtered(bytes: &[u8], selected: Option<&HashSet<String>>) -> Result<Self> {
        let mut groups = HashMap::new();
        let mut seen = HashSet::new();
        let mut count = 0usize;
        let mut domain = Domain::default();
        visit_messages(bytes, 1, |bytes| {
            let metadata = GroupMetadata::decode(bytes)?;
            let key = metadata.country_code.to_ascii_lowercase();
            ensure!(
                !key.is_empty() && !key.contains('@'),
                "invalid geosite database group name"
            );
            ensure!(seen.insert(key.clone()), "duplicate geosite group");
            let retain = selected.is_none_or(|names| names.contains(&key));
            let mut domains = Vec::new();
            visit_messages(bytes, 2, |bytes| {
                count += 1;
                ensure!(count <= 5_000_000, "geosite database exceeds domain limit");
                domain.clear();
                domain.merge(bytes)?;
                ensure!(
                    (0..=3).contains(&domain.kind),
                    "unknown GEOSITE domain type: {}",
                    domain.kind
                );
                ensure!(
                    !domain.value.is_empty()
                        && domain.value.len() <= if domain.kind == 1 { 4096 } else { 253 },
                    "invalid GEOSITE domain value"
                );
                if retain {
                    domains.push(CompactDomain {
                        kind: domain.kind as u8,
                        value: std::mem::take(&mut domain.value).into_boxed_str(),
                        attributes: domain
                            .attribute
                            .drain(..)
                            .map(|attr| attr.key.into_boxed_str())
                            .collect(),
                    });
                }
                Ok(())
            })?;
            if retain {
                domains.shrink_to_fit();
                groups.insert(key.into_boxed_str(), domains);
            }
            Ok(())
        })
        .context("invalid geosite.dat protobuf")?;
        ensure!(!seen.is_empty(), "geosite.dat contains no groups");
        groups.shrink_to_fit();
        Ok(Self { groups })
    }

    /// Visit selected entries without allocating an intermediate MatchSpec list.
    pub fn expand_with(
        &self,
        input: &str,
        mut visit: impl FnMut(&'static str, &str) -> Result<()>,
    ) -> Result<()> {
        let (group, filters) = selector(input)?;
        let domains = self
            .groups
            .get(group.as_str())
            .with_context(|| format!("GEOSITE group not found: {group}"))?;
        for domain in domains.iter().filter(|domain| {
            filters.iter().all(|(key, negated)| {
                let present = domain
                    .attributes
                    .iter()
                    .any(|attr| attr.eq_ignore_ascii_case(key));
                present != *negated
            })
        }) {
            let kind = match domain.kind {
                0 => "domain_keyword",
                1 => "domain_regex",
                2 => "domain_suffix",
                3 => "domain_exact",
                _ => unreachable!("domain types are validated during decoding"),
            };
            visit(kind, &domain.value)?;
        }
        Ok(())
    }

    pub fn expand(&self, input: &str) -> Result<Vec<MatchSpec>> {
        let mut entries = Vec::new();
        self.expand_with(input, |kind, value| {
            entries.push(MatchSpec {
                kind: kind.into(),
                value: value.into(),
            });
            Ok(())
        })?;
        Ok(entries)
    }
}

#[cfg(test)]
#[path = "test/geosite_test.rs"]
mod tests;
