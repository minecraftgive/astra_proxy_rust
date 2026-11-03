use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Subscription {
    pub url: String,
    pub interval: u64,
    pub user_agent: String,
    /// Proxy group or outbound used by Proxy Core for subscription downloads.
    pub proxy: String,
    pub import_groups: bool,
    pub managed_nodes: Vec<String>,
    pub managed_groups: Vec<String>,
}
impl Default for Subscription {
    fn default() -> Self {
        Self {
            url: String::new(),
            interval: 3600,
            user_agent: "clash.meta/astra".into(),
            proxy: "PROXY".into(),
            import_groups: false,
            managed_nodes: vec![],
            managed_groups: vec![],
        }
    }
}
impl Subscription {
    pub fn validate(&self) -> Result<()> {
        crate::config::validate_remote_url(&self.url)?;
        ensure!(
            !self.proxy.trim().is_empty() && !self.proxy.contains(['\r', '\n']),
            "invalid subscription update proxy"
        );
        ensure!(
            (60..=604800).contains(&self.interval),
            "subscription interval must be 60..604800 seconds"
        );
        ensure!(
            !self.user_agent.is_empty()
                && self.user_agent.len() <= 256
                && !self.user_agent.contains(['\r', '\n']),
            "invalid subscription user agent"
        );
        Ok(())
    }
}
