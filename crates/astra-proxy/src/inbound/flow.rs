use crate::rules::Query;
use crate::{
    proxy::session::{Binding, Session},
    App,
};
use anyhow::{bail, Result};
use astra_model::{Inbound, Network, ProcessInfo, RouteAction, Target};
use std::net::SocketAddr;

fn loopback_target(host: &str) -> bool {
    let host = host.trim_matches(['[', ']']);
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) => {
            let ip = ip.to_canonical();
            ip.is_loopback() || ip.is_unspecified()
        }
        Err(_) => {
            let host = host.trim_end_matches('.').to_ascii_lowercase();
            host == "localhost" || host.ends_with(".localhost")
        }
    }
}

/// Metadata from the accepting socket. An explicit listener's local address is
/// useful for process lookup but is never the application's requested address.
pub(crate) struct Flow<'a> {
    original: &'a Target,
    inbound: Inbound,
    network: Network,
    source: Option<SocketAddr>,
    local: Option<SocketAddr>,
}

pub(crate) enum FlowPolicy {
    Requested,
    /// Packet/transparent ingress may recover a routing name for a real IP.
    RecoverName(Option<String>),
    Bound(Binding),
}

impl<'a> Flow<'a> {
    pub(crate) fn new(
        original: &'a Target,
        inbound: Inbound,
        network: Network,
        source: Option<SocketAddr>,
        local: Option<SocketAddr>,
    ) -> Self {
        Self {
            original,
            inbound,
            network,
            source,
            local,
        }
    }

    async fn owner(&self, app: &App) -> Option<ProcessInfo> {
        if let Ok(owner) = crate::hosted::INGRESS_PROCESS.try_with(Clone::clone) {
            return owner;
        }
        if app.rules.standalone_action.is_none() || self.inbound == Inbound::Tun {
            if let (Some(source), Some(local)) = (self.source, self.local) {
                return crate::platform::process::lookup(self.network, source, local).await;
            }
        }
        None
    }

    async fn decision(&self, app: &App, target: &Target) -> Result<RouteAction> {
        let explicit = matches!(
            self.inbound,
            Inbound::Socks5 | Inbound::HttpConnect | Inbound::Mixed
        );
        let process = self.owner(app).await;
        if self.inbound == Inbound::Tun
            && process
                .as_ref()
                .is_some_and(|p| crate::platform::process::own_process(p.pid))
        {
            bail!("TUN recaptured a proxy-core connection; check tun.interface and outbound bypass routes");
        }
        let destination_ip = if explicit {
            self.original.host.parse().ok()
        } else {
            self.local.map(|s| s.ip())
        }
        .filter(|ip| !app.synthetic_ip(*ip));
        let query = Query {
            target,
            inbound: self.inbound,
            network: self.network,
            process: process.as_ref(),
            destination_ip,
        };
        app.rules
            .decide(app.overrides.load().mode, app.cache.as_ref(), query)
            .await
    }

    pub(crate) async fn plan(self, app: &App, policy: FlowPolicy) -> Result<Session> {
        let operation = astra_observe::operation!("routing.ingress", "入站路由请求（含进程查找）");
        let result = self.plan_inner(app, policy).await;
        operation.finish(result.is_ok());
        result
    }

    async fn plan_inner(self, app: &App, policy: FlowPolicy) -> Result<Session> {
        let (target, routing_target, binding) = match policy {
            FlowPolicy::RecoverName(hostname) => {
                let (route, dial) = super::target::select(app, self.original, hostname)?;
                (dial, Some(route), None)
            }
            FlowPolicy::Requested => (app.restore_fake_target(self.original)?, None, None),
            FlowPolicy::Bound(binding) => {
                (app.restore_fake_target(self.original)?, None, Some(binding))
            }
        };
        let mapped = routing_target
            .as_ref()
            .is_some_and(|route| route.host != self.original.host);
        let strict_interface = matches!(binding, Some(Binding::Interface { .. }));
        let route = routing_target.as_ref().unwrap_or(&target);
        let explicit = matches!(
            self.inbound,
            Inbound::Socks5 | Inbound::HttpConnect | Inbound::Mixed
        );
        let local_service = explicit
            && self
                .source
                .is_some_and(|s| !s.ip().to_canonical().is_loopback())
            && loopback_target(&route.host);
        let (action, lease) = if local_service {
            astra_observe::counter!("routing.block", "路由：拦截决策").add(1);
            (RouteAction::Block, app.groups.sessions.lease(&[]))
        } else if let Some(binding) = binding {
            binding.resolve(app).await?
        } else {
            let action = self.decision(app, route).await?;
            if explicit && self.network == Network::Tcp && action == RouteAction::Block {
                (action, app.groups.sessions.lease(&[]))
            } else {
                app.resolve_session(&action).await?
            }
        };
        Ok(Session {
            target,
            mapped,
            action,
            lease,
            inbound: self.inbound,
            source: self.source,
            strict_interface,
        })
    }
}
