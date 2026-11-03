//! Core management data contract, separate from transport.
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
#[derive(Debug, Serialize, Deserialize)]
pub enum Command {
    Status,
    Stop,
    Reload { path: Option<PathBuf> },
    SetLogLevel(crate::logging::LogLevel),
    GetConfig,
    ExplainRoute(crate::RouteRequest),
    UpdateSubscriptions,
    Runtime(crate::runtime::Command),
    Session,
    WatchRuntime { interval_ms: u64 },
    Identity,
    CommitService { boot_id: String },
    StopIdentified { boot_id: String, force: bool },
    Monitor,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    Status(Status),
    Config(Result<ConfigSnapshot, String>),
    Explanation(Result<RouteExplanation, String>),
    Runtime(Result<crate::runtime::Snapshot, String>),
    Identity(Identity),
    Monitor(Result<crate::monitor::InternalSnapshot, String>),
}

/// A process number alone is never a durable process identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub started: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    pub version: u16,
    pub boot_id: String,
    pub process: ProcessIdentity,
    pub applied: Option<Applied>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteExplanation {
    pub api_version: u16,
    pub pid: u32,
    pub revision: u64,
    pub config: PathBuf,
    pub request: crate::RouteRequest,
    pub action: crate::RouteAction,
    pub path: String,
    pub default_action: String,
    pub condition: Option<(String, String)>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ConfigSnapshot {
    pub api_version: u16,
    pub pid: u32,
    pub path: PathBuf,
    pub text: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub api_version: u16,
    pub command: Command,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Status {
    pub api_version: u16,
    pub pid: u32,
    pub uptime_secs: u64,
    pub error: Option<String>,
    pub applied: Option<Applied>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Applied {
    pub path: PathBuf,
    pub digest: u64,
}
