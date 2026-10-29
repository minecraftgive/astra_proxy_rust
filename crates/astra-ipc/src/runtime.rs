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
/// Load only private runtime state; source profiles never supply mutable switches.
pub fn load_overrides(profile: &std::path::Path) -> std::io::Result<Overrides> {
    use std::io::Read;
    let path = crate::storage::runtime_path(profile, "overrides")?;
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Overrides::default())
        }
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 1024 * 1024 {
        return Err(std::io::Error::other("runtime overrides exceed 1 MiB"));
    }
    let mut state: Overrides = serde_json::from_slice(&bytes)?;
    state.system_proxy.get_or_insert(false);
    state.tun.get_or_insert(false);
    state.allow_lan.get_or_insert(false);
    Ok(state)
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
