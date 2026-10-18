mod config;
mod egress;
mod inbound;
mod limit;
mod mode_config;
mod native;
mod outbound;
mod packet;
mod rule_client;
mod runtime;
mod system_proxy;
mod transparent;
mod tuic;
mod tun;
mod udp;
mod xhttp;

use anyhow::{bail, Context, Result};
use astra_ipc::{Inbound, Network, RouteAction, RouteRequest, Target, API_VERSION};
use config::{OutboundConfig, ProxyConfig};
use moka::sync::Cache;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::time::timeout;

#[derive(Clone, Hash, PartialEq, Eq)]
struct CacheKey {
    target: Target,
    revision: u64,
    network: Network,
    inbound: Inbound,
}

pub struct App {
    config: ProxyConfig,
    source_text: String,
    route_leases: Vec<Arc<tun::OwnedRoute>>,
    outbounds: HashMap<String, OutboundConfig>,
    rule_client: Option<rule_client::RuleClient>,
    standalone_action: Option<RouteAction>,
    cache: Cache<CacheKey, RouteAction>,
    native: native::Registry,
    udp_permits: Arc<limit::Limit>,
    permits: Arc<limit::Limit>,
}

impl App {
    pub async fn route(&self, target: &Target, inbound: Inbound) -> Result<RouteAction> {
        self.route_network(target, inbound, Network::Tcp).await
    }

    pub async fn route_network(
        &self,
        target: &Target,
        inbound: Inbound,
        network: Network,
    ) -> Result<RouteAction> {
        if let Some(action) = &self.standalone_action {
            return Ok(action.clone());
        }
        let client = self.rule_client.as_ref().context("rule client missing")?;
        let revision = client.revision();
        let key = CacheKey {
            revision,
            target: target.clone(),
            network,
            inbound,
        };
        if revision != 0 && self.config.decision_cache_ttl_secs > 0 {
            if let Some(action) = self.cache.get(&key) {
                return Ok(action);
            }
        }
        let req = RouteRequest {
            api_version: API_VERSION,
            id: 0,
            target: target.clone(),
            network,
            inbound,
        };
        let response = client.decide(req).await?;
        // Do not cache a decision across a concurrent rule generation switch.
        if revision != 0 && response.revision == revision && self.config.decision_cache_ttl_secs > 0
        {
            self.cache.insert(key, response.action.clone());
        }
        let decision = response.action;
        Ok(decision)
    }

    pub async fn connect(&self, target: &Target, action: &RouteAction) -> Result<outbound::Stream> {
        if *action == RouteAction::Direct {
            if let Some(interface) = self
                .config
                .tun
                .as_ref()
                .and_then(|tun| tun.interface.as_deref())
            {
                return timeout(
                    Duration::from_secs(self.config.connect_timeout_secs),
                    egress::connect(target, interface),
                )
                .await
                .context("direct egress timeout")?
                .map(|stream| Box::new(stream) as outbound::Stream);
            }
        }
        if let RouteAction::Proxy(tag) = action {
            if let Some(adapter) = self.native.get(tag) {
                return timeout(
                    Duration::from_secs(self.config.connect_timeout_secs),
                    native::connect(adapter, target),
                )
                .await
                .context("outbound connection timeout")?;
            }
        }
        let selected = match action {
            RouteAction::Direct => None,
            RouteAction::Block => bail!("blocked"),
            RouteAction::Proxy(tag) => Some(
                self.outbounds
                    .get(tag)
                    .with_context(|| format!("undefined outbound tag {tag}"))?,
            ),
        };
        timeout(
            Duration::from_secs(self.config.connect_timeout_secs),
            outbound::open(target, selected),
        )
        .await
        .context("outbound connection timeout")?
    }
}

impl App {
    fn load(path: &std::path::Path) -> Result<Self> {
        let path = std::fs::canonicalize(path)?;
        let source_text = std::fs::read_to_string(&path)?;
        let config: ProxyConfig = toml::from_str(&source_text)?;
        let outbounds = config.validate()?;
        let native = native::compile(
            &config.outbounds,
            path.parent().context("configuration has no parent")?,
        )?;
        let mut builder = Cache::builder().max_capacity(65_536);
        if config.decision_cache_ttl_secs > 0 {
            builder = builder.time_to_live(Duration::from_secs(config.decision_cache_ttl_secs));
        }
        Ok(Self {
            standalone_action: config.standalone_tag()?.map(RouteAction::Proxy),
            rule_client: if config.standalone {
                None
            } else {
                Some(rule_client::RuleClient::new(
                    config.rule_socket.clone(),
                    config.ipc_connections,
                    config.decision_timeout_ms,
                ))
            },
            udp_permits: Arc::new(limit::Limit::new(config.max_udp_flows)),
            permits: Arc::new(limit::Limit::new(config.max_connections)),
            cache: builder.build(),
            route_leases: vec![],
            source_text,
            config,
            outbounds,
            native,
        })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    astra_ipc::logging::init()?;
    let mut args = std::env::args().skip(1);
    let first = args.next();
    if first.as_deref() == Some("--restore-system-proxy") {
        system_proxy::recover(std::path::Path::new(
            &args.next().context("expected recovery file path")?,
        ))?;
        return Ok(());
    }
    if matches!(first.as_deref(), Some("--reload" | "--switch")) {
        let socket = args.next().context("expected management socket")?;
        let path = if first.as_deref() == Some("--switch") {
            Some(std::fs::canonicalize(
                args.next().context("expected config path")?,
            )?)
        } else {
            None
        };
        astra_ipc::control::request(
            std::path::Path::new(&socket),
            astra_ipc::control::Command::Reload { path },
        )
        .await?;
        return Ok(());
    }
    let check = first.as_deref() == Some("--check");
    let mode = first.as_deref() == Some("--print-routing-mode");
    let path = if check || mode { args.next() } else { first }
        .unwrap_or_else(|| "configs/proxy.toml".to_owned());
    let app = App::load(std::path::Path::new(&path))?;
    if mode {
        println!(
            "{}",
            if app.config.standalone {
                "standalone"
            } else {
                "rules"
            }
        );
        return Ok(());
    }
    if check {
        return Ok(());
    }
    runtime::run(std::fs::canonicalize(path)?, app).await
}
