//! Versioned path observations; no inferred topology or reachability from silence.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathContext {
    pub sampled_at_ms: String,
    pub core_tun: Option<bool>,
    pub physical: Option<PhysicalPathEvidence>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhysicalPathEvidence {
    pub interface_id: String,
    pub interface_name: String,
    pub interface_index: u32,
    pub source_address: String,
    pub family: String,
    pub sampled_at_ms: String,
    pub binding_verified: bool,
    pub route_verified: bool,
    pub target: Option<String>,
    pub method: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathOutcome {
    Reply,
    TimeExceeded,
    Unreachable,
    /// Silence proves neither a missing device nor a broken/monitored path.
    NoResponse,
    PermissionDenied,
    Unavailable,
    PathChanged,
    TunLocal,
    Unconfirmed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathCapability {
    pub supported: bool,
    pub method: String,
    pub permission_required: Option<String>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraceSample {
    pub hop: u8,
    pub probe: u8,
    pub address: Option<String>,
    pub rtt_ms: Option<f64>,
    pub outcome: PathOutcome,
    pub icmp_type: Option<u8>,
    pub icmp_code: Option<u8>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraceSummary {
    pub target: String,
    pub family: String,
    pub capability: PathCapability,
    pub physical: Option<PhysicalPathEvidence>,
    pub max_hops: u8,
    /// Probes whose native adapter confirmed submission; a failed adapter may
    /// stop after an unknown send state and is not counted as a timeout.
    pub probes_sent: u16,
    pub reached: bool,
    pub completed: bool,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EchoObservation {
    pub target: String,
    pub family: String,
    pub from: Option<String>,
    pub outcome: PathOutcome,
    pub rtt_ms: Option<f64>,
    pub ttl: Option<u8>,
    /// False for TUN-local or otherwise unconfirmed-path echo observations.
    pub proves_reachability: bool,
    pub tun_local: bool,
    pub physical: Option<PhysicalPathEvidence>,
    pub message: String,
}
