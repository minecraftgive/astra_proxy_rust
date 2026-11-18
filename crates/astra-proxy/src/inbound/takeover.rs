//! Platform resources held across configuration generations. Prepare can fail
//! before publication; activation preserves OS resolver and TUN teardown order.
use crate::{
    config::ProxyConfig,
    platform::{system_proxy, transparent, tun},
    runtime::Current,
    App,
};
use anyhow::{Context, Result};
use std::{
    path::Path,
    sync::{Arc, OnceLock},
};

#[derive(Default)]
pub(crate) struct Takeover {
    /// Declared before `tunnel` so the OS resolver is restored first on drop.
    system_dns: Option<crate::platform::system_dns::Takeover>,
    tunnel: Option<tun::Guard>,
    tproxy_udp: Option<transparent::UdpGuard>,
}

pub(crate) struct Prepared {
    replace_tun: bool,
    tunnel: Option<tun::Prepared>,
    replace_udp: bool,
    udp: Option<transparent::PreparedUdp>,
}

impl Prepared {
    pub(crate) fn route_leases(&self, old: &App) -> Vec<Arc<tun::OwnedRoute>> {
        if self.replace_tun {
            self.tunnel
                .as_ref()
                .map(|prepared| prepared.bypass_leases())
                .unwrap_or_default()
        } else {
            old.route_leases.clone()
        }
    }
}

impl Takeover {
    pub(crate) async fn failure(&mut self) -> Option<String> {
        if let Some(tunnel) = &mut self.tunnel {
            if let Some(error) = tunnel.failure().await {
                return Some(error);
            }
        }
        if let Some(udp) = &mut self.tproxy_udp {
            if let Some(error) = udp.failure().await {
                return Some(error);
            }
        }
        None
    }
    pub(crate) async fn prepare(
        &self,
        candidate: &ProxyConfig,
        old: &ProxyConfig,
    ) -> Result<Prepared> {
        let replace_tun =
            candidate.tun != old.tun || (candidate.tun.is_some() && self.tunnel.is_none());
        let tunnel = if replace_tun {
            match &candidate.tun {
                Some(config) => Some(tun::prepare(config.clone(), self.tunnel.as_ref()).await?),
                None => None,
            }
        } else {
            None
        };
        let replace_udp = candidate.tproxy != old.tproxy
            || (candidate.tproxy.is_some() && self.tproxy_udp.is_none());
        let udp = if replace_udp {
            match &candidate.tproxy {
                Some(config) => Some(transparent::prepare_udp(config.listen)?),
                None => None,
            }
        } else {
            None
        };
        Ok(Prepared {
            replace_tun,
            tunnel,
            replace_udp,
            udp,
        })
    }
    pub(crate) async fn activate(&mut self, prepared: Prepared, current: &Current) {
        let Prepared {
            replace_tun,
            tunnel,
            replace_udp,
            udp,
        } = prepared;
        if replace_tun {
            // Restore the OS resolver while the old TUN can still answer.
            self.system_dns = None;
            let previous = self.tunnel.take();
            self.tunnel = tunnel.map(|prepared| prepared.start(current.clone(), previous));
        }
        let wanted = crate::platform::system_dns::Key::wanted(
            &current.load().config,
            self.tunnel.as_ref().and_then(|tunnel| tunnel.name().ok()),
        );
        if self.system_dns.as_ref().map(|takeover| &takeover.key) != wanted.as_ref() {
            self.system_dns = None;
            if let Some(key) = wanted {
                self.system_dns = Some(crate::platform::system_dns::Takeover::start(key).await);
            }
        }
        if replace_udp {
            self.tproxy_udp = udp.map(|prepared| prepared.start(current.clone()));
        }
    }
}

pub(crate) fn switch_system_proxy(
    system: &mut system_proxy::Guard,
    candidate: &ProxyConfig,
    old: &ProxyConfig,
    journal: &Path,
    consistency: &OnceLock<String>,
) -> Result<()> {
    system.restore().context("restore previous system proxy")?;
    match system_proxy::Guard::apply(candidate, journal) {
        Ok(guard) => *system = guard,
        Err(error) => {
            return Err(failed_system_proxy_switch(
                error,
                system_proxy::Guard::apply(old, journal),
                system,
                consistency,
            ));
        }
    }
    Ok(())
}

fn failed_system_proxy_switch(
    original: anyhow::Error,
    rollback: Result<system_proxy::Guard>,
    system: &mut system_proxy::Guard,
    consistency: &std::sync::OnceLock<String>,
) -> anyhow::Error {
    match rollback {
        Ok(previous) => {
            *system = previous;
            original
        }
        Err(rollback) => {
            let message = format!("System proxy switch failed: {original:#}; previous system proxy restoration also failed: {rollback:#}; outcome unknown; controlled shutdown required");
            let _ = consistency.set(message.clone());
            anyhow::anyhow!(message)
        }
    }
}
#[cfg(test)]
#[path = "../test/inbound_takeover_test.rs"]
mod tests;
