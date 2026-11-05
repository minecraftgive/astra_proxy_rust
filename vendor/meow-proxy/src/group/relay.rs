//! Relay proxy group (M1.C-2) — chains multiple outbounds in sequence.
//!
//! Traffic flow: `client → proxy[0] → proxy[1] → … → proxy[N-1] → target`.
//!
//! # Dial algorithm
//!
//! - `proxy[0]`: `dial_tcp(meta_for_proxy[1])` — real TCP connect targeting the
//!   next hop's address.
//! - `proxy[1..N-2]`: `connect_over(stream, meta_for_proxy[i+1])` — proxy-level
//!   CONNECT tunnel through the already-established stream.
//! - `proxy[N-1]`: `connect_over(stream, final_target)` — final hop connects to
//!   the actual target.
//!
//! Nested relay-of-relay works at any chain position: `flatten_hops` splices
//! a nested `RelayGroup`'s resolved members into the outer chain in place, so
//! the preceding hop dials the inner chain's entry point and each inner member
//! runs `connect_over` like any other hop.
//!
//! # UDP
//!
//! UDP does not chain yet. `ProxyAdapter` has no way to hand a hop a dialer
//! at dial time (upstream's `ListenPacketWithDialer`), so the relay cannot
//! run the exit's datagram protocol through the hops before it; only
//! `dialer-proxy` injects a dialer, and it does so when the config is
//! parsed. After flattening, DIRECT/COMPATIBLE hops are dropped as upstream
//! does, and the proxy hops that remain decide where UDP goes:
//!
//! - none: the chain is a direct path, and a DIRECT hop sends the UDP;
//! - one: that hop is the exit and sends the UDP itself, so
//!   `[DIRECT, ss]` exits at `ss` exactly like its TCP;
//! - two or more: `dial_udp` fails closed with `UdpNotSupported`. UDP must
//!   never leave through any hop other than the exit.
//!
//! `support_udp` gives the same answer from a non-touching peek, so the rule
//! engine skips a UDP rule that targets an unchainable relay, as upstream's
//! `SupportUDP` check does. Chaining UDP across several proxy hops is a
//! follow-up. Until then, `dialer-proxy` on the exit node chains its UDP:
//! Shadowsocks and SOCKS5 open their association through the front's own
//! UDP relay, and Trojan, VLESS, Snell and mux sessions carry UDP inside the
//! chained TCP stream.
//!
//! upstream: adapter/outbound/relay.go

use super::dialer_proxy::DialerProxyAdapter;
use async_trait::async_trait;
use meow_common::{
    AdapterType, MeowError, Metadata, Proxy, ProxyAdapter, ProxyConn, ProxyHealth, ProxyPacketConn,
    Result,
};
use smol_str::SmolStr;
use std::sync::Arc;
use tracing::debug;

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Build a `Metadata` that targets the given proxy's `server:port`.
///
/// `proxy.addr()` returns `"host:port"` for all adapter types; we parse it
/// here rather than adding a separate port accessor to the trait.
///
/// Note: `rfind(':')` is intentional — it correctly handles IPv6 literal
/// addresses (e.g. `[2001:db8::1]:1080`) by splitting at the *last* colon,
/// so the port is always the suffix after the rightmost colon.
fn metadata_for_proxy(proxy: &Arc<dyn Proxy>, internal: bool) -> Metadata {
    let addr = proxy.addr();
    // IP literals land in `dst_ip` so the preceding hop encodes a typed
    // address (ATYP 1/4, SocksAddr::Ip) rather than a domain name that
    // happens to look like one.  `conn_type` is explicit `Inner`:
    // `Metadata::default()` yields `ConnType::Http`, but this is an internal
    // chained dial — rules, /connections, and stats must not classify it as
    // an HTTP inbound.  Same rule `ProxyDialer::dial` applies (review B2).
    //
    // `internal` carries the connection's usage-accounting marker through
    // the rebuild — when hop N is itself `dialer-proxy`-chained to a lazy
    // front hop, a housekeeping relay (probe/fetch) must not count as use
    // of that group.
    let (host, port) = if let Some(colon) = addr.rfind(':') {
        (
            &addr[..colon],
            addr[colon + 1..].parse::<u16>().unwrap_or(0),
        )
    } else {
        // Addr with no port (e.g. DIRECT ""). Relay treats it as port 0;
        // DIRECT's connect_over ignores the metadata anyway.
        (addr, 0)
    };
    // `metadata_ip_literal` also folds the bracketed form so
    // `[2001:db8::1]` parses as an IpAddr too (issue #701).
    match meow_common::metadata_ip_literal(host) {
        Some(ip) => Metadata {
            network: meow_common::Network::Tcp,
            conn_type: meow_common::ConnType::Inner,
            dst_ip: Some(ip),
            dst_port: port,
            internal,
            ..Default::default()
        },
        None => Metadata {
            network: meow_common::Network::Tcp,
            conn_type: meow_common::ConnType::Inner,
            host: host.into(),
            dst_port: port,
            internal,
            ..Default::default()
        },
    }
}

