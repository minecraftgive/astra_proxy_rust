//! DNS integration: resolver policy stays in astra-dns; only egress and listener
//! lifetime live here. OS socket binding is delegated to platform::egress.
mod server;
#[cfg(test)]
#[path = "../test/dns_test.rs"]
mod tests;
mod upstream;
use crate::App;
use anyhow::Result;
use astra_model::Target;
pub(crate) use server::tcp;
pub(crate) use server::{Guard, Prepared};
use std::{future::Future, net::IpAddr, pin::Pin};
pub(crate) use upstream::{framed, tls_connector};

pub(super) struct Transport<'a>(pub &'a App);
impl astra_dns::Transport for Transport<'_> {
    fn exchange<'a>(
        &'a self,
        upstream: &'a astra_dns::Upstream,
        query: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>>> + Send + 'a>> {
        Box::pin(upstream::exchange(self.0, upstream, query))
    }
}
/// Ignores respect_rules: upstreams are dialed directly, so a lookup made
/// while dialing a proxy cannot re-enter that proxy.
struct DirectTransport<'a>(&'a App);
impl astra_dns::Transport for DirectTransport<'_> {
    fn exchange<'a>(
        &'a self,
        upstream: &'a astra_dns::Upstream,
        query: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>>> + Send + 'a>> {
        Box::pin(upstream::exchange_direct(self.0, upstream, query))
    }
}

/// Build the lookup-only resolver: none when the DNS service already exists
/// or plaintext is explicitly allowed; otherwise the configured (or default)
/// encrypted upstreams without listener, hijack or Fake IP.
pub(crate) fn lookup_resolver(
    service: bool,
    config: Option<&astra_dns::Config>,
) -> Result<Option<astra_dns::Resolver>> {
    if service || config.is_some_and(|config| config.plaintext_bypass) {
        return Ok(None);
    }
    let mut config = config.cloned().unwrap_or_default();
    config.enable = true;
    config.listen.clear();
    config.hijack = false;
    config.fake_ip = false;
    astra_dns::Resolver::new(config).map(Some)
}

/// `localhost` names never leave the machine (RFC 6761).
fn loopback(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host == "localhost" || host.ends_with(".localhost")
}

/// Process-wide hostname hook for vendored protocol adapters. Every node
/// server hostname resolved by meow/astra-protocols comes through here.
pub(crate) struct HostHook(pub Box<dyn Fn() -> std::sync::Arc<App> + Send + Sync>);
impl HostHook {
    fn addresses(result: Result<Vec<std::net::SocketAddr>>) -> std::io::Result<Vec<IpAddr>> {
        result
            .map(|addresses| addresses.into_iter().map(|a| a.ip()).collect())
            .map_err(|error| std::io::Error::other(format!("{error:#}")))
    }
}
#[async_trait::async_trait]
impl meow_common::HostResolver for HostHook {
    async fn resolve(&self, host: &str) -> std::io::Result<IpAddr> {
        self.resolve_all(host)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("no address for {host}"),
                )
            })
    }
    async fn resolve_all(&self, host: &str) -> std::io::Result<Vec<IpAddr>> {
        let app = (self.0)();
        Self::addresses(app.lookup_real(host, 0).await)
    }
    async fn resolve_all_nested(&self, host: &str) -> Option<std::io::Result<Vec<IpAddr>>> {
        let app = (self.0)();
        app.resolver()?;
        Some(Self::addresses(app.lookup_with(host, 0, true).await))
    }
}
impl App {
    pub(crate) fn synthetic_ip(&self, ip: IpAddr) -> bool {
        match ip {
            IpAddr::V4(ip) => {
                u32::from(ip) & 0xfffe0000 == u32::from(std::net::Ipv4Addr::new(198, 18, 0, 0))
            }
            IpAddr::V6(_) => self.dns.as_ref().is_some_and(|dns| dns.is_fake(ip)),
        }
    }
    pub(crate) fn restore_fake_target(&self, target: &Target) -> Result<Target> {
        if let Ok(ip) = target.host.parse::<IpAddr>() {
            if self.synthetic_ip(ip) {
                let domain=self.dns.as_ref().map(|dns|dns.restore(ip)).transpose()?.flatten()
                    .ok_or_else(||anyhow::anyhow!("unmapped Fake IP {ip}; refresh application DNS and check for another proxy using a different Fake IP mapping"))?;
                return Ok(Target {
                    host: domain,
                    port: target.port,
                });
            }
        }
        Ok(target.clone())
    }
    pub(crate) fn restore_target(&self, target: &Target) -> Result<Target> {
        if target.host.parse().is_ok_and(|ip| self.synthetic_ip(ip)) {
            return self.restore_fake_target(target);
        }
        let mut restored = target.clone();
        if let (Some(dns), Ok(ip)) = (&self.dns, target.host.parse::<IpAddr>()) {
            if let Some(domain) = dns.restore(ip)? {
                restored.host = domain;
            }
        }
        Ok(restored)
    }
    /// The resolver answering this process's own lookups: the DNS service
    /// when enabled, else the lookup-only encrypted resolver.
    pub(crate) fn resolver(&self) -> Option<&astra_dns::Resolver> {
        self.dns.as_ref().or(self.lookup_dns.as_ref())
    }
    pub(crate) async fn lookup_real(
        &self,
        host: &str,
        port: u16,
    ) -> Result<Vec<std::net::SocketAddr>> {
        self.lookup_with(host, port, false).await
    }
    /// Resolve for a local dial. The system resolver is used only when no
    /// encrypted resolver exists (plaintext_bypass) or for a
    /// plaintext_bypass_domains name without its own nameserver_policy.
    pub(crate) async fn lookup_with(
        &self,
        host: &str,
        port: u16,
        direct: bool,
    ) -> Result<Vec<std::net::SocketAddr>> {
        let host = host.trim_start_matches('[').trim_end_matches(']');
        if let Ok(ip) = host.parse::<IpAddr>() {
            return Ok(vec![std::net::SocketAddr::new(ip, port)]);
        }
        if loopback(host) {
            return Ok(vec![
                std::net::SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), port),
                std::net::SocketAddr::new(std::net::Ipv6Addr::LOCALHOST.into(), port),
            ]);
        }
        let Some(dns) = self.resolver() else {
            return Ok(tokio::net::lookup_host((host, port)).await?.collect());
        };
        if !dns.config.plaintext_bypass
            && dns.config.plaintext_allowed(host)
            && !dns.config.has_policy(host)
        {
            return Ok(tokio::net::lookup_host((host, port)).await?.collect());
        }
        let addresses = if direct {
            dns.lookup(host, &DirectTransport(self)).await
        } else {
            dns.lookup(host, &Transport(self)).await
        };
        Ok(addresses?
            .into_iter()
            .map(|ip| std::net::SocketAddr::new(ip, port))
            .collect())
    }
    pub(crate) async fn dns_answer(&self, raw: &[u8]) -> Result<Vec<u8>> {
        if let Some(host) = &self.hosted {
            return host.dns_answer(raw).await;
        }
        let dns = self
            .dns
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("DNS service disabled"))?;
        dns.answer(raw, &Transport(self)).await
    }
    pub(crate) fn intercept_dns(&self, destination: std::net::SocketAddr) -> bool {
        self.dns
            .as_ref()
            .is_some_and(|dns| dns.config.hijack && destination.port() == 53)
    }
}
