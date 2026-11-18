//! A selected outbound and group lease are held for the lifetime of a flow.
use super::group_sessions::Lease;
use crate::{egress, native, outbound, proxy, App};
use anyhow::{bail, Context, Result};
use astra_model::{Inbound, Network, RouteAction, Target};
use serde::Deserialize;
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::time::timeout;

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Binding {
    Block,
    Direct,
    Interface { name: String },
    Proxy { tag: String },
}

pub(crate) struct Session {
    pub(crate) action: RouteAction,
    pub(crate) target: Target,
    pub(crate) mapped: bool,
    pub(crate) lease: Lease,
    pub(crate) inbound: Inbound,
    pub(crate) source: Option<SocketAddr>,
    pub(crate) strict_interface: bool,
}

impl Session {
    pub(crate) async fn connect_tcp(self, app: &App) -> Result<outbound::Stream> {
        let Self {
            target,
            action: resolved,
            lease,
            inbound,
            source,
            strict_interface,
            ..
        } = self;
        let target = &target;
        let traffic = proxy::traffic::Traffic::new(
            &app.metrics,
            source,
            target,
            inbound,
            Network::Tcp,
            &resolved,
        );
        if let Some(traffic) = &traffic {
            traffic.set_path(app.describe_chain(&resolved, &lease));
        }
        let connected = tokio::select! {
            biased;
            _ = lease.cancelled() => Err(anyhow::anyhow!("proxy group switched during connection")),
            _ = proxy::traffic::Traffic::cancelled(&traffic) => Err(anyhow::anyhow!("connection closed by user")),
            result = async {
                if strict_interface {
                    let interface = lease.1.as_deref().context("missing bound interface")?;
                    let socket = timeout(Duration::from_secs(app.config.connect_timeout_secs), egress::connect_bound(target, interface)).await??;
                    Ok(Box::new(socket) as outbound::Stream)
                } else {
                    app.connect_planned(target, &resolved, &lease).await
                }
            } => result,
        };
        match connected {
            Ok(inner) => {
                let inner = lease.wrap(inner);
                if let Some(traffic) = &traffic {
                    traffic.connected();
                } else {
                    return Ok(inner);
                }
                Ok(Box::new(proxy::traffic::Stream::new(inner, traffic)))
            }
            Err(error) => {
                if let Some(traffic) = &traffic {
                    traffic.failed(&error);
                }
                Err(error)
            }
        }
    }

    pub(crate) async fn open_udp(self, app: &App) -> Result<proxy::udp::PacketConnection> {
        let source = self.source.unwrap_or(SocketAddr::from(([0, 0, 0, 0], 0)));
        proxy::udp::open_planned(
            app,
            &self.target,
            self.inbound,
            source,
            self.action,
            self.lease,
        )
        .await
    }
}

impl Binding {
    pub(crate) async fn resolve(self, app: &App) -> Result<(RouteAction, Lease)> {
        match self {
            Binding::Block => bail!("process binding blocks this connection"),
            Binding::Proxy { tag } => app.resolve_session(&RouteAction::Proxy(tag)).await,
            Binding::Direct => Ok((RouteAction::Direct, app.groups.sessions.lease(&[]))),
            Binding::Interface { name } => {
                anyhow::ensure!(
                    !name.trim().is_empty() && !name.chars().any(char::is_control),
                    "invalid bound interface"
                );
                let mut lease = app.groups.sessions.lease(&[]);
                lease.1 = Some(name);
                Ok((RouteAction::Direct, lease))
            }
        }
    }
}

async fn optional_timeout<F: std::future::Future>(
    limit: Option<Duration>,
    future: F,
) -> std::result::Result<F::Output, tokio::time::error::Elapsed> {
    match limit {
        Some(limit) => timeout(limit, future).await,
        None => Ok(future.await),
    }
}

