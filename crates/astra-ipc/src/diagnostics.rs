//! Result data shared by the core and manager; no credentials or configuration text.
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Probe {
    pub outbound: String,
    pub direct: bool,
    pub connect_ms: Option<u64>,
    pub elapsed_ms: u64,
    pub ip: Option<std::net::IpAddr>,
    pub error: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub url: String,
    pub running_config: bool,
    pub probes: Vec<Probe>,
}