/// DIRECT and COMPATIBLE hops add nothing to a chain: they pass the
/// preceding hop's stream through unchanged. Upstream `relay.go` drops
/// `C.Direct`/`C.Compatible` from the chain for the same reason.
fn is_transparent_hop(proxy: &Arc<dyn Proxy>) -> bool {
    matches!(
        proxy.adapter_type(),
        AdapterType::Direct | AdapterType::Compatible
    )
}

/// Resolve a group hop to the concrete proxy selected for this connection.
///
/// Groups may contain other groups, so keep unwrapping until a leaf (or an
/// adapter without an active member) is reached.  Resolve once before dialing
/// so stateful selectors such as load-balance use the same member for both the
/// preceding hop's target and their own `connect_over` call.
///
/// `touch` is passed to `unwrap_proxy`: `true` on the dial path, `false` for
/// the `support_udp` peek, which must not advance round-robin counters or
/// record usage.
fn resolve_proxy(mut proxy: Arc<dyn Proxy>, metadata: &Metadata, touch: bool) -> Arc<dyn Proxy> {
    while let Some(selected) = proxy.unwrap_proxy(metadata, touch) {
        if Arc::ptr_eq(&proxy, &selected) {
            break;
        }
        proxy = selected;
    }
    proxy
}

/// Resolve every hop once and splice nested relay groups' resolved members
/// into the chain in place.
///
/// `resolve_proxy` cannot see through a nested `RelayGroup` (it is a leaf to
/// `unwrap_proxy`), and a group hop has no `addr()` of its own — the address
/// the preceding hop must dial is the *entry point* of the inner chain.  By
/// flattening, `outer = [X, inner[p, q], Y]` becomes `[X, p, q, Y]`: X dials
/// p's server, then each hop runs `connect_over` in order — exactly mihomo's
/// composed-dialer semantics.  Resolving members here (once) also keeps
/// stateful selectors coherent: the member the preceding hop dials is the
/// same member whose `connect_over` runs.
///
/// A `dialer-proxy` hop whose inner adapter is itself a `RelayGroup` is
/// flattened the same way at a non-first position — inside a chain the
/// per-outbound dialer is ignored by design
/// (`DialerProxyAdapter::connect_over` delegates to the inner adapter), so
/// the member effectively *is* the inner chain.  At the *global* first hop
/// the wrapper is kept: its own `dial_tcp` is what applies the configured
/// front dialer.
///
/// Flattening also fails the dial up front on terminal hops — members that
/// are not a transparent `DIRECT` and have no dialable address (`REJECT`,
/// an unresolvable group, …).  Their `connect_over` would error anyway, so
/// failing here is equivalent — except it happens *before* the preceding
/// hop is told to open a real connection to whatever comes after them.
///
/// `touch` is forwarded to [`resolve_proxy`]: dials pass `true`, and the
/// `support_udp` peek passes `false`.
fn flatten_hops(
    proxies: &[Arc<dyn Proxy>],
    metadata: &Metadata,
    touch: bool,
) -> Result<Vec<Arc<dyn Proxy>>> {
    let mut out = Vec::with_capacity(proxies.len());
    flatten_hops_at(proxies, metadata, touch, 0, &mut out)?;
    Ok(out)
}

