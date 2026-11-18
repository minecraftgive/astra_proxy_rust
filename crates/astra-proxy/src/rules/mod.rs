//! Client routing policy and revision-scoped cache for decisions from Rule Core.
mod client;
use crate::{config::ProxyConfig, App};
use anyhow::{Context, Result};
use astra_ipc::API_VERSION;
use astra_model::runtime::Mode;
use astra_model::{Inbound, Network, ProcessInfo, RouteAction, RouteRequest, Target};
use moka::sync::Cache;
use std::time::Duration;

pub(crate) type DecisionCache = Cache<CacheKey, RouteAction>;

pub(crate) fn decision_cache(ttl: u64) -> Option<DecisionCache> {
    (ttl > 0).then(|| {
        Cache::builder()
            .max_capacity(8192)
            .time_to_live(Duration::from_secs(ttl))
            .build()
    })
}

pub(crate) struct Query<'a> {
    pub(crate) target: &'a Target,
    pub(crate) inbound: Inbound,
    pub(crate) network: Network,
    pub(crate) process: Option<&'a ProcessInfo>,
    pub(crate) destination_ip: Option<std::net::IpAddr>,
}

#[derive(Clone, Hash, PartialEq, Eq)]
pub(crate) struct CacheKey {
    destination_ip: Option<std::net::IpAddr>,
    target: Target,
    revision: u64,
    network: Network,
    inbound: Inbound,
    process: Option<ProcessInfo>,
}

#[derive(Hash)]
struct CacheKeyRef<'a> {
    destination_ip: Option<std::net::IpAddr>,
    target: &'a Target,
    revision: u64,
    network: Network,
    inbound: Inbound,
    process: Option<&'a ProcessInfo>,
}

impl moka::Equivalent<CacheKey> for CacheKeyRef<'_> {
    fn equivalent(&self, key: &CacheKey) -> bool {
        self.destination_ip == key.destination_ip
            && self.target == &key.target
            && self.revision == key.revision
            && self.network == key.network
            && self.inbound == key.inbound
            && self.process == key.process.as_ref()
    }
}

pub(crate) struct Rules {
    pub(crate) rule_client: Option<client::RuleClient>,
    pub(crate) standalone_action: Option<RouteAction>,
}

impl Rules {
    pub(crate) fn new(config: &ProxyConfig) -> Result<Self> {
        Ok(Self {
            standalone_action: config.standalone_tag()?.map(RouteAction::Proxy),
            rule_client: if config.standalone || config.dns_only() {
                None
            } else {
                Some(client::RuleClient::new(
                    config.rule_socket.clone(),
                    config.ipc_connections,
                    config.decision_timeout_ms,
                ))
            },
        })
    }
    pub(crate) async fn decide(
        &self,
        mode: Mode,
        cache: Option<&DecisionCache>,
        query: Query<'_>,
    ) -> Result<RouteAction> {
        let Query {
            target,
            inbound,
            network,
            process,
            destination_ip,
        } = query;
        let operation = astra_observe::operation!("routing", "路由决策");
        let result = self.decide_inner(mode, cache, &query).await;
        operation.finish(result.is_ok());
        match &result {
            Ok(action) => {
                match action {
                    RouteAction::Direct => {
                        astra_observe::counter!("routing.direct", "路由：直连决策").add(1)
                    }
                    RouteAction::Block => {
                        astra_observe::counter!("routing.block", "路由：拦截决策").add(1)
                    }
                    RouteAction::Proxy(_) => {
                        astra_observe::counter!("routing.proxy", "路由：代理决策").add(1)
                    }
                }
                tracing::debug!(target:"astra_route",phase="route",host=?target.host,port=target.port,?destination_ip,?inbound,?network,?process,?action,"route selected");
            }
            Err(error) => {
                tracing::debug!(target:"astra_route",phase="route_failed",host=?target.host,port=target.port,?inbound,?network,%error,"routing failed")
            }
        }
        result
    }

    async fn decide_inner(
        &self,
        mode: Mode,
        cache: Option<&DecisionCache>,
        query: &Query<'_>,
    ) -> Result<RouteAction> {
        let Query {
            target,
            inbound,
            network,
            process,
            destination_ip,
        } = *query;
        match mode {
            Mode::Direct => {
                astra_observe::counter!("routing.cache_bypass", "路由缓存：绕过查询").add(1);
                return Ok(RouteAction::Direct);
            }
            Mode::Global => {
                astra_observe::counter!("routing.cache_bypass", "路由缓存：绕过查询").add(1);
                return Ok(RouteAction::Proxy("PROXY".into()));
            }
            Mode::Rule => {}
        }
        if let Some(action) = &self.standalone_action {
            astra_observe::counter!("routing.cache_bypass", "路由缓存：绕过查询").add(1);
            return Ok(action.clone());
        }
        let client = self.rule_client.as_ref().context("rule client missing")?;
        let revision = client.revision();
        let key = CacheKeyRef {
            destination_ip,
            revision,
            target,
            network,
            inbound,
            process,
        };
        if revision != 0 && cache.is_some() {
            if let Some(action) = cache.and_then(|cache| cache.get(&key)) {
                astra_observe::counter!("routing.cache_hits", "路由缓存：命中").add(1);
                return Ok(action);
            }
            astra_observe::counter!("routing.cache_misses", "路由缓存：未命中").add(1);
        } else {
            astra_observe::counter!("routing.cache_bypass", "路由缓存：绕过查询").add(1);
        }
        let req = RouteRequest {
            destination_ip,
            api_version: API_VERSION,
            id: 0,
            target: target.clone(),
            network,
            inbound,
            process: process.cloned(),
        };
        let response = client.decide(req).await?;
        // Do not cache a decision across a concurrent rule generation switch.
        if revision != 0 && response.revision == revision {
            if let Some(cache) = cache {
                let key = CacheKey {
                    destination_ip,
                    revision,
                    target: target.clone(),
                    network,
                    inbound,
                    process: process.cloned(),
                };
                cache.insert(key, response.action.clone());
                astra_observe::counter!("routing.cache_inserts", "路由缓存：写入").add(1);
            }
        }
        Ok(response.action)
    }
}

impl App {
    pub async fn route(&self, target: &Target, inbound: Inbound) -> Result<RouteAction> {
        self.route_network(target, inbound, Network::Tcp).await
    }

    pub(crate) async fn policy_action(
        &self,
        target: &Target,
        inbound: Inbound,
        network: Network,
    ) -> Result<RouteAction> {
        let restored = self.restore_fake_target(target)?;
        let query = Query {
            target: &restored,
            inbound,
            network,
            process: None,
            destination_ip: None,
        };
        self.rules
            .decide(self.overrides.load().mode, self.cache.as_ref(), query)
            .await
    }

    /// A policy query previews groups without advancing a balancing selector.
    pub async fn route_network(
        &self,
        target: &Target,
        inbound: Inbound,
        network: Network,
    ) -> Result<RouteAction> {
        let action = self.policy_action(target, inbound, network).await?;
        if matches!(action, RouteAction::Proxy(_))
            && self.resolve_action(&action).await? == RouteAction::Block
        {
            return Ok(RouteAction::Block);
        }
        Ok(action)
    }
}

#[cfg(test)]
#[path = "../test/rules_cache_test.rs"]
mod cache_tests;
