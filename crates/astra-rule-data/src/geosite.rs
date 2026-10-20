//! V2Ray GeoSiteList protobuf schema and selector expansion.
use crate::MatchSpec;
use anyhow::{ensure, Context, Result};
use prost::Message;
use std::collections::HashMap;

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

pub struct Database {
    groups: HashMap<String, Vec<Domain>>,
}

/// Group names may contain ! (e.g. geolocation-!cn); only an attribute's
/// leading ! negates its presence. Multiple attributes are intersected.
pub fn selector(input: &str) -> Result<(String, Vec<(String, bool)>)> {
    let mut parts = input.split('@');
    let group = parts.next().unwrap_or_default().to_ascii_lowercase();
    let valid = |value: &str| {
        !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.!".contains(&b))
    };
    ensure!(valid(&group), "invalid GEOSITE group: {input}");
    let filters = parts
        .map(|part| {
            let (part, negated) = part.strip_prefix('!').map_or((part, false), |s| (s, true));
            ensure!(
                valid(part) && !part.contains('!'),
                "invalid GEOSITE attribute: {input}"
            );
            Ok((part.to_ascii_lowercase(), negated))
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(filters.len() <= 16, "too many GEOSITE attribute filters");
    Ok((group, filters))
}
impl Database {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let list = GeoSiteList::decode(bytes).context("invalid geosite.dat protobuf")?;
        ensure!(!list.entry.is_empty(), "geosite.dat contains no groups");
        let mut groups = HashMap::new();
        let mut count = 0usize;
        for group in list.entry {
            let key = group.country_code.to_ascii_lowercase();
            ensure!(
                !key.is_empty() && !key.contains('@'),
                "invalid geosite database group name"
            );
            count += group.domain.len();
            ensure!(count <= 5_000_000, "geosite database exceeds domain limit");
            ensure!(
                groups.insert(key, group.domain).is_none(),
                "duplicate geosite group"
            );
        }
        Ok(Self { groups })
    }
    pub fn expand(&self, input: &str) -> Result<Vec<MatchSpec>> {
        let (group, filters) = selector(input)?;
        let domains = self
            .groups
            .get(&group)
            .with_context(|| format!("GEOSITE group not found: {group}"))?;
        domains
            .iter()
            .filter(|domain| {
                filters.iter().all(|(key, negated)| {
                    let present = domain
                        .attribute
                        .iter()
                        .any(|attr| attr.key.eq_ignore_ascii_case(key));
                    present != *negated
                })
            })
            .map(|domain| {
                let kind = match domain.kind {
                    0 => "domain_keyword",
                    1 => "domain_regex",
                    2 => "domain_suffix",
                    3 => "domain_exact",
                    other => anyhow::bail!("unknown GEOSITE domain type: {other}"),
                };
                ensure!(
                    !domain.value.is_empty()
                        && domain.value.len() <= if domain.kind == 1 { 4096 } else { 253 },
                    "invalid GEOSITE domain value"
                );
                Ok(MatchSpec {
                    kind: kind.into(),
                    value: domain.value.clone(),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn database() -> Database {
        Database::decode(include_bytes!("../../astra-rule/tests/data/geosite.dat")).unwrap()
    }
    #[test]
    fn selectors_preserve_group_names_and_filter_attributes() {
        let db = database();
        assert_eq!(db.expand("TeSt").unwrap().len(), 4);
        assert_eq!(db.expand("test@cn").unwrap().len(), 2);
        assert_eq!(db.expand("test@!cn").unwrap().len(), 2);
        assert_eq!(db.expand("test@cn@fast").unwrap().len(), 1);
        assert_eq!(db.expand("test@missing").unwrap().len(), 0);
        assert_eq!(
            db.expand("geolocation-!cn").unwrap()[0].value,
            "abroad.test"
        );
        assert!(db.expand("missing").is_err());
        for value in [
            "", "@cn", "test@", "test@!", "test a", "test@cn!", "test@@cn",
        ] {
            assert!(selector(value).is_err(), "{value}");
        }
    }
    #[test]
    fn all_domain_types_and_invalid_database_are_handled() {
        assert_eq!(
            database()
                .expand("test")
                .unwrap()
                .iter()
                .map(|rule| rule.kind.as_str())
                .collect::<Vec<_>>(),
            [
                "domain_exact",
                "domain_suffix",
                "domain_keyword",
                "domain_regex"
            ]
        );
        assert!(Database::decode(b"invalid protobuf").is_err());
        assert!(Database::decode(b"").is_err());
        let empty = GeoSiteList {
            entry: vec![GeoSite {
                country_code: "empty".into(),
                domain: vec![],
            }],
        };
        assert!(Database::decode(&empty.encode_to_vec())
            .unwrap()
            .expand("empty")
            .unwrap()
            .is_empty());
    }
}