/// Recursion bound for [`flatten_hops`]: expansion past this depth is a hard
/// error — far beyond any sane config (config resolution already guarantees
/// the group graph is a DAG; the bound stops pathological hand-built graphs).
const MAX_FLATTEN_DEPTH: usize = 16;

/// `RelayHopFailed` for a hop that can never act as a dial target, raised
/// during flattening — before any hop performs network I/O.
fn undialable_hop_error(hop: usize, proxy: &Arc<dyn Proxy>) -> MeowError {
    let name = proxy.name();
    MeowError::RelayHopFailed {
        hop,
        source: Box::new(MeowError::Proxy(format!(
            "relay: {name} has no dialable address"
        ))),
    }
}

/// `out` is threaded through the recursion (rather than collected per
/// level) so `out.is_empty()` means "no hop emitted anywhere yet" — i.e.
/// this member lands at the *global* first position, even when reached
/// through nested-relay splicing.
fn flatten_hops_at(
    proxies: &[Arc<dyn Proxy>],
    metadata: &Metadata,
    touch: bool,
    depth: usize,
    out: &mut Vec<Arc<dyn Proxy>>,
) -> Result<()> {
    for proxy in proxies {
        // A `DialerProxyAdapter` at the global first hop must keep its
        // wrapper, so do not run `resolve_proxy` on it: `unwrap_proxy`
        // delegates to the inner adapter and would strip the wrapper (and
        // its front dialer) whenever the inner is resolvable.
        //
        // Known limitation: this inspects the raw member only. A DPA
        // reached *through* a resolvable group (e.g. Selector whose member
        // is `DPA(group)`) is still unwrapped — parsed configs can only
        // wrap leaf outbounds, so that shape is hand-built-only.
        let resolved = if out.is_empty()
            && proxy
                .as_any()
                .and_then(|a| a.downcast_ref::<DialerProxyAdapter>())
                .is_some()
        {
            Arc::clone(proxy)
        } else {
            resolve_proxy(Arc::clone(proxy), metadata, touch)
        };
        if let Some(relay) = resolved
            .as_any()
            .and_then(|a| a.downcast_ref::<RelayGroup>())
        {
            if depth >= MAX_FLATTEN_DEPTH {
                return Err(MeowError::Proxy(format!(
                    "relay: group expansion exceeds depth {MAX_FLATTEN_DEPTH}"
                )));
            }
            flatten_hops_at(&relay.proxies, metadata, touch, depth + 1, out)?;
            continue;
        }
        if out.is_empty() {
            // Global first hop: it self-dials, so a preserved
            // `DialerProxyAdapter` is valid here.  Anything else that is
            // not a transparent DIRECT hop and has no dialable address is
            // terminal — fail now rather than letting its black-hole
            // `dial_tcp` (e.g. REJECT-DROP's sleep) burn the dial timeout.
            if !is_transparent_hop(&resolved)
                && resolved.addr().is_empty()
                && resolved
                    .as_any()
                    .and_then(|a| a.downcast_ref::<DialerProxyAdapter>())
                    .is_none()
            {
                return Err(undialable_hop_error(0, &resolved));
            }
            out.push(resolved);
            continue;
        }
        // Non-first hop: peel `DialerProxyAdapter` layers to reach an inner
        // `RelayGroup` (DPA(DPA(relay)) …).  Each `resolve_proxy` call
        // re-enters the unwrap chain because a DPA delegates `unwrap_proxy`
        // to its inner; a peel that lands on a leaf keeps the ORIGINAL
        // member below.  Not reachable from parsed configs — the
        // dialer-proxy pass always wraps a leaf — but keeps hand-built
        // graphs consistent.
        let mut peeled = Arc::clone(&resolved);
        while let Some(inner) = peeled
            .as_any()
            .and_then(|a| a.downcast_ref::<DialerProxyAdapter>())
            .map(|dpa| Arc::clone(dpa.inner()))
        {
            peeled = resolve_proxy(inner, metadata, touch);
        }
        if let Some(relay) = peeled.as_any().and_then(|a| a.downcast_ref::<RelayGroup>()) {
            if depth >= MAX_FLATTEN_DEPTH {
                return Err(MeowError::Proxy(format!(
                    "relay: group expansion exceeds depth {MAX_FLATTEN_DEPTH}"
                )));
            }
            flatten_hops_at(&relay.proxies, metadata, touch, depth + 1, out)?;
            continue;
        }
        // A non-DIRECT member with no dialable address (REJECT, an
        // unresolvable group, …) is terminal, but `metadata_for_next_hop`
        // would skip it and hand the preceding hop the *next* real target —
        // `[entry, REJECT]` would make `entry` connect to the final
        // destination before the reject fires.  Fail before any I/O.
        if !is_transparent_hop(&peeled) && peeled.addr().is_empty() {
            return Err(undialable_hop_error(out.len(), &peeled));
        }
        out.push(resolved);
    }
    Ok(())
}

