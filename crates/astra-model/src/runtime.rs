//! Runtime controls and bounded telemetry, independent of source configuration.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Rule,
    Global,
    Direct,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Overrides {
    pub mode: Mode,
    pub selected: BTreeMap<String, String>,
    pub system_proxy: Option<bool>,
    pub tun: Option<bool>,
    pub allow_lan: Option<bool>,
}

impl Default for Overrides {
    fn default() -> Self {
        Self {
            mode: Mode::Rule,
            selected: BTreeMap::new(),
            system_proxy: Some(false),
            tun: Some(false),
            allow_lan: Some(false),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Command {
    Snapshot,
    SetMode(Mode),
    Select { group: String, member: String },
    TestGroup(String),
    Disconnect(Option<u64>),
    SystemProxy(bool),
    Tun(bool),
    Reset,
    AllowLan(bool),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Snapshot {
    pub pid: u32,
    pub mode: Mode,
    pub system_proxy: bool,
    pub tun: bool,
    pub allow_lan: bool,
    pub upload: u64,
    pub download: u64,
    pub proxy_upload: u64,
    pub proxy_download: u64,
    pub active: usize,
    pub total: u64,
    pub truncated: bool,
    pub connections: Vec<Connection>,
    pub groups: Vec<Group>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Connection {
    pub id: u64,
    pub source: Option<String>,
    pub host: String,
    pub port: u16,
    pub network: String,
    pub inbound: String,
    pub outbound: String,
    pub upload: u64,
    pub download: u64,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    pub name: String,
    pub mode: String,
    pub selected: Option<String>,
    pub testing: bool,
    pub members: Vec<Member>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Member {
    pub name: String,
    pub delay_ms: Option<u64>,
    /// None means not tested, false means the latest probe failed.
    pub alive: Option<bool>,
}

#[cfg(test)]
#[path = "test/runtime_mode_state_test.rs"]
mod mode_state_tests;

/// Additive JSON telemetry. Deliberately not part of the bincode runtime Snapshot.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeView {
    #[serde(flatten)]
    pub snapshot: Snapshot,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry: Option<RuntimeTelemetry>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Coverage {
    #[default]
    Unknown,
    Complete,
    Partial,
    Stale,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeTelemetry {
    pub host_boot_id: String,
    pub active_generation: Option<u64>,
    pub ingress_paused: bool,
    pub sample_sequence: u64,
    /// Milliseconds on the host's monotonic clock, not wall-clock timestamps.
    pub sample_elapsed_ms: u64,
    pub sample_age_ms: u64,
    pub coverage: Coverage,
    pub generations: Vec<GenerationTelemetry>,
    pub connection_generations: std::collections::BTreeMap<u64, u64>,
    /// Keep optional history off the stack of frontend event/operation futures.
    pub traffic: Option<Box<TrafficTelemetry>>,
    /// Bounded, JSON-only query history; absent on older cores.
    pub dns: Option<Box<DnsQueryLog>>,
    /// Effective resolver configuration of the active worker, not a saved draft.
    pub dns_resolver: Option<Box<DnsResolverStatus>>,
    /// Actual captured paths and side-effect-free group previews. JSON-only.
    pub chains: Option<Box<ChainTelemetry>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DnsResolverStatus {
    pub enabled: bool,
    pub fake_ip: bool,
    pub ipv6: bool,
    pub respect_rules: bool,
    pub listen: Vec<std::net::SocketAddr>,
    pub nameservers: Vec<DnsServer>,
    pub fallback: Vec<DnsServer>,
    pub proxy_server_nameserver: Vec<DnsServer>,
    pub policy_count: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DnsServer {
    pub address: String,
    pub transport: String,
}

impl DnsResolverStatus {
    pub fn from_config(config: Option<&crate::config::dns::Config>) -> Self {
        let Some(config) = config else {
            return Self::default();
        };
        let servers = |values: &[String]| {
            values
                .iter()
                .map(|address| {
                    let transport = crate::config::dns::Upstream::parse(address)
                        .map(|server| match server.scheme.as_str() {
                            "udp" => "UDP",
                            "tcp" => "TCP",
                            "tls" => "DoT",
                            "https" => "DoH",
                            _ => "Unknown",
                        })
                        .unwrap_or("Unknown");
                    DnsServer {
                        address: address.clone(),
                        transport: transport.into(),
                    }
                })
                .collect()
        };
        Self {
            enabled: config.enable,
            fake_ip: config.fake_ip,
            ipv6: config.ipv6,
            respect_rules: config.respect_rules,
            listen: config.listen.clone(),
            nameservers: servers(&config.nameserver),
            fallback: servers(&config.fallback),
            proxy_server_nameserver: servers(&config.proxy_server_nameserver),
            policy_count: config.nameserver_policy.len(),
        }
    }
}

pub const DNS_QUERY_LIMIT: usize = 128;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DnsQueryLog {
    /// Most recently completed queries first.
    pub records: Vec<DnsQuery>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DnsQuery {
    /// Resolver lifetime plus sequence: remains unambiguous across reloads.
    pub resolver: String,
    pub id: u64,
    pub timestamp_ms: u64,
    pub domain: String,
    pub query_type: String,
    pub source: String,
    pub upstream: Option<String>,
    pub status: String,
    pub answers: Vec<String>,
    pub answers_truncated: bool,
    pub duration_us: u64,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TrafficTelemetry {
    /// Per resolved outbound, including closed flows. JSON-only extension.
    pub nodes: Option<BTreeMap<String, NodeTraffic>>,
    pub history_error: Option<String>,
    pub selected_node: Option<String>,
}

/// Upload + download bytes. Rolling windows use minute buckets; one month is 30 days.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct NodeTraffic {
    pub session: u64,
    pub hour: u64,
    pub day: u64,
    pub week: u64,
    pub month: u64,
    pub persist: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GenerationTelemetry {
    pub generation: u64,
    pub sample_age_ms: Option<u64>,
    pub healthy: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ChainTelemetry {
    pub connections: BTreeMap<u64, ChainPath>,
    pub previews: BTreeMap<String, ChainPath>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ChainPath {
    pub hops: Vec<ChainHop>,
    pub groups: Vec<String>,
    pub status: String,
    pub issue: Option<String>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ChainHop {
    pub name: String,
    pub protocol: String,
}
