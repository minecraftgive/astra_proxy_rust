use serde::{Deserialize, Serialize};
use std::net::IpAddr;

pub const FAKE_IP_LOOKUP_CAPABILITY: &str = "fake_ip_lookup_v1";
pub const NETWORK_DIAGNOSTICS_STREAM_CAPABILITY: &str = "network_diagnostics_stream_v1";
pub const NETWORK_DIAGNOSTICS_PINNED_CAPABILITY: &str = "network_diagnostics_pinned_v1";
pub const NETWORK_DIAGNOSTICS_RESOLUTION_CAPABILITY: &str = "network_diagnostics_resolution_v1";
pub const NETWORK_DIAGNOSTICS_TOOLS_CAPABILITY: &str = "network_diagnostics_tools_v1";
pub const NETWORK_DIAGNOSTICS_NAT_CAPABILITY: &str = "network_diagnostics_nat_v1";
pub const NETWORK_DIAGNOSTICS_PATH_CAPABILITY: &str = "network_diagnostics_path_v1";
pub const MAX_FAKE_IP_LOOKUP: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FakeIpMapping {
    pub address: IpAddr,
    /// None means no live mapping was found, never that an address range proves Fake-IP use.
    pub domain: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FakeIpLookupResponse {
    pub version: u16,
    pub boot_id: String,
    pub outcome: FakeIpLookupOutcome,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FakeIpLookupOutcome {
    Mappings(Vec<FakeIpMapping>),
    Unsupported,
    InvalidRequest(String),
    CoreChanged,
    Failed(String),
}
