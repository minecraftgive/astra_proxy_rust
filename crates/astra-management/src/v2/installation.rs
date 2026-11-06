use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};
use ts_rs::TS;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum InstallationScope {
    #[default]
    User,
    System,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct InstallationDestination {
    pub directory: Option<PathBuf>,
    pub entry: Option<PathBuf>,
    pub installed: bool,
    pub requires_elevation: bool,
    pub unavailable_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct InstallationPackage {
    pub id: String,
    pub role: String,
    pub release_id: String,
    pub source: PathBuf,
    pub user: InstallationDestination,
    pub system: InstallationDestination,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct InstallationStatus {
    pub target: String,
    pub account: String,
    pub packages: Vec<InstallationPackage>,
    pub unavailable_roles: BTreeMap<String, String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct InstallationResult {
    pub role: String,
    pub directory: PathBuf,
    pub entry: PathBuf,
    pub already_installed: bool,
    pub message: String,
    pub launch_command: String,
}

/// Only returned through the target machine's private local IPC. No browser
/// or remote controller may request a local interactive elevation launch.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct InstallationLaunch {
    pub program: PathBuf,
    pub arguments: Vec<String>,
}
