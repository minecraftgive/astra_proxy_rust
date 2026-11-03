//! Pure deployment declarations shared by the service and provisioning tools.
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Controller,
    #[default]
    Agent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(try_from = "Deployment", into = "Deployment")]
pub struct ManagerConfig {
    pub mode: Mode,
    pub state_dir: PathBuf,
    pub config_dir: PathBuf,
    pub bin_dir: PathBuf,
    pub controller: Option<ControllerConfig>,
    pub agent: AgentConfig,
    pub runtime_dir: Option<PathBuf>,
    pub local_ipc: LocalIpc,
    pub web: WebConfig,
    pub limits: Limits,
    pub config_file: Option<PathBuf>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Deployment {
    role: Mode,
    storage: Storage,
    controller: Option<ControllerConfig>,
    #[serde(default)]
    upstream: Upstream,
    #[serde(default)]
    local_ipc: LocalIpc,
    #[serde(default)]
    web: WebConfig,
    #[serde(default)]
    limits: Limits,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Storage {
    state_dir: PathBuf,
    config_dir: PathBuf,
    bin_dir: PathBuf,
    runtime_dir: Option<PathBuf>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Upstream {
    enabled: bool,
    agent_id: String,
    name: String,
    controller_url: Option<String>,
    credential_file: Option<PathBuf>,
    trust_root: Option<PathBuf>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControllerConfig {
    pub listen: SocketAddr,
    pub tls_cert: PathBuf,
    pub tls_key: PathBuf,
    /// A digest for each node; browser clients never receive node credentials.
    #[serde(default)]
    pub agent_token_hashes: BTreeMap<String, String>,
}
#[derive(Debug, Clone, Default)]
pub struct AgentConfig {
    pub id: String,
    pub name: String,
    pub controller_url: Option<String>,
    pub token_file: Option<PathBuf>,
    pub ca_certificate: Option<PathBuf>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalIpc {
    pub endpoint: Option<PathBuf>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub concurrent_tasks: usize,
    pub event_buffer: usize,
    pub upload_bytes: u64,
    pub ipc_connections: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            concurrent_tasks: 16,
            event_buffer: 1024,
            upload_bytes: 256 * 1024 * 1024,
            ipc_connections: 64,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebConfig {
    pub enabled: bool,
    pub listen: SocketAddr,
    pub public_origin: Option<String>,
    pub tls: Option<WebTls>,
    pub dev_http: bool,
    pub state_dir: Option<PathBuf>,
    pub binary: Option<PathBuf>,
    pub credentials_file: Option<PathBuf>,
}
impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen: "127.0.0.1:8787".parse().unwrap(),
            public_origin: None,
            tls: None,
            dev_http: false,
            state_dir: None,
            binary: None,
            credentials_file: None,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebTls {
    pub certificate: PathBuf,
    pub private_key: PathBuf,
}
impl Default for ManagerConfig {
    fn default() -> Self {
        Self {
            mode: Mode::Agent,
            state_dir: "state".into(),
            config_dir: "config".into(),
            bin_dir: ".".into(),
            controller: None,
            agent: AgentConfig {
                id: "local".into(),
                name: "Local host".into(),
                ..Default::default()
            },
            runtime_dir: None,
            local_ipc: LocalIpc::default(),
            web: WebConfig::default(),
            limits: Limits::default(),
            config_file: None,
        }
    }
}
impl TryFrom<Deployment> for ManagerConfig {
    type Error = String;
    fn try_from(value: Deployment) -> Result<Self, String> {
        if value.upstream.enabled && value.upstream.controller_url.is_none() {
            return Err("upstream.enabled requires controller_url".into());
        }
        let a = value.upstream;
        Ok(Self {
            mode: value.role,
            state_dir: value.storage.state_dir,
            config_dir: value.storage.config_dir,
            bin_dir: value.storage.bin_dir,
            runtime_dir: value.storage.runtime_dir,
            controller: value.controller,
            agent: AgentConfig {
                id: a.agent_id,
                name: a.name,
                controller_url: if a.enabled { a.controller_url } else { None },
                token_file: a.credential_file,
                ca_certificate: a.trust_root,
            },
            local_ipc: value.local_ipc,
            web: value.web,
            limits: value.limits,
            config_file: None,
        })
    }
}
impl From<ManagerConfig> for Deployment {
    fn from(value: ManagerConfig) -> Self {
        Self {
            role: value.mode,
            storage: Storage {
                state_dir: value.state_dir,
                config_dir: value.config_dir,
                bin_dir: value.bin_dir,
                runtime_dir: value.runtime_dir,
            },
            controller: value.controller,
            upstream: Upstream {
                enabled: value.agent.controller_url.is_some(),
                agent_id: value.agent.id,
                name: value.agent.name,
                controller_url: value.agent.controller_url,
                credential_file: value.agent.token_file,
                trust_root: value.agent.ca_certificate,
            },
            local_ipc: value.local_ipc,
            web: value.web,
            limits: value.limits,
        }
    }
}
impl ManagerConfig {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let mut result: Self = toml::from_str(&std::fs::read_to_string(path)?)?;
        result.resolve_paths(path.parent().unwrap_or(Path::new(".")));
        result.validate()?;
        result.config_file = Some(std::fs::canonicalize(path)?);
        Ok(result)
    }
    pub fn resolve_paths(&mut self, base: &Path) {
        fn resolve(base: &Path, p: &mut PathBuf) {
            if p.is_relative() {
                *p = base.join(&*p);
            }
        }
        for p in [&mut self.state_dir, &mut self.config_dir, &mut self.bin_dir] {
            resolve(base, p);
        }
        for p in [
            &mut self.runtime_dir,
            &mut self.local_ipc.endpoint,
            &mut self.agent.token_file,
            &mut self.agent.ca_certificate,
            &mut self.web.state_dir,
            &mut self.web.binary,
            &mut self.web.credentials_file,
        ]
        .into_iter()
        .flatten()
        {
            resolve(base, p);
        }
        if let Some(c) = &mut self.controller {
            resolve(base, &mut c.tls_cert);
            resolve(base, &mut c.tls_key);
        }
        if let Some(t) = &mut self.web.tls {
            resolve(base, &mut t.certificate);
            resolve(base, &mut t.private_key);
        }
    }
    pub fn endpoint(&self) -> PathBuf {
        self.local_ipc.endpoint.clone().unwrap_or_else(|| {
            self.runtime_dir
                .as_ref()
                .unwrap_or(&self.state_dir)
                .join("management.sock")
        })
    }
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.state_dir != self.config_dir,
            "state_dir and config_dir must differ"
        );
        anyhow::ensure!(
            (1..=1024).contains(&self.limits.concurrent_tasks)
                && (1..=1024).contains(&self.limits.ipc_connections),
            "task and IPC concurrency must be 1..1024"
        );
        anyhow::ensure!(
            (16..=100_000).contains(&self.limits.event_buffer)
                && (1..=4 * 1024 * 1024 * 1024u64).contains(&self.limits.upload_bytes),
            "invalid event or upload limit"
        );
        match self.mode {
            Mode::Controller => {
                let controller = self
                    .controller
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("controller configuration is required"))?;
                anyhow::ensure!(
                    controller
                        .agent_token_hashes
                        .values()
                        .all(|v| v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit())),
                    "agent_token_hashes must contain SHA-256 hexadecimal hashes"
                );
                anyhow::ensure!(self.agent.controller_url.is_none(),"controller cannot also have an upstream; deploy a local agent to manage this machine");
            }
            Mode::Agent => {
                anyhow::ensure!(
                    !self.agent.id.trim().is_empty() && self.agent.id.len() <= 80,
                    "stable upstream.agent_id is required"
                );
                if let Some(url) = &self.agent.controller_url {
                    anyhow::ensure!(url.starts_with("wss://"), "upstream requires wss://");
                    anyhow::ensure!(
                        self.agent.token_file.is_some(),
                        "upstream credential_file is required"
                    );
                }
            }
        }
        if self.web.enabled {
            anyhow::ensure!(
                self.web
                    .public_origin
                    .as_ref()
                    .is_some_and(|s| !s.is_empty()),
                "web.public_origin is required"
            );
            anyhow::ensure!(
                self.web.tls.is_some() || (self.web.dev_http && self.web.listen.ip().is_loopback()),
                "Web requires TLS; explicit dev_http is allowed only on loopback"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deployment_round_trip_and_safe_defaults() {
        let original = ManagerConfig::default();
        let encoded = toml::to_string(&original).unwrap();
        assert!(encoded.contains("role = \"agent\""));
        assert!(encoded.contains("[storage]"));
        assert!(encoded.contains("[upstream]"));
        let mut decoded: ManagerConfig = toml::from_str(&encoded).unwrap();
        assert!(decoded.validate().is_ok());
        assert!(!decoded.web.enabled);
        decoded.web.enabled = true;
        decoded.web.public_origin = Some("http://0.0.0.0:8787".into());
        decoded.web.dev_http = true;
        decoded.web.listen = "0.0.0.0:8787".parse().unwrap();
        assert!(decoded.validate().is_err());
    }
    #[test]
    fn unrecognized_deployment_is_not_silently_reset() {
        assert!(toml::from_str::<ManagerConfig>("role='agent'\nmode='agent'\n").is_err());
    }
}
