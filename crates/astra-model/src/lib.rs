//! Pure shared domain models. No I/O, runtime, or application dependencies.
use serde::{Deserialize, Serialize};
pub mod config;
pub mod groups;
pub mod network_analysis;
pub mod network_nat;
pub mod network_path;
pub mod network_state;
pub mod network_tls;
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Hash, PartialEq, Eq)]
pub enum Network {
    Tcp,
    Udp,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Hash, PartialEq, Eq)]
pub enum Inbound {
    Socks5,
    HttpConnect,
    Tun,
    Tproxy,
    Redirect,
    Mixed,
}

#[derive(Debug, Clone, Serialize, Deserialize, Hash, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteRequest {
    /// Observed real destination retained alongside a DNS/SNI hostname.
    /// Synthetic Fake IP addresses must never populate this field.
    #[serde(default)]
    pub destination_ip: Option<std::net::IpAddr>,
    pub api_version: u16,
    pub id: u64,
    pub target: Target,
    pub network: Network,
    pub inbound: Inbound,
    pub process: Option<ProcessInfo>,
}

/// Observed socket owner, supplied by the local proxy, never by client headers.
#[derive(Debug, Clone, Serialize, Deserialize, Hash, PartialEq, Eq)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum RouteAction {
    Direct,
    Block,
    Proxy(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteResponse {
    pub revision: u64,
    pub api_version: u16,
    pub id: u64,
    pub action: RouteAction,
}

pub mod update;
/// Resident host/worker compatibility, shared with release metadata.
pub const HOST_PROTOCOL_VERSION: u16 = 1;

pub mod document;
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub enum CoreKind {
    Proxy,
    Rule,
    Server,
}
impl CoreKind {
    pub fn binary(self) -> &'static str {
        match (self, cfg!(windows)) {
            (Self::Proxy, true) => "astra-proxy.exe",
            (Self::Proxy, false) => "astra-proxy",
            (Self::Rule, true) => "astra-rule.exe",
            (Self::Rule, false) => "astra-rule",
            (Self::Server, true) => "astra-server-core.exe",
            (Self::Server, false) => "astra-server-core",
        }
    }
}

pub mod server;

pub mod control;
pub mod host_control;
pub mod logging;
pub mod nettool;
pub mod runtime;

pub mod diagnostics;
pub mod recovery;

pub mod monitor;
