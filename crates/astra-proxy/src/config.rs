use anyhow::{Context, Result};
use std::collections::HashMap;

pub use astra_protocols::config::OutboundConfig;

pub use astra_model::config::proxy::ProxyConfig;
pub trait ProxyConfigExt {
    fn validate(&self) -> Result<HashMap<String, OutboundConfig>>;
}
impl ProxyConfigExt for ProxyConfig {
    fn validate(&self) -> Result<HashMap<String, OutboundConfig>> {
        let map = self.validate_structure()?;
        crate::mode_config::validate(self)?;
        anyhow::ensure!(
            self.max_connections <= tokio::sync::Semaphore::MAX_PERMITS,
            "invalid connection limits"
        );
        for ob in &self.outbounds {
            match ob {
                OutboundConfig::VlessTls { sni, .. } => {
                    tokio_rustls::rustls::pki_types::ServerName::try_from(sni.clone())
                        .with_context(|| format!("invalid SNI for {}", ob.tag()))?;
                }
                OutboundConfig::TrojanTls { sni, .. } => {
                    tokio_rustls::rustls::pki_types::ServerName::try_from(sni.clone())
                        .with_context(|| format!("invalid SNI for {}", ob.tag()))?;
                }
                _ => {}
            }
        }
        for (name, group) in &self.proxy_groups {
            if group.mode != astra_model::groups::Mode::Select {
                crate::proxy::groups::test_uri(&group.url)
                    .with_context(|| format!("proxy group {name}"))?;
            }
        }
        Ok(map)
    }
}

#[cfg(test)]
#[path = "test/config_standalone_test.rs"]
mod standalone_tests;

#[cfg(test)]
#[path = "test/config_inbound_acl_test.rs"]
mod inbound_acl_tests;
