//! Per-outbound `dialer-proxy` support (issue #210, mihomo-compatible).
//!
//! A `dialer-proxy` makes a single outbound establish its underlying connection
//! *through* another proxy/group instead of dialling the OS socket directly:
//!
//! ```yaml
//! - name: daniel
//!   type: snell
//!   server: ss.example.com
//!   port: 8443
//!   dialer-proxy: fast      # reach ss.example.com via the `fast` proxy
//! ```
//!
//! This is exactly a two-hop relay chain `[dialer, inner]` where the final hop
//! (`inner`) connects to the real target, so we reuse the existing relay dial
//! algorithm (`relay::relay_tcp`) rather than introducing a second chained
//! dialer (architect guidance on issue #210).  `DialerProxyAdapter` otherwise
//! presents itself as the inner outbound — same name, type, address, and health
//! — so dashboards and rules see no difference.
//!
//! The front hop is held as a [`DialerTarget`] — a *name* plus a registry
//! handle — and resolved on every dial, matching mihomo's
//! `component/proxydialer/byname.go`.  Capturing the front proxy as an `Arc`
//! while the config is still being built freezes a stale entry instead: proxy
//! groups clone their members before the `dialer-proxy` pass replaces them, so
//! a grouped node would silently bypass its chain (issue #513).
//!
//! ## Limitations
//!
//! - **UDP**: this *wrapper* is the fallback for transports that own their
//!   framing and cannot accept an injected dialer — routing a UDP
//!   association through an arbitrary dialer needs per-protocol
//!   UDP-over-proxy framing, so `dial_udp` returns
//!   [`MeowError::UdpNotSupported`] (Class A, ADR-0002) here. Protocols
//!   that do accept an injected `TcpDialer` (e.g. Shadowsocks, SOCKS5)
//!   chain UDP natively and never reach this adapter.
//! - **As a relay hop**: when a dialer-proxy outbound itself appears inside a
//!   `relay` chain, `connect_over` delegates to the inner adapter — the relay
//!   chain already defines the path, so the per-outbound dialer is not applied a
//!   second time. At the chain's *first* hop the wrapper is kept instead and
//!   its own `dial_tcp` runs, so the configured front dialer still fires.

use async_trait::async_trait;
use meow_common::{
    AdapterType, DelayHistory, MeowError, Metadata, Proxy, ProxyAdapter, ProxyConn, ProxyHealth,
    ProxyPacketConn, Result,
};
use std::sync::Arc;

use super::relay::relay_tcp;
use crate::dialer::DialerTarget;

/// Wraps an outbound so its underlying connection is dialled through `dialer`.
pub struct DialerProxyAdapter {
    /// The actual outbound (SS/Trojan/VLESS/Snell/…); identity is borrowed from
    /// it so the wrapper is transparent to the API and rule engine.
    inner: Arc<dyn Proxy>,
    /// Front proxy/group the inner outbound dials through, resolved by name on
    /// every dial.
    dialer: DialerTarget,
}

impl DialerProxyAdapter {
    pub fn new(inner: Arc<dyn Proxy>, dialer: DialerTarget) -> Self {
        Self { inner, dialer }
    }

    /// Name of the front dialer proxy/group (for diagnostics/tests).
    pub fn dialer_name(&self) -> &str {
        self.dialer.name()
    }

    /// The wrapped outbound. Used by relay flattening to splice an inner
    /// relay group's members into the outer chain.
    pub(crate) fn inner(&self) -> &Arc<dyn Proxy> {
        &self.inner
    }
}

#[async_trait]
impl ProxyAdapter for DialerProxyAdapter {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn adapter_type(&self) -> AdapterType {
        self.inner.adapter_type()
    }

    fn addr(&self) -> &str {
        self.inner.addr()
    }

    /// UDP through this fallback wrapper is not supported; see module docs.
    fn support_udp(&self) -> bool {
        false
    }

    async fn dial_tcp(&self, metadata: &Metadata) -> Result<Box<dyn ProxyConn>> {
        // `[dialer, inner]`: dialer dials inner's server, inner connects to the
        // real target via `connect_over`. An unresolvable front hop must fail —
        // dialling direct would leak past a chain configured for policy reasons.
        // The depth guard mirrors `NamedProxyDialer`: a provider-sourced member
        // can route the front hop's group selection back to this adapter
        // (issue #489).
        crate::dialer::scoped_chain_dial(
            self.dialer.name(),
            |name| {
                MeowError::Proxy(format!(
                    "dialer-proxy '{name}': chain exceeds {} hops; a provider \
                     member or group is routing the dial back into itself",
                    crate::dialer::MAX_DIALER_CHAIN_DEPTH
                ))
            },
            async {
                let front = self
                    .dialer
                    .resolve()
                    .ok_or_else(|| MeowError::Proxy(self.dialer.missing_error()))?;
                let chain = [front, Arc::clone(&self.inner)];
                relay_tcp(&chain, metadata).await
            },
        )
        .await
    }

    async fn dial_udp(&self, _metadata: &Metadata) -> Result<Box<dyn ProxyPacketConn>> {
        Err(MeowError::UdpNotSupported)
    }

    /// As a relay hop the chain already defines the path; do not re-apply the
    /// per-outbound dialer. Delegate to the inner adapter's handshake.
    async fn connect_over(
        &self,
        stream: Box<dyn ProxyConn>,
        metadata: &Metadata,
    ) -> Result<Box<dyn ProxyConn>> {
        self.inner.connect_over(stream, metadata).await
    }

    fn unwrap_proxy(&self, metadata: &Metadata, touch: bool) -> Option<Arc<dyn Proxy>> {
        self.inner.unwrap_proxy(metadata, touch)
    }

    /// Forward the session flush (issue #695): `member_proxies` exposes the
    /// inner adapter's members, never the inner adapter itself, so the
    /// reachability walk would otherwise miss its pools. A wrapper chain is
    /// built bottom-up, so this cannot recurse into a cycle.
    fn reset_sessions(&self) {
        self.inner.reset_sessions();
    }

    fn health(&self) -> &ProxyHealth {
        self.inner.health()
    }
}

impl Proxy for DialerProxyAdapter {
    fn alive(&self) -> bool {
        self.inner.alive()
    }

    fn alive_for_url(&self, url: &str) -> bool {
        self.inner.alive_for_url(url)
    }

    fn last_delay(&self) -> u16 {
        self.inner.last_delay()
    }

    fn last_delay_for_url(&self, url: &str) -> u16 {
        self.inner.last_delay_for_url(url)
    }

    fn delay_history(&self) -> Vec<DelayHistory> {
        self.inner.delay_history()
    }

    /// Lets relay `flatten_hops` downcast the wrapper and reach an inner
    /// `RelayGroup`'s members.
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    fn members(&self) -> Option<Vec<String>> {
        self.inner.members()
    }

    fn member_proxies(&self) -> Option<Vec<Arc<dyn Proxy>>> {
        self.inner.member_proxies()
    }

    fn current(&self) -> Option<String> {
        self.inner.current()
    }
}

#[cfg(test)]
#[path = "../test/group_dialer_proxy_test.rs"]
mod tests;
