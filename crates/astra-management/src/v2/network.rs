//! Versioned diagnostic reports, deliberately independent of the strict v2 Task schema.
use serde::{Deserialize, Serialize};
#[cfg(feature = "bindings")]
use ts_rs::TS;

pub const NETWORK_DIAGNOSTICS_CAPABILITY: &str = "network_diagnostics_jobs_v1";
pub const NETWORK_BATCH_CAPABILITY: &str = "network_diagnostics_batch_v1";

/// One accepted, cancellable batch. Its 180-second deadline includes admission
/// and NAT scheduling. No comparison traffic is implicit.
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkBatchRequest {
    pub projects: Vec<NetworkBatchProject>,
    #[serde(default)]
    pub comparison: bool,
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkBatchProject {
    pub id: String,
    pub path_id: String,
    pub request: super::NetworkDiagnosticsRequest,
}
pub const NETWORK_REPORT_BYTES: usize = 2 * 1024 * 1024;
pub const NETWORK_PAGE_BYTES: usize = 64 * 1024;
pub const NETWORK_EVENT_BYTES: usize = 2 * 1024;
pub const NETWORK_EVENT_COUNT: usize = 256;
pub const NETWORK_RETENTION_BYTES: usize = 64 * 1024 * 1024;
pub const NETWORK_RETENTION_COUNT: usize = 32;
pub const NETWORK_RETENTION_MS: u64 = 7 * 24 * 60 * 60 * 1000;

#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkState {
    Queued,
    Running,
    Complete,
    Partial,
    Failed,
    Cancelled,
    Interrupted,
}
impl NetworkState {
    pub fn terminal(self) -> bool {
        !matches!(self, Self::Queued | Self::Running)
    }
}

#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkResult {
    pub id: String,
    pub sampled_at_ms: String,
    /// Core event; integer counters use decimal strings on the management wire.
    pub data: serde_json::Value,
}

#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkReport {
    pub operation_id: String,
    pub schema_version: u16,
    pub revision: String,
    pub state: NetworkState,
    pub message: String,
    pub started_ms: String,
    pub finished_ms: Option<String>,
    pub truncated: bool,
    /// The outcome and operation fingerprint survive expiry of the evidence.
    pub expired: bool,
    pub items: Vec<NetworkResult>,
    /// Resume from this cursor, including when the current page has no items yet.
    pub cursor: String,
    pub has_more: bool,
}
