use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};
#[cfg(feature = "bindings")]
use ts_rs::TS;

#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallationScope {
    #[default]
    User,
    System,
}

#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallationDestination {
    pub directory: Option<PathBuf>,
    pub entry: Option<PathBuf>,
    pub installed: bool,
    pub requires_elevation: bool,
    pub unavailable_reason: Option<String>,
}

#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallationPackage {
    pub id: String,
    pub role: String,
    pub release_id: String,
    pub source: PathBuf,
    pub user: InstallationDestination,
    pub system: InstallationDestination,
}

#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallationStatus {
    pub target: String,
    pub account: String,
    pub packages: Vec<InstallationPackage>,
    pub unavailable_roles: BTreeMap<String, String>,
    pub warnings: Vec<String>,
}

#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallationResult {
    pub role: String,
    pub directory: PathBuf,
    pub entry: PathBuf,
    pub already_installed: bool,
    pub message: String,
    pub launch_command: String,
}

/// Ordered installation steps carried by `EventPayload::Installation`.
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallationStep {
    Preparing,
    Copying,
    Verifying,
    Publishing,
    Complete,
}

/// Structured progress for one `install_current_package` operation. Byte
/// counts are decimal strings and are non-zero only while copying.
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallationProgress {
    pub step: InstallationStep,
    pub step_index: u8,
    pub step_count: u8,
    pub current: String,
    pub total: String,
}

/// Only returned through the target machine's private local IPC. No browser
/// or remote controller may request a local interactive elevation launch.
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallationLaunch {
    pub program: PathBuf,
    pub arguments: Vec<String>,
}
