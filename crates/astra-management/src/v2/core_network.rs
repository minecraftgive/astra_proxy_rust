use serde::{Deserialize, Serialize};
#[cfg(feature = "bindings")]
use ts_rs::TS;

#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoreQueryStatus {
    Ok,
    Unsupported,
    CommunicationError,
    ProtocolError,
    Unconfirmed,
    CoreError,
    InvalidRequest,
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkCoreIdentity {
    pub version: u16,
    pub boot_id: String,
    pub pid: u32,
    pub capabilities: Option<Vec<String>>,
    pub applied_digest: Option<String>,
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkCoreReply {
    pub schema_version: u16,
    pub instance: String,
    pub sampled_at_ms: String,
    pub status: CoreQueryStatus,
    pub message: Option<String>,
    pub identity: Option<NetworkCoreIdentity>,
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FakeIpMapping {
    pub address: String,
    pub domain: Option<String>,
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FakeIpLookupReply {
    pub schema_version: u16,
    pub instance: String,
    pub sampled_at_ms: String,
    pub status: CoreQueryStatus,
    pub message: Option<String>,
    pub boot_id: Option<String>,
    pub mappings: Vec<FakeIpMapping>,
}