/// Return the target for a hop, skipping later DIRECT/COMPATIBLE hops —
/// they are transparent no-ops inside an already-established relay stream
/// (upstream `relay.go` removes `C.Direct`/`C.Compatible` hops from the
/// target chain the same way).  Members with no dialable address
/// (`addr() == ""`, e.g. REJECT or an unresolvable group) can no longer
/// reach this point — `flatten_hops_at` rejects them before any hop
/// performs I/O; the `!addr().is_empty()` predicate stays as
/// defence-in-depth should the invariant ever break.
fn metadata_for_next_hop(
    proxies: &[Arc<dyn Proxy>],
    start: usize,
    final_target: &Metadata,
) -> Metadata {
    proxies[start..]
        .iter()
        .find(|proxy| !is_transparent_hop(proxy) && !proxy.addr().is_empty())
        .map_or_else(
            || final_target.clone(),
            |proxy| metadata_for_proxy(proxy, final_target.is_internal()),
        )
}

// ─── Core relay functions ─────────────────────────────────────────────────────

/// Establish a TCP relay chain.
///
/// `proxies` must have length ≥ 2 (validated at parse time; `debug_assert`
/// guards against test-harness mistakes).
pub(crate) async fn relay_tcp(
    proxies: &[Arc<dyn Proxy>],
    final_target: &Metadata,
) -> Result<Box<dyn ProxyConn>> {
    debug_assert!(
        proxies.len() >= 2,
        "relay chain must have at least 2 proxies"
    );

    let proxies = flatten_hops(proxies, final_target, true)?;
    if proxies.len() < 2 {
        return Err(MeowError::Proxy(format!(
            "relay: chain resolved to fewer than 2 hops ({})",
            proxies.len()
        )));
    }

    // proxy[0]: real TCP connect; target = the next non-DIRECT proxy's
    // server:port, or the final destination if only DIRECT hops remain.
    let meta = metadata_for_next_hop(&proxies, 1, final_target);
    debug!(
        relay.hop = 0,
        relay.proxy = proxies[0].name(),
        relay.target = %meta.remote_address(),
        "relay: dial_tcp hop 0"
    );
    let mut conn: Box<dyn ProxyConn> =
        proxies[0]
            .dial_tcp(&meta)
            .await
            .map_err(|e| MeowError::RelayHopFailed {
                hop: 0,
                source: Box::new(e),
            })?;

    // proxy[1..N-2]: connect_over through the previous hop's stream.
    for i in 1..proxies.len() - 1 {
        let meta = metadata_for_next_hop(&proxies, i + 1, final_target);
        debug!(
            relay.hop = i,
            relay.proxy = proxies[i].name(),
            relay.target = %meta.remote_address(),
            "relay: connect_over hop {i}"
        );
        conn =
            proxies[i]
                .connect_over(conn, &meta)
                .await
                .map_err(|e| MeowError::RelayHopFailed {
                    hop: i,
                    source: Box::new(e),
                })?;
    }

    // proxy[N-1]: final hop connects to the actual target.
    let last = proxies.len() - 1;
    debug!(
        relay.hop = last,
        relay.proxy = proxies[last].name(),
        relay.target = %final_target.remote_address(),
        "relay: connect_over final hop {last}"
    );
    conn = proxies[last]
        .connect_over(conn, final_target)
        .await
        .map_err(|e| MeowError::RelayHopFailed {
            hop: last,
            source: Box::new(e),
        })?;

    Ok(conn)
}

