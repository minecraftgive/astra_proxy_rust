//! Observations from the target Agent's OS, distinct from core runtime and probes.
use serde::{Deserialize, Serialize};
#[cfg(feature = "bindings")]
use ts_rs::TS;

#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub schema_version: u16,
    pub agent_id: String,
    pub sampled_at_ms: String,
    pub dns_sampled_at_ms: String,
    pub interfaces: Vec<Interface>,
    pub dns: Vec<Resolver>,
    pub routes: Vec<Route>,
    pub issues: Vec<Issue>,
    pub truncated: bool,
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterfaceKind {
    Physical,
    Virtual,
    Loopback,
    Unknown,
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Interface {
    /// Stable only for the current Agent/OS interface lifetime; never a client-local ID.
    pub id: String,
    pub name: String,
    pub index: u32,
    pub kind: InterfaceKind,
    pub connected: bool,
    pub addresses: Vec<Address>,
    pub source: String,
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Address {
    pub address: String,
    pub prefix_len: Option<u8>,
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resolver {
    pub id: String,
    pub source: String,
    pub interface_id: Option<String>,
    pub scope: String,
    pub domains: Vec<String>,
    pub servers: Vec<String>,
    pub priority: Option<u32>,
    pub flags: Vec<String>,
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    pub destination: String,
    pub gateway: Option<String>,
    pub interface_id: Option<String>,
    pub flags: Vec<String>,
    pub source: String,
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Issue {
    pub component: String,
    pub message: String,
}
