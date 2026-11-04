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
mod standalone_tests {
    use super::*;
    fn config(extra: &str, nodes: usize) -> ProxyConfig {
        let mut text = format!("socks5_listen='127.0.0.1:1080'\n{extra}\n");
        for index in 0..nodes {
            text.push_str(&format!(
                "[[outbounds]]\nkind='socks5'\ntag='node{index}'\nserver='127.0.0.1:1081'\n"
            ));
        }
        toml::from_str(&text).unwrap()
    }
    #[test]
    fn standalone_selection_is_explicit_and_never_direct() {
        let single = config("standalone=true", 1);
        single.validate().unwrap();
        assert_eq!(single.standalone_tag().unwrap().as_deref(), Some("node0"));
        let multiple = config("standalone=true\nstandalone_outbound='node1'", 2);
        multiple.validate().unwrap();
        assert_eq!(multiple.standalone_tag().unwrap().as_deref(), Some("node1"));
        for invalid in [
            config("standalone=true", 0),
            config("standalone=true", 2),
            config("standalone=true\nstandalone_outbound='missing'", 1),
            config(
                "rule_socket='/tmp/rules.sock'\nstandalone_outbound='node0'",
                1,
            ),
            config("", 1),
        ] {
            assert!(invalid.validate().is_err());
        }
        let normal = config("rule_socket='/tmp/rules.sock'", 0);
        normal.validate().unwrap();
        assert_eq!(normal.standalone_tag().unwrap(), None);
    }
}

#[cfg(test)]
mod inbound_acl_tests {
    use super::*;
    #[test]
    fn lan_acl_and_authentication_exceptions_have_independent_precedence() {
        let app = crate::App::from_text(std::path::Path::new("acl.toml"), "standalone=true\nstandalone_outbound='only'\nmixed_listen='0.0.0.0:7890'\nallow_lan=false\nauthentication=['alice:correct']\nlan_allowed_ips=['127.0.0.0/8','192.0.2.0/24']\nlan_disallowed_ips=['192.0.2.10/32']\nskip_auth_prefixes=['127.0.0.1/32']\n[proxy_groups.only]\nmembers=['DIRECT']\n".into()).unwrap();
        assert!(app.inbound_allowed("127.0.0.1".parse().unwrap()));
        assert!(!app.inbound_allowed("192.0.2.11".parse().unwrap()));
        let mut overrides = (*app.overrides.load_full()).clone();
        overrides.allow_lan = Some(true);
        app.overrides.store(std::sync::Arc::new(overrides));
        assert!(app.inbound_allowed("192.0.2.11".parse().unwrap()));
        assert!(!app.inbound_allowed("192.0.2.10".parse().unwrap()));
        assert!(!app.inbound_allowed("198.51.100.1".parse().unwrap()));
        assert!(app
            .authentication(Some("127.0.0.1".parse().unwrap()))
            .is_empty());
        assert_eq!(
            app.authentication(Some("192.0.2.11".parse().unwrap()))
                .len(),
            1
        );
        assert_eq!(app.authentication(None).len(), 1);
        let bad: ProxyConfig = toml::from_str("authentication=['missing-separator']").unwrap();
        assert!(bad.validate().is_err());
    }
    #[test]
    fn network_recovery_is_an_opt_in_experiment() {
        let default: ProxyConfig = toml::from_str("socks5_listen='127.0.0.1:1080'").unwrap();
        assert!(!default.experimental.network_recovery);
        assert!(!toml::to_string(&default).unwrap().contains("experimental"));
        let enabled: ProxyConfig = toml::from_str(
            "[experimental]
network_recovery=true",
        )
        .unwrap();
        assert!(enabled.experimental.network_recovery);
        assert!(toml::from_str::<ProxyConfig>(
            "[experimental]
unknown=true"
        )
        .is_err());
    }
}
