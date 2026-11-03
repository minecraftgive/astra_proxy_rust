//! Runtime controls and bounded telemetry, independent of source configuration.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Rule,
    Global,
    Direct,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Overrides {
    pub mode: Mode,
    pub selected: BTreeMap<String, String>,
    pub system_proxy: Option<bool>,
    pub tun: Option<bool>,
    pub allow_lan: Option<bool>,
}

impl Default for Overrides {
    fn default() -> Self {
        Self {
            mode: Mode::Rule,
            selected: BTreeMap::new(),
            system_proxy: Some(false),
            tun: Some(false),
            allow_lan: Some(false),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Command {
    Snapshot,
    SetMode(Mode),
    Select { group: String, member: String },
    TestGroup(String),
    Disconnect(Option<u64>),
    SystemProxy(bool),
    Tun(bool),
    Reset,
    AllowLan(bool),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Snapshot {
    pub pid: u32,
    pub mode: Mode,
    pub system_proxy: bool,
    pub tun: bool,
    pub allow_lan: bool,
    pub upload: u64,
    pub download: u64,
    pub proxy_upload: u64,
    pub proxy_download: u64,
    pub active: usize,
    pub total: u64,
    pub truncated: bool,
    pub connections: Vec<Connection>,
    pub groups: Vec<Group>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Connection {
    pub id: u64,
    pub source: Option<String>,
    pub host: String,
    pub port: u16,
    pub network: String,
    pub inbound: String,
    pub outbound: String,
    pub upload: u64,
    pub download: u64,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    pub name: String,
    pub mode: String,
    pub selected: Option<String>,
    pub testing: bool,
    pub members: Vec<Member>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Member {
    pub name: String,
    pub delay_ms: Option<u64>,
    /// None means not tested, false means the latest probe failed.
    pub alive: Option<bool>,
}

#[cfg(test)]
mod mode_state_tests {
    use super::*;
    #[test]
    fn switches_default_off_and_are_serialized_separately_from_profiles() {
        let state = Overrides::default();
        assert_eq!(state.system_proxy, Some(false));
        assert_eq!(state.tun, Some(false));
        assert_eq!(state.allow_lan, Some(false));
        let decoded: Overrides = serde_json::from_str("{}").unwrap();
        assert_eq!(decoded.system_proxy, Some(false));
        assert_eq!(decoded.tun, Some(false));
        let saved = serde_json::to_string(&Overrides {
            system_proxy: Some(true),
            tun: Some(true),
            allow_lan: Some(true),
            ..Default::default()
        })
        .unwrap();
        let restored: Overrides = serde_json::from_str(&saved).unwrap();
        assert_eq!(restored.system_proxy, Some(true));
        assert_eq!(restored.tun, Some(true));
    }
}

/// Additive JSON telemetry. Deliberately not part of the bincode runtime Snapshot.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeView {
    #[serde(flatten)]
    pub snapshot: Snapshot,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry: Option<RuntimeTelemetry>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Coverage {
    #[default]
    Unknown,
    Complete,
    Partial,
    Stale,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeTelemetry {
    pub host_boot_id: String,
    pub active_generation: Option<u64>,
    pub ingress_paused: bool,
    pub sample_sequence: u64,
    /// Milliseconds on the host's monotonic clock, not wall-clock timestamps.
    pub sample_elapsed_ms: u64,
    pub sample_age_ms: u64,
    pub coverage: Coverage,
    pub generations: Vec<GenerationTelemetry>,
    pub connection_generations: std::collections::BTreeMap<u64, u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GenerationTelemetry {
    pub generation: u64,
    pub sample_age_ms: Option<u64>,
    pub healthy: bool,
}