/// The hop that sends a flattened chain's UDP, or `None` when UDP cannot be
/// sent without chaining it (see the module docs).
///
/// With no proxy hops the chain is a direct path, and its first DIRECT hop
/// sends the UDP. With one proxy hop, that hop is the exit. With two or
/// more, UDP would have to be chained, which no adapter supports yet.
/// Upstream `relay.go` makes the same split before `ListenPacketWithDialer`.
fn udp_exit(hops: &[Arc<dyn Proxy>]) -> Option<&Arc<dyn Proxy>> {
    let mut proxied = hops.iter().filter(|hop| !is_transparent_hop(hop));
    match (proxied.next(), proxied.next()) {
        (None, _) => hops.first(),
        (Some(exit), None) => Some(exit),
        (Some(_), Some(_)) => None,
    }
}

/// Open a relay chain's UDP association on the chain's exit hop.
///
/// Fails closed with `UdpNotSupported` when two or more proxy hops remain
/// after flattening, or when the exit does not support UDP. No hop's
/// `dial_udp` runs in either case: any hop other than the exit would send
/// the datagrams out of the wrong place.
///
/// upstream: adapter/outbound/relay.go — `ListenPacketContext`
async fn relay_udp(
    proxies: &[Arc<dyn Proxy>],
    metadata: &Metadata,
) -> Result<Box<dyn ProxyPacketConn>> {
    let hops = flatten_hops(proxies, metadata, true)?;
    let Some(exit) = udp_exit(&hops) else {
        debug!(
            relay.hops = hops.len(),
            "relay: UDP across more than one proxy hop is not supported"
        );
        return Err(MeowError::UdpNotSupported);
    };
    if !exit.support_udp() {
        return Err(MeowError::UdpNotSupported);
    }
    debug!(
        relay.proxy = exit.name(),
        relay.target = %metadata.remote_address(),
        "relay: dial_udp on the exit hop"
    );
    exit.dial_udp(metadata).await
}

// ─── RelayGroup ───────────────────────────────────────────────────────────────

/// A relay proxy group: chains ≥2 proxies in sequence.
///
/// upstream: adapter/outbound/relay.go — `Relay`
pub struct RelayGroup {
    name: SmolStr,
    proxies: Vec<Arc<dyn Proxy>>,
    health: ProxyHealth, // for API surface; relay has no self-health-check
}

impl RelayGroup {
    /// Create a new relay group.
    ///
    /// Panics in debug builds if `proxies.len() < 2` — parse-time hard-error
    /// must have been enforced before calling this.
    pub fn new(name: &str, proxies: Vec<Arc<dyn Proxy>>) -> Self {
        debug_assert!(
            proxies.len() >= 2,
            "relay group requires at least 2 proxies; got {}",
            proxies.len()
        );
        Self {
            name: SmolStr::from(name),
            proxies,
            health: ProxyHealth::new(),
        }
    }
}