impl App {
    pub async fn connect(&self, target: &Target, action: &RouteAction) -> Result<outbound::Stream> {
        self.connect_with_timeout(
            target,
            action,
            Some(Duration::from_secs(self.config.connect_timeout_secs)),
        )
        .await
    }
    // Diagnostics own their end-to-end deadline; None avoids racing nested timers.
    pub(crate) async fn connect_with_timeout(
        &self,
        target: &Target,
        action: &RouteAction,
        connect_timeout: Option<Duration>,
    ) -> Result<outbound::Stream> {
        let restored = self.restore_fake_target(target)?;
        let target = &restored;
        let (resolved, lease) =
            optional_timeout(connect_timeout, self.groups.resolve_session(self, action))
                .await
                .context("proxy group selection timed out")??;
        let inner = tokio::select! {
            biased;
            _ = lease.cancelled() => bail!("proxy group switched during connection"),
            result = self.connect_planned_with_timeout(target, &resolved, &lease, connect_timeout) => result?,
        };
        Ok(lease.wrap(inner))
    }
    pub(crate) async fn resolve_session(
        &self,
        action: &RouteAction,
    ) -> Result<(RouteAction, proxy::group_sessions::Lease)> {
        timeout(
            Duration::from_secs(self.config.connect_timeout_secs),
            self.groups.resolve_session(self, action),
        )
        .await
        .context("proxy group selection timed out")?
    }
    pub(crate) async fn resolve_action(&self, action: &RouteAction) -> Result<RouteAction> {
        timeout(
            Duration::from_secs(self.config.connect_timeout_secs),
            self.groups.resolve(self, action),
        )
        .await
        .context("proxy group selection timed out; health checks are still running")?
    }
    pub(crate) fn native_for(&self, interface: Option<&str>) -> &native::Registry {
        interface
            .and_then(|name| self.native_interfaces.get(name))
            .unwrap_or(&self.native)
    }
    pub(crate) fn describe_chain(
        &self,
        action: &RouteAction,
        lease: &proxy::group_sessions::Lease,
    ) -> astra_model::runtime::ChainPath {
        astra_model::runtime::ChainPath {
            hops: lease
                .3
                .iter()
                .map(|tag| astra_model::runtime::ChainHop {
                    name: tag.clone(),
                    protocol: self.outbounds[tag].protocol_name().into(),
                })
                .collect(),
            groups: lease.4.clone(),
            status: match action {
                RouteAction::Direct => "direct",
                RouteAction::Block => "blocked",
                _ => "proxy",
            }
            .into(),
            issue: None,
        }
    }
    pub(crate) fn chain_telemetry(&self) -> astra_model::runtime::ChainTelemetry {
        let previews = self
            .config
            .proxy_groups
            .keys()
            .take(128)
            .map(|name| {
                let path = match self
                    .groups
                    .plan(self, &RouteAction::Proxy(name.clone()), false)
                {
                    Ok((action, lease)) => self.describe_chain(&action, &lease),
                    Err(error) => astra_model::runtime::ChainPath {
                        status: "unresolved".into(),
                        issue: Some(error.to_string()),
                        ..Default::default()
                    },
                };
                (name.clone(), path)
            })
            .collect();
        astra_model::runtime::ChainTelemetry {
            connections: self.metrics.chain_paths(),
            previews,
        }
    }
    pub(crate) fn chain_adapter(
        &self,
        nodes: &[String],
        interface: Option<&str>,
        epochs: Vec<(String, u64)>,
    ) -> Result<Arc<native::NativeOutbound>> {
        let first = nodes.first().context("empty proxy chain")?;
        let key = (nodes.to_vec(), interface.map(str::to_owned), epochs);
        if let Some(adapter) = self.chain_cache.get(&key) {
            return Ok(adapter);
        }
        let mut adapter = self
            .native_for(interface)
            .get(first)
            .with_context(|| format!("undefined outbound {first}"))?
            .clone();
        for (index, tag) in nodes.iter().enumerate().skip(1) {
            adapter = Arc::new(
                native::compile_one(
                    &self.outbounds[tag],
                    &self.chain_base,
                    Arc::new(astra_protocols::chain::PrefixDialer(
                        adapter,
                        nodes[index - 1].clone(),
                    )),
                )
                .with_context(|| format!("chain hop {} ({tag})", index + 1))?,
            );
        }
        self.chain_cache.insert(key, adapter.clone());
        Ok(adapter)
    }
    pub(crate) async fn connect_planned(
        &self,
        target: &Target,
        action: &RouteAction,
        lease: &proxy::group_sessions::Lease,
    ) -> Result<outbound::Stream> {
        self.connect_planned_with_timeout(
            target,
            action,
            lease,
            Some(Duration::from_secs(self.config.connect_timeout_secs)),
        )
        .await
    }
    async fn connect_planned_with_timeout(
        &self,
        target: &Target,
        action: &RouteAction,
        lease: &proxy::group_sessions::Lease,
        connect_timeout: Option<Duration>,
    ) -> Result<outbound::Stream> {
        let interface = lease.1.as_deref().or_else(|| {
            self.config
                .tun
                .as_ref()
                .and_then(|tun| tun.interface.as_deref())
        });
        meow_common::outbound_iface::with_outbound_interface(interface, async {
            if matches!(action, RouteAction::Proxy(_)) {
                let adapter = self.chain_adapter(&lease.3, interface, lease.epochs())?;
                return optional_timeout(connect_timeout, native::connect(&adapter, target))
                    .await
                    .context("chain connection timeout")?
                    .with_context(|| format!("proxy chain {}", lease.3.join(" -> ")));
            }
            self.connect_resolved_inner(target, action, interface, connect_timeout)
                .await
        })
        .await?
    }
    pub(crate) async fn connect_resolved_on(
        &self,
        target: &Target,
        action: &RouteAction,
        interface: Option<&str>,
    ) -> Result<outbound::Stream> {
        let (resolved, mut lease) = self.groups.plan(self, action, true)?;
        if interface.is_some() {
            lease.1 = interface.map(str::to_owned);
        }
        self.connect_planned(target, &resolved, &lease).await
    }
    async fn connect_resolved_inner(
        &self,
        target: &Target,
        action: &RouteAction,
        interface: Option<&str>,
        connect_timeout: Option<Duration>,
    ) -> Result<outbound::Stream> {
        if *action == RouteAction::Direct
            && (interface.is_some() || self.config.tun.is_some() || self.resolver().is_some())
        {
            return optional_timeout(connect_timeout, async {
                let mut last = None;
                for address in self.lookup_real(&target.host, target.port).await? {
                    let literal = Target {
                        host: address.ip().to_string(),
                        port: address.port(),
                    };
                    match egress::connect(&literal, interface).await {
                        Ok(stream) => return Ok(stream),
                        Err(error) => last = Some(error),
                    }
                }
                Err(last
                    .unwrap_or_else(|| anyhow::anyhow!("direct DNS lookup returned no addresses")))
            })
            .await
            .context("direct egress timeout")?
            .map(|stream| Box::new(stream) as outbound::Stream);
        }
        if let RouteAction::Proxy(tag) = action {
            if let Some(adapter) = self.native_for(interface).get(tag) {
                return optional_timeout(connect_timeout, native::connect(adapter, target))
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
        optional_timeout(connect_timeout, outbound::open(target, selected))
            .await
            .context("outbound connection timeout")?
    }
}
