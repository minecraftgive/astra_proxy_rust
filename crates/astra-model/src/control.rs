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
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    Status(Status),
    Config(Result<ConfigSnapshot, String>),
    Explanation(Result<RouteExplanation, String>),
    Runtime(Result<crate::runtime::Snapshot, String>),
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Applied {
    pub path: PathBuf,
    pub digest: u64,
}