#[async_trait]
impl ProxyAdapter for RelayGroup {
    fn name(&self) -> &str {
        &self.name
    }

    fn adapter_type(&self) -> AdapterType {
        AdapterType::Relay
    }

    /// Relay groups have no single address — they span a chain.
    fn addr(&self) -> &str {
        ""
    }

    /// Whether `dial_udp` would open an association: the exit hop's own
    /// answer, `false` when two or more proxy hops remain after flattening,
    /// and `true` for an all-DIRECT chain (see the module docs).
    ///
    /// The flattening here is a peek (`touch = false`), so it changes no
    /// usage counters. Groups resolve with no destination, so a
    /// destination-keyed pick (consistent-hashing load-balance) may differ
    /// from the one a dial makes. `dial_udp` checks again and fails closed.
    ///
    /// upstream: relay.go — `SupportUDP`, minus the dialer-chained branch
    /// meow cannot take yet.
    fn support_udp(&self) -> bool {
        let peek = Metadata {
            network: meow_common::Network::Udp,
            ..Default::default()
        };
        flatten_hops(&self.proxies, &peek, false)
            .is_ok_and(|hops| udp_exit(&hops).is_some_and(|exit| exit.support_udp()))
    }

    async fn dial_tcp(&self, metadata: &Metadata) -> Result<Box<dyn ProxyConn>> {
        relay_tcp(&self.proxies, metadata).await
    }

    /// Sends UDP from the chain's exit hop, or fails closed with
    /// `UdpNotSupported`; see `relay_udp`.
    ///
    /// upstream: relay.go — `ListenPacketContext`.
    async fn dial_udp(&self, metadata: &Metadata) -> Result<Box<dyn ProxyPacketConn>> {
        relay_udp(&self.proxies, metadata).await
    }

    // No `connect_over` override: after `flatten_hops` no `RelayGroup` can
    // legitimately appear at a non-first hop (nested relays are spliced in
    // place and over-depth expansion is a hard error), so the trait's
    // default `NotSupported` is the correct failure for any group that
    // still reaches this point — loud, and attributed to its own hop index.

    fn health(&self) -> &ProxyHealth {
        &self.health
    }
}

impl Proxy for RelayGroup {
    // Deliberately no `unwrap_proxy` override: a relay resolves its hops
    // lazily per-dial (metadata may differ per connection), so a
    // match-time peek cannot name "the" member. Upstream `Relay` likewise
    // never overrode `Unwrap` — a PASS hop deep in a chain surfaces as a
    // loud `RelayHopFailed` at dial (flattening rejects nop hops) rather
    // than a silent rule skip. Match-time `unwraps_to(Pass)` therefore
    // sees a relay group as opaque — the rule materializes and fails
    // closed at dial.
    fn alive(&self) -> bool {
        self.health.alive()
    }

    fn alive_for_url(&self, _url: &str) -> bool {
        self.health.alive()
    }

    fn last_delay(&self) -> u16 {
        self.health.last_delay()
    }

    fn last_delay_for_url(&self, _url: &str) -> u16 {
        self.health.last_delay()
    }

    fn delay_history(&self) -> Vec<meow_common::DelayHistory> {
        self.health.delay_history()
    }

    /// Lets `flatten_hops` downcast a nested relay hop and splice this
    /// group's resolved members into the outer chain.
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    fn members(&self) -> Option<Vec<String>> {
        Some(self.proxies.iter().map(|p| p.name().to_string()).collect())
    }

    fn member_proxies(&self) -> Option<Vec<Arc<dyn Proxy>>> {
        Some(self.proxies.clone())
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "../test/group_relay_test.rs"]
mod tests;
