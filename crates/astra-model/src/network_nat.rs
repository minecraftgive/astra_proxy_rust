//! Diagnostic UDP/STUN evidence. A proxy STUN observation is never a NAT classification.
pub use crate::nettool::AddressFamily;
use serde::{Deserialize, Serialize};

#[cfg_attr(feature = "bindings", derive(ts_rs::TS), ts(rename = "NatRequest"))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub agent_id: String,
    pub interface_id: String,
    pub family: AddressFamily,
    pub servers: Vec<String>,
    pub advanced: bool,
}
impl Request {
    pub fn validate(&self) -> Result<(), String> {
        if self.agent_id.trim().is_empty()
            || self.agent_id.len() > 256
            || self.agent_id.chars().any(char::is_control)
        {
            return Err("NAT requires its target Agent identity".into());
        }
        if self.interface_id.trim().is_empty()
            || self.interface_id.len() > 256
            || self.interface_id.chars().any(char::is_control)
        {
            return Err("请选择物理网卡 / Select a physical interface".into());
        }
        if self.servers.is_empty()
            || self.servers.len() > 4
            || self.servers.iter().any(|s| {
                s.is_empty()
                    || s.len() > 1024
                    || s.chars().any(char::is_whitespace)
                    || s.chars().any(char::is_control)
                    || s.contains(['@', '?', '#'])
                    || s.strip_prefix("stun://")
                        .or_else(|| s.strip_prefix("stun:"))
                        .unwrap_or(s)
                        .contains('/')
            })
        {
            return Err("NAT requires 1–4 bounded STUN endpoints".into());
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Behavior {
    #[default]
    Unknown,
    EndpointIndependent,
    AddressDependent,
    AddressAndPortDependent,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Comparison {
    #[default]
    Insufficient,
    Consistent,
    Different,
    Unstable,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    pub phase: String,
    pub server: String,
    pub expected_source: String,
    pub source: Option<String>,
    pub local_endpoint: String,
    pub mapped_endpoint: Option<String>,
    pub other_address: Option<String>,
    pub response_origin: Option<String>,
    pub transaction_id: String,
    pub response_received: bool,
    pub attempts: u32,
    pub discarded_packets: u32,
    pub elapsed_ms: u64,
    pub error: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub interface_id: String,
    pub interface_name: String,
    pub family: AddressFamily,
    pub local_endpoint: Option<String>,
    pub physical_path: Option<crate::network_path::PhysicalPathEvidence>,
    pub comparison: Comparison,
    pub mapping: Behavior,
    pub filtering: Behavior,
    /// Optional traditional name backed by complete, stable behavior tests.
    /// Unusual mapping/filtering combinations intentionally have no such name.
    /// `no_translation_observed` makes no claim about filtering or reachability.
    pub traditional_type: Option<String>,
    pub advanced_capable: bool,
    pub unstable: bool,
    pub partial: bool,
    pub elapsed_ms: u64,
    pub observations: Vec<Observation>,
    pub notes: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    Observation { observation: Observation },
    Report { report: Report },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StunResult {
    pub server: String,
    pub egress: String,
    pub proxy_session: bool,
    pub observation: Observation,
    /// Intentionally no mapping/filtering classification on this tool.
    pub note: String,
}
