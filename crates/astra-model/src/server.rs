//! Server control DTO shared by cores and management clients.
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
pub const VERSION: u16 = 1;
#[derive(Serialize, Deserialize)]
pub enum Action {
    Status,
    Stop,
    Reload { path: Option<PathBuf> },
    Logs,
}
#[derive(Serialize, Deserialize)]
pub struct Request {
    pub version: u16,
    pub action: Action,
}
#[derive(Default, Serialize, Deserialize)]
pub struct Reply {
    pub version: u16,
    pub error: Option<String>,
    pub pid: u32,
    pub uptime_secs: u64,
    pub active: u64,
    pub accepted: u64,
    pub rejected: u64,
    pub upload: u64,
    pub download: u64,
    pub config: PathBuf,
    pub listeners: Vec<String>,
    pub logs: String,
}
