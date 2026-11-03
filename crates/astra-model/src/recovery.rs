//! Credential-free outcome of a network cleanup attempt.
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryState {
    Recovered,
    Pending,
    Conflict,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notice {
    pub state: RecoveryState,
    pub timestamp_ms: u64,
    pub message: String,
    pub journal: PathBuf,
    pub boot_id: String,
}
