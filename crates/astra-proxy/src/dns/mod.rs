//! DNS integration: resolver policy stays in astra-dns; only egress and listener
//! lifetime live here. OS socket binding is delegated to platform::egress.
mod server;
#[cfg(test)]
mod tests;
mod upstream;
use crate::App;
use anyhow::Result;
use astra_ipc::Target;
pub(crate) use server::tcp;
pub(crate) use server::{Guard, Prepared};
use std::{future::Future, net::IpAddr, pin::Pin};

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
    pub(crate) async fn lookup_real(
        &self,
        host: &str,
        port: u16,
    ) -> Result<Vec<std::net::SocketAddr>> {
        if let Some(dns) = &self.dns {
            return Ok(dns
                .lookup(host, &Transport(self))
                .await?
                .into_iter()
                .map(|ip| std::net::SocketAddr::new(ip, port))
                .collect());
        }
        Ok(tokio::net::lookup_host((host, port)).await?.collect())
    }
    pub(crate) async fn dns_answer(&self, raw: &[u8]) -> Result<Vec<u8>> {
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
