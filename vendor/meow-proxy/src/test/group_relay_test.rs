use super::*;
use crate::{LbStrategy, LoadBalanceGroup, SelectorGroup};
use meow_common::{DelayHistory, MeowError, ProxyConn, ProxyHealth, ProxyPacketConn};
use std::net::SocketAddr;
use std::sync::Arc;

// ── MockProxy ──────────────────────────────────────────────────────────

/// A transparent mock hop.
///
/// - `dial_tcp` opens a `NopConn` and records nothing (hop 0 never visits).
/// - `connect_over` appends `self.marker` to `visits` and returns the
///   stream (or `fail_with` if set). Records `Metadata.host` to
///   `last_dial_host` for A5 assertions.
struct MockProxy {
    proxy_name: String,
    adapter_type: AdapterType,
    /// Pre-formatted `"server:port"` string — avoids a fresh allocation
    /// (and a `Box::leak`) on every `addr()` call.
    addr_str: String,
    health: ProxyHealth,
    udp: bool,
    /// Each `connect_over` call appends `self.marker` here.
    visits: Arc<parking_lot::Mutex<Vec<u8>>>,
    marker: u8,
    /// Each dial call (dial_tcp or connect_over) records the target host.
    last_dial_host: Arc<parking_lot::Mutex<Option<String>>>,
    /// If `Some`, `connect_over` returns this error.
    fail_with: Arc<parking_lot::Mutex<Option<MeowError>>>,
    /// If `Some`, `dial_tcp` returns this error.
    dial_fail_with: Arc<parking_lot::Mutex<Option<MeowError>>>,
    /// Each `dial_udp` call records its target host here.
    udp_dials: Arc<parking_lot::Mutex<Vec<String>>>,
}

/// Recorded dial target: `host` for domains, `dst_ip` for IP literals —
/// `metadata_for_proxy` types IP-literal servers as `dst_ip`.
fn recorded_host(metadata: &Metadata) -> String {
    if metadata.host.is_empty() {
        metadata.dst_ip.map(|ip| ip.to_string()).unwrap_or_default()
    } else {
        metadata.host.to_string()
    }
}

impl MockProxy {
    fn new(name: &str, server: &str, port: u16, marker: u8) -> Arc<Self> {
        Arc::new(Self {
            proxy_name: name.to_string(),
            adapter_type: AdapterType::Socks5,
            addr_str: format!("{server}:{port}"),
            health: ProxyHealth::new(),
            udp: false,
            visits: Arc::new(parking_lot::Mutex::new(Vec::new())),
            marker,
            last_dial_host: Arc::new(parking_lot::Mutex::new(None)),
            fail_with: Arc::new(parking_lot::Mutex::new(None)),
            dial_fail_with: Arc::new(parking_lot::Mutex::new(None)),
            udp_dials: Arc::new(parking_lot::Mutex::new(Vec::new())),
        })
    }

    fn new_udp(name: &str, server: &str, port: u16, marker: u8) -> Arc<Self> {
        let m = Self::new(name, server, port, marker);
        // SAFETY: only called at construction before sharing.
        let inner = Arc::try_unwrap(m).ok().unwrap();
        Arc::new(Self { udp: true, ..inner })
    }

    fn no_udp(name: &str, server: &str, port: u16, marker: u8) -> Arc<Self> {
        Self::new(name, server, port, marker) // udp defaults to false
    }

    /// UDP-capable like the real `DirectAdapter`.
    fn direct() -> Arc<Self> {
        let proxy = Self::new("DIRECT", "", 0, 0);
        let inner = Arc::try_unwrap(proxy).ok().unwrap();
        Arc::new(Self {
            adapter_type: AdapterType::Direct,
            addr_str: String::new(),
            udp: true,
            ..inner
        })
    }

    /// A terminal hop with no dialable address — the shape relay sees
    /// for a `RejectAdapter` member (`adapter_type != Direct`,
    /// `addr() == ""`; leaf adapters reach relay chains through the
    /// config's `Proxy` wrapper).
    fn reject(drop: bool) -> Arc<Self> {
        let proxy = Self::new(if drop { "REJECT-DROP" } else { "REJECT" }, "", 0, 0);
        let inner = Arc::try_unwrap(proxy).ok().unwrap();
        Arc::new(Self {
            adapter_type: if drop {
                AdapterType::RejectDrop
            } else {
                AdapterType::Reject
            },
            addr_str: String::new(),
            ..inner
        })
    }

    fn failing(name: &str, server: &str, port: u16, err: MeowError) -> Arc<Self> {
        let m = Self::new(name, server, port, 0);
        *m.fail_with.lock() = Some(err);
        m
    }

    fn dial_failing(name: &str, server: &str, port: u16, err: MeowError) -> Arc<Self> {
        let m = Self::new(name, server, port, 0);
        *m.dial_fail_with.lock() = Some(err);
        m
    }
}

// ── NopConn ────────────────────────────────────────────────────────────

/// A `ProxyConn` that accepts all writes and returns EOF on reads.
struct NopConn;

impl tokio::io::AsyncRead for NopConn {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        _buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(())) // EOF
    }
}

impl tokio::io::AsyncWrite for NopConn {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::task::Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

impl Unpin for NopConn {}
impl ProxyConn for NopConn {}

// ── NopPacketConn ──────────────────────────────────────────────────────

struct NopPacketConn;

#[async_trait]
impl ProxyPacketConn for NopPacketConn {
    async fn read_packet(&self, _buf: &mut [u8]) -> Result<(usize, SocketAddr)> {
        Err(MeowError::NotSupported("nop".into()))
    }
    async fn write_packet(&self, buf: &[u8], _addr: &SocketAddr) -> Result<usize> {
        Ok(buf.len())
    }
    fn local_addr(&self) -> Result<SocketAddr> {
        Ok("127.0.0.1:0".parse().unwrap())
    }
    fn close(&self) -> Result<()> {
        Ok(())
    }
}

// ── MockProxy: ProxyAdapter impl ───────────────────────────────────────

#[async_trait]
impl ProxyAdapter for MockProxy {
    fn name(&self) -> &str {
        &self.proxy_name
    }
    fn adapter_type(&self) -> AdapterType {
        self.adapter_type
    }
    fn addr(&self) -> &str {
        &self.addr_str
    }
    fn support_udp(&self) -> bool {
        self.udp
    }

    async fn dial_tcp(&self, metadata: &Metadata) -> Result<Box<dyn ProxyConn>> {
        *self.last_dial_host.lock() = Some(recorded_host(metadata));
        if let Some(err) = self.dial_fail_with.lock().take() {
            return Err(err);
        }
        Ok(Box::new(NopConn))
    }

    async fn dial_udp(&self, metadata: &Metadata) -> Result<Box<dyn ProxyPacketConn>> {
        self.udp_dials.lock().push(recorded_host(metadata));
        if self.udp {
            Ok(Box::new(NopPacketConn))
        } else {
            Err(MeowError::NotSupported("mock: no UDP".into()))
        }
    }

    async fn connect_over(
        &self,
        stream: Box<dyn ProxyConn>,
        metadata: &Metadata,
    ) -> Result<Box<dyn ProxyConn>> {
        *self.last_dial_host.lock() = Some(recorded_host(metadata));
        self.visits.lock().push(self.marker);
        if let Some(err) = self.fail_with.lock().take() {
            return Err(err);
        }
        Ok(stream)
    }

    fn health(&self) -> &ProxyHealth {
        &self.health
    }
}

impl Proxy for MockProxy {
    fn alive(&self) -> bool {
        true
    }
    fn alive_for_url(&self, _url: &str) -> bool {
        true
    }
    fn last_delay(&self) -> u16 {
        0
    }
    fn last_delay_for_url(&self, _url: &str) -> u16 {
        0
    }
    fn delay_history(&self) -> Vec<DelayHistory> {
        vec![]
    }
}

// ─── A. TCP relay chain — connect_over traversal ──────────────────────

// A1: two-hop chain — A.dial_tcp, B.connect_over
// upstream: adapter/outbound/relay.go::DialContext
// NOT direct connection to target — A receives meta_for_B as its target.
#[tokio::test]
async fn relay_two_hop_tcp_roundtrip() {
    let a = MockProxy::new("A", "10.0.0.1", 1080, 1);
    let b = MockProxy::new("B", "10.0.0.2", 1080, 2);
    let a_visits = Arc::clone(&a.visits);
    let b_visits = Arc::clone(&b.visits);

    let proxies: Vec<Arc<dyn Proxy>> = vec![a, b];
    let group = RelayGroup::new("two-hop", proxies);
    let meta = Metadata {
        host: "target.example".into(),
        dst_port: 443,
        ..Default::default()
    };

    group.dial_tcp(&meta).await.expect("relay_two_hop");

    // A used dial_tcp, NOT connect_over — visits must be empty.
    assert!(
        a_visits.lock().is_empty(),
        "hop 0 must use dial_tcp, NOT connect_over; A.visits={:?}",
        a_visits.lock()
    );
    // B used connect_over exactly once.
    assert_eq!(
        *b_visits.lock(),
        vec![2],
        "hop 1 must call connect_over once; B.visits={:?}",
        b_visits.lock()
    );
}

// A2: three-hop chain — A.dial_tcp, B.connect_over, C.connect_over
#[tokio::test]
async fn relay_three_hop_tcp_roundtrip() {
    let a = MockProxy::new("A", "10.0.0.1", 1080, 1);
    let b = MockProxy::new("B", "10.0.0.2", 1080, 2);
    let c = MockProxy::new("C", "10.0.0.3", 1080, 3);
    let a_visits = Arc::clone(&a.visits);
    let b_visits = Arc::clone(&b.visits);
    let c_visits = Arc::clone(&c.visits);

    let proxies: Vec<Arc<dyn Proxy>> = vec![a, b, c];
    let group = RelayGroup::new("three-hop", proxies);
    let meta = Metadata {
        host: "target.example".into(),
        dst_port: 443,
        ..Default::default()
    };

    group.dial_tcp(&meta).await.expect("relay_three_hop");

    assert!(a_visits.lock().is_empty(), "A.visits must be empty");
    assert_eq!(*b_visits.lock(), vec![2], "B.visits == [2]");
    assert_eq!(*c_visits.lock(), vec![3], "C.visits == [3]");
}

// A3: guard-rail — first hop uses dial_tcp, NOT connect_over
#[tokio::test]
async fn relay_first_hop_uses_dial_tcp_not_connect_over() {
    let a = MockProxy::new("A", "10.0.0.1", 1080, 1);
    let b = MockProxy::new("B", "10.0.0.2", 1080, 2);
    let a_visits = Arc::clone(&a.visits);

    let proxies: Vec<Arc<dyn Proxy>> = vec![a, b];
    let group = RelayGroup::new("guard-dial-tcp", proxies);
    let meta = Metadata {
        host: "target.example".into(),
        dst_port: 443,
        ..Default::default()
    };

    group.dial_tcp(&meta).await.unwrap();

    assert!(
        a_visits.lock().is_empty(),
        "hop 0 MUST NOT call connect_over; found visits={:?}",
        a_visits.lock()
    );
}

// A4: guard-rail — intermediate hops use connect_over, not dial_tcp
#[tokio::test]
async fn relay_intermediate_hops_use_connect_over() {
    let a = MockProxy::new("A", "10.0.0.1", 1080, 1);
    let b = MockProxy::new("B", "10.0.0.2", 1080, 2);
    let c = MockProxy::new("C", "10.0.0.3", 1080, 3);
    let a_visits = Arc::clone(&a.visits);
    let b_visits = Arc::clone(&b.visits);

    let proxies: Vec<Arc<dyn Proxy>> = vec![a, b, c];
    let group = RelayGroup::new("guard-connect-over", proxies);
    let meta = Metadata {
        host: "target.example".into(),
        dst_port: 443,
        ..Default::default()
    };

    group.dial_tcp(&meta).await.unwrap();

    assert!(a_visits.lock().is_empty(), "A must NOT use connect_over");
    assert!(
        !b_visits.lock().is_empty(),
        "B (middle hop) MUST use connect_over"
    );
}

// A5: guard-rail — each hop receives the NEXT hop's address as its dial target
// For chain [A→B→C→target]:
//   A.dial_tcp receives meta with host="10.0.0.2" (B's server)
//   B.connect_over receives meta with host="10.0.0.3" (C's server)
//   C.connect_over receives meta with host="target.example"
#[tokio::test]
async fn relay_each_hop_receives_next_hop_address() {
    let a = MockProxy::new("A", "10.0.0.1", 1080, 1);
    let b = MockProxy::new("B", "10.0.0.2", 1081, 2);
    let c = MockProxy::new("C", "10.0.0.3", 1082, 3);
    let a_host = Arc::clone(&a.last_dial_host);
    let b_host = Arc::clone(&b.last_dial_host);
    let c_host = Arc::clone(&c.last_dial_host);

    let proxies: Vec<Arc<dyn Proxy>> = vec![a, b, c];
    let group = RelayGroup::new("hop-address-check", proxies);
    let meta = Metadata {
        host: "target.example".into(),
        dst_port: 443,
        ..Default::default()
    };

    group.dial_tcp(&meta).await.unwrap();

    // A must be called with B's server address.
    assert_eq!(
        *a_host.lock(),
        Some("10.0.0.2".into()),
        "A must receive B's host; got {:?}",
        a_host.lock()
    );
    // B must be called with C's server address.
    assert_eq!(
        *b_host.lock(),
        Some("10.0.0.3".into()),
        "B must receive C's host; got {:?}",
        b_host.lock()
    );
    // C (final hop) must receive the actual target.
    assert_eq!(
        *c_host.lock(),
        Some("target.example".into()),
        "C must receive final target; got {:?}",
        c_host.lock()
    );
}

#[tokio::test]
async fn relay_resolves_group_at_later_hop() {
    let entry = MockProxy::new("entry", "10.0.0.1", 1080, 1);
    let exit = MockProxy::new("exit", "10.0.0.2", 1081, 2);
    let entry_host = Arc::clone(&entry.last_dial_host);
    let exit_host = Arc::clone(&exit.last_dial_host);
    let exit_visits = Arc::clone(&exit.visits);
    let exit_group: Arc<dyn Proxy> = Arc::new(SelectorGroup::new("exit-group", vec![exit]));

    let group = RelayGroup::new("group-hop", vec![entry, exit_group]);
    let target = Metadata {
        host: "target.example".into(),
        dst_port: 443,
        ..Default::default()
    };

    group.dial_tcp(&target).await.expect("group relay hop");

    assert_eq!(*entry_host.lock(), Some("10.0.0.2".into()));
    assert_eq!(*exit_host.lock(), Some("target.example".into()));
    assert_eq!(*exit_visits.lock(), vec![2]);
}

/// `metadata_for_proxy` must type IP-literal server addrs as `dst_ip`
/// (ATYP 1/4 / SocksAddr::Ip for the preceding hop's CONNECT) rather
/// than stuffing them into `host` as pseudo-domains.
#[test]
fn metadata_for_proxy_types_ip_literals_as_dst_ip() {
    let p: Arc<dyn Proxy> = MockProxy::new("ip-hop", "203.0.113.7", 8443, 1);
    let m = metadata_for_proxy(&p, false);
    assert_eq!(m.dst_ip, Some("203.0.113.7".parse().expect("v4")));
    assert!(m.host.is_empty());
    assert_eq!(m.dst_port, 8443);
    assert_eq!(m.network, meow_common::Network::Tcp);
    // Internal chained dial, not an HTTP inbound.
    assert_eq!(m.conn_type, meow_common::ConnType::Inner);
}

#[test]
fn metadata_for_proxy_strips_ipv6_brackets_into_dst_ip() {
    let p: Arc<dyn Proxy> = MockProxy::new("v6-hop", "[2001:db8::1]", 1080, 1);
    let m = metadata_for_proxy(&p, false);
    assert_eq!(m.dst_ip, Some("2001:db8::1".parse().expect("v6")));
    assert_eq!(m.dst_port, 1080);
}

/// `addr()` is always `format!("{server}:{port}")`, so even an
/// unbracketed IPv6 server like `"2001:db8::1"` yields
/// `"2001:db8::1:1080"` — `rfind(':')` still isolates the true port and
/// the remainder parses as an `IpAddr`.  Pin that invariant.
#[test]
fn metadata_for_proxy_unbracketed_ipv6_server() {
    let p: Arc<dyn Proxy> = MockProxy::new("v6-bare", "2001:db8::1", 1080, 1);
    let m = metadata_for_proxy(&p, false);
    assert_eq!(m.dst_ip, Some("2001:db8::1".parse().expect("v6")));
    assert_eq!(m.dst_port, 1080);
    assert!(m.host.is_empty());
}

#[test]
fn metadata_for_proxy_keeps_domains_in_host() {
    let p: Arc<dyn Proxy> = MockProxy::new("dns-hop", "example.com", 443, 1);
    let m = metadata_for_proxy(&p, false);
    assert_eq!(m.host.as_str(), "example.com");
    assert!(m.dst_ip.is_none());
    assert_eq!(m.dst_port, 443);
}

/// #555: the next-hop rebuild must carry the connection's `internal`
/// marker — a relay hop that is itself `dialer-proxy`-chained to a lazy
/// front hop must not count housekeeping relays as use.  Both arms of
/// `metadata_for_next_hop` (rebuild and `final_target` clone) must agree.
#[test]
fn metadata_for_next_hop_carries_internal_marker() {
    let p: Arc<dyn Proxy> = MockProxy::new("hop", "example.com", 443, 1);
    let proxies = vec![p];
    let internal_target = Metadata {
        internal: true,
        ..Metadata::default()
    };
    let user_target = Metadata::default();

    let m = metadata_for_next_hop(&proxies, 0, &internal_target);
    assert!(m.is_internal(), "rebuilt next-hop meta keeps the marker");
    let m = metadata_for_next_hop(&proxies, 0, &user_target);
    assert!(!m.is_internal(), "user relay stays user-classed");

    // The `final_target.clone()` arm (no dialable next hop) preserves
    // the whole metadata — internal included — by construction.
    let undialable: Arc<dyn Proxy> = MockProxy::reject(false);
    // `find` skips empty addr → falls back to the clone arm.
    let m = metadata_for_next_hop(&[undialable], 0, &internal_target);
    assert!(m.is_internal());
}

/// A nested relay whose members include a selector must resolve that
/// selector to its leaf before calling `connect_over`, and the nested
/// group must be *flattened* into the outer chain: `outer = [a,
/// inner[sel→b, c]]` runs as `[a, b, c]` so that `a` dials the inner
/// chain's entry point (b's server) rather than the group's own empty
/// `addr()`.
#[tokio::test]
async fn nested_connect_over_resolves_group_members() {
    let a = MockProxy::new("a", "10.0.0.1", 1080, 1);
    let b = MockProxy::new("b", "10.0.0.2", 1081, 2);
    let c = MockProxy::new("c", "10.0.0.3", 1082, 3);
    let a_host = Arc::clone(&a.last_dial_host);
    let b_visits = Arc::clone(&b.visits);
    let b_host = Arc::clone(&b.last_dial_host);
    let c_visits = Arc::clone(&c.visits);
    let c_host = Arc::clone(&c.last_dial_host);
    let sel: Arc<dyn Proxy> = Arc::new(SelectorGroup::new("sel", vec![b]));
    let inner: Arc<dyn Proxy> = Arc::new(RelayGroup::new("inner", vec![sel, c]));
    let outer = RelayGroup::new("outer", vec![a, inner]);
    let target = Metadata {
        host: "t.example".into(),
        dst_port: 443,
        ..Default::default()
    };

    outer.dial_tcp(&target).await.expect("nested relay");

    // `a` dials the inner chain's entry point — b's server, not the
    // relay group's empty addr().
    assert_eq!(*a_host.lock(), Some("10.0.0.2".into()));
    // Inner chain ran sel→b (resolved) as a connect_over hop aimed at
    // c's server, then c as the final hop aimed at the target.
    assert_eq!(*b_visits.lock(), vec![2]);
    assert_eq!(*b_host.lock(), Some("10.0.0.3".into()));
    assert_eq!(*c_visits.lock(), vec![3]);
    assert_eq!(*c_host.lock(), Some("t.example".into()));
}

/// A `dialer-proxy` member whose inner outbound is itself a relay group
/// flattens the same way: `outer = [a, dpa[inner=[p,q]]]` runs as
/// `[a, p, q]`, so `a` dials the inner chain's entry point (p's server)
/// rather than the group's empty `addr()`.  The per-outbound dialer is
/// deliberately unresolvable here — inside a relay chain it must never
/// be consulted.
#[tokio::test]
async fn nested_dialer_proxy_relay_member_flattens_inner_chain() {
    let a = MockProxy::new("a", "10.0.0.1", 1080, 1);
    let p = MockProxy::new("p", "10.0.0.2", 1081, 2);
    let q = MockProxy::new("q", "10.0.0.3", 1082, 3);
    let a_host = Arc::clone(&a.last_dial_host);
    let p_visits = Arc::clone(&p.visits);
    let p_host = Arc::clone(&p.last_dial_host);
    let q_host = Arc::clone(&q.last_dial_host);

    let inner_relay: Arc<dyn Proxy> = Arc::new(RelayGroup::new("inner", vec![p, q]));
    let dpa: Arc<dyn Proxy> = Arc::new(DialerProxyAdapter::new(
        inner_relay,
        crate::dialer::DialerTarget::new(
            "never-resolved",
            &crate::dialer::ProxyRegistry::default(),
        ),
    ));
    let outer = RelayGroup::new("outer", vec![a, dpa]);
    let target = Metadata {
        host: "t.example".into(),
        dst_port: 443,
        ..Default::default()
    };

    outer
        .dial_tcp(&target)
        .await
        .expect("dpa-wrapped relay member");

    assert_eq!(*a_host.lock(), Some("10.0.0.2".into()));
    assert_eq!(*p_visits.lock(), vec![2]);
    assert_eq!(*p_host.lock(), Some("10.0.0.3".into()));
    assert_eq!(*q_host.lock(), Some("t.example".into()));
}

/// `DPA(DPA(relay))` must peel both wrapper layers — a single-level peel
/// would retain the outer DPA (empty `addr()`), silently misrouting the
/// preceding hop's dial to the *next* member's server.
#[tokio::test]
async fn nested_dialer_proxy_layers_all_peel_to_inner_relay() {
    let a = MockProxy::new("a", "10.0.0.1", 1080, 1);
    let p = MockProxy::new("p", "10.0.0.2", 1081, 2);
    let q = MockProxy::new("q", "10.0.0.3", 1082, 3);
    let a_host = Arc::clone(&a.last_dial_host);
    let p_visits = Arc::clone(&p.visits);
    let q_host = Arc::clone(&q.last_dial_host);

    let dead = || {
        crate::dialer::DialerTarget::new("never-resolved", &crate::dialer::ProxyRegistry::default())
    };
    let relay: Arc<dyn Proxy> = Arc::new(RelayGroup::new("inner", vec![p, q]));
    let dpa_inner: Arc<dyn Proxy> = Arc::new(DialerProxyAdapter::new(relay, dead()));
    let dpa_outer: Arc<dyn Proxy> = Arc::new(DialerProxyAdapter::new(dpa_inner, dead()));
    let outer = RelayGroup::new("outer", vec![a, dpa_outer]);
    let target = Metadata {
        host: "t.example".into(),
        dst_port: 443,
        ..Default::default()
    };

    outer
        .dial_tcp(&target)
        .await
        .expect("doubly-wrapped relay member");

    assert_eq!(*a_host.lock(), Some("10.0.0.2".into()));
    assert_eq!(*p_visits.lock(), vec![2]);
    assert_eq!(*q_host.lock(), Some("t.example".into()));
}

/// A `dialer-proxy` member at hop 0 must NOT be peeled — its `dial_tcp`
/// preserves the configured front dialer. With an unresolvable dialer
/// the dial must fail loudly, not silently splice the inner chain.
#[tokio::test]
async fn dialer_proxy_at_hop0_keeps_its_front_dialer() {
    let p = MockProxy::new("p", "10.0.0.2", 1081, 2);
    let q = MockProxy::new("q", "10.0.0.3", 1082, 3);
    let x = MockProxy::new("x", "10.0.0.4", 1083, 4);
    let p_visits = Arc::clone(&p.visits);

    let relay: Arc<dyn Proxy> = Arc::new(RelayGroup::new("inner", vec![p, q]));
    let dpa: Arc<dyn Proxy> = Arc::new(DialerProxyAdapter::new(
        relay,
        crate::dialer::DialerTarget::new(
            "never-resolved",
            &crate::dialer::ProxyRegistry::default(),
        ),
    ));
    let outer = RelayGroup::new("outer", vec![dpa, x]);
    let target = Metadata {
        host: "t.example".into(),
        dst_port: 443,
        ..Default::default()
    };

    let err = outer
        .dial_tcp(&target)
        .await
        .err()
        .expect("hop-0 DPA must run its own dial_tcp and fail on the unresolvable dialer");
    assert!(
        err.to_string().contains("never-resolved") || err.to_string().contains("dialer"),
        "error must name the missing dialer, got: {err}"
    );
    // Peeling would have spliced the inner chain and run `p` — visits
    // must stay empty to prove no splice happened.
    assert!(p_visits.lock().is_empty(), "hop-0 DPA must not be peeled");
}

/// Expansion deeper than `MAX_FLATTEN_DEPTH` is a hard error — a
/// hand-constructed cyclic or absurdly deep group graph must fail the
/// dial outright, not silently retain an unexpanded relay mid-chain
/// (which would misroute the preceding hop to the next member's server).
#[tokio::test]
async fn flatten_beyond_max_depth_is_a_hard_error() {
    fn leaf(name: &str) -> Arc<dyn Proxy> {
        MockProxy::new(name, "10.0.0.9", 1090, 9)
    }
    // 20-deep nesting exceeds MAX_FLATTEN_DEPTH (16).
    let mut inner: Arc<dyn Proxy> = leaf("leaf0");
    for i in 0..20 {
        inner = Arc::new(RelayGroup::new("r", vec![inner, leaf(&format!("leaf{i}"))]));
    }
    let outer = RelayGroup::new("outer", vec![leaf("a"), inner]);
    let target = Metadata {
        host: "t.example".into(),
        dst_port: 443,
        ..Default::default()
    };

    let err = outer
        .dial_tcp(&target)
        .await
        .err()
        .expect("over-depth expansion must fail");
    assert!(
        err.to_string().contains("depth"),
        "error must name the depth bound, got: {err}"
    );
}

#[tokio::test]
async fn relay_skips_direct_when_choosing_next_hop_target() {
    let entry = MockProxy::new("entry", "10.0.0.1", 1080, 1);
    let exit = MockProxy::new("exit", "10.0.0.2", 1081, 2);
    let entry_host = Arc::clone(&entry.last_dial_host);
    let exit_host = Arc::clone(&exit.last_dial_host);
    let direct: Arc<dyn Proxy> = MockProxy::direct();

    let group = RelayGroup::new("direct-hop", vec![entry, direct, exit]);
    let target = Metadata {
        host: "target.example".into(),
        dst_port: 443,
        ..Default::default()
    };

    group.dial_tcp(&target).await.expect("DIRECT relay hop");

    assert_eq!(*entry_host.lock(), Some("10.0.0.2".into()));
    assert_eq!(*exit_host.lock(), Some("target.example".into()));
}

#[tokio::test]
async fn relay_targets_destination_when_direct_is_last_hop() {
    let entry = MockProxy::new("entry", "10.0.0.1", 1080, 1);
    let entry_host = Arc::clone(&entry.last_dial_host);
    let direct: Arc<dyn Proxy> = MockProxy::direct();

    let group = RelayGroup::new("direct-final-hop", vec![entry, direct]);
    let target = Metadata {
        host: "target.example".into(),
        dst_port: 443,
        ..Default::default()
    };

    group
        .dial_tcp(&target)
        .await
        .expect("final DIRECT relay hop");

    assert_eq!(*entry_host.lock(), Some("target.example".into()));
}

// ─── B. Parse-time validation ─────────────────────────────────────────
// These are tested via the config parser (proxy_parser.rs).
// We add guard-rail variants here for the RelayGroup constructor itself.

// B-guard: debug_assert fires when proxies.len() < 2 in debug builds
// (parse-time hard-error handles production; debug_assert catches test mistakes)
// Cannot be a regular #[test] in release builds — documented as spec guard.
// See G2 for the grep-based guard-rail.

// ─── C. UDP relay ─────────────────────────────────────────────────────
//
// UDP does not chain (see the module docs). It leaves from the one
// proxy hop that remains after DIRECT hops are dropped, or fails closed.

fn udp_target() -> Metadata {
    Metadata {
        network: meow_common::Network::Udp,
        host: "target.example".into(),
        dst_port: 53,
        ..Default::default()
    }
}

// C1: `[DIRECT, B]` exits at B, like its TCP. Hop 0 used to send the
// UDP here, so it left straight from the host.
#[tokio::test]
async fn relay_udp_direct_then_proxy_exits_at_proxy() {
    let direct = MockProxy::direct();
    let b = MockProxy::new_udp("B", "10.0.0.2", 1080, 2);
    let direct_udp = Arc::clone(&direct.udp_dials);
    let b_udp = Arc::clone(&b.udp_dials);

    let group = RelayGroup::new("udp-direct-b", vec![direct, b]);

    assert!(group.support_udp(), "B is the exit and supports UDP");
    group.dial_udp(&udp_target()).await.expect("dial_udp via B");
    assert_eq!(*b_udp.lock(), vec!["target.example".to_string()]);
    assert!(direct_udp.lock().is_empty(), "DIRECT must not send the UDP");
}

// C2: two proxy hops would need chained UDP, so the dial fails closed
// before any hop runs. Hop 0 used to send the UDP here, so it left
// from A instead of B.
#[tokio::test]
async fn relay_udp_two_proxy_hops_fail_closed() {
    let a = MockProxy::new_udp("A", "10.0.0.1", 1080, 1);
    let b = MockProxy::new_udp("B", "10.0.0.2", 1080, 2);
    let a_udp = Arc::clone(&a.udp_dials);
    let b_udp = Arc::clone(&b.udp_dials);
    let a_host = Arc::clone(&a.last_dial_host);
    let b_visits = Arc::clone(&b.visits);

    let group = RelayGroup::new("udp-a-b", vec![a, b]);

    assert!(!group.support_udp());
    let err = group
        .dial_udp(&udp_target())
        .await
        .err()
        .expect("must fail closed");
    assert!(
        matches!(err, MeowError::UdpNotSupported),
        "must be UdpNotSupported; got {err:?}"
    );
    assert!(a_udp.lock().is_empty(), "A.dial_udp must not run");
    assert!(b_udp.lock().is_empty(), "B.dial_udp must not run");
    assert!(a_host.lock().is_none(), "A must not dial TCP either");
    assert!(b_visits.lock().is_empty(), "B must not connect_over either");
}

// C3: a DIRECT hop between two proxies does not reduce the count, and
// once two proxy hops remain, their own UDP support does not matter.
#[tokio::test]
async fn relay_udp_multi_hop_fails_closed_for_every_shape() {
    fn hop(name: &str, udp: bool) -> Arc<MockProxy> {
        match (name, udp) {
            ("DIRECT", _) => MockProxy::direct(),
            (_, true) => MockProxy::new_udp(name, "10.0.0.1", 1080, 1),
            (_, false) => MockProxy::no_udp(name, "10.0.0.1", 1080, 1),
        }
    }
    let shapes: [&[(&str, bool)]; 4] = [
        &[("A", true), ("DIRECT", true), ("B", true)],
        &[("A", false), ("B", true)],
        &[("A", true), ("B", false)],
        &[("A", true), ("B", true), ("C", true)],
    ];
    for shape in shapes {
        let mocks: Vec<Arc<MockProxy>> = shape.iter().map(|&(n, u)| hop(n, u)).collect();
        let proxies: Vec<Arc<dyn Proxy>> = mocks
            .iter()
            .map(|m| Arc::clone(m) as Arc<dyn Proxy>)
            .collect();
        let group = RelayGroup::new("udp-multi", proxies);

        assert!(!group.support_udp(), "{shape:?}");
        let err = group
            .dial_udp(&udp_target())
            .await
            .err()
            .expect("must fail closed");
        assert!(
            matches!(err, MeowError::UdpNotSupported),
            "{shape:?}: got {err:?}"
        );
        for m in &mocks {
            assert!(
                m.udp_dials.lock().is_empty(),
                "{shape:?}: {} must not dial UDP",
                m.name()
            );
        }
    }
}

// C4: an all-DIRECT chain is a direct path, and its first DIRECT hop
// sends the UDP.
#[tokio::test]
async fn relay_udp_all_direct_sends_from_direct() {
    let d1 = MockProxy::direct();
    let d2 = MockProxy::direct();
    let d1_udp = Arc::clone(&d1.udp_dials);
    let d2_udp = Arc::clone(&d2.udp_dials);

    let group = RelayGroup::new("udp-direct-direct", vec![d1, d2]);

    assert!(group.support_udp());
    group.dial_udp(&udp_target()).await.expect("direct UDP");
    assert_eq!(*d1_udp.lock(), vec!["target.example".to_string()]);
    assert!(d2_udp.lock().is_empty());
}

// C5: the exit's own capability decides. An exit without UDP fails
// closed with `UdpNotSupported`, not the exit's own error, and its
// `dial_udp` never runs.
#[tokio::test]
async fn relay_udp_exit_without_udp_fails_closed() {
    for exit_first in [true, false] {
        let a = MockProxy::no_udp("A", "10.0.0.1", 1080, 1);
        let a_udp = Arc::clone(&a.udp_dials);
        let direct: Arc<dyn Proxy> = MockProxy::direct();
        let proxies: Vec<Arc<dyn Proxy>> = if exit_first {
            vec![a, direct]
        } else {
            vec![direct, a]
        };
        let group = RelayGroup::new("udp-no-udp-exit", proxies);

        assert!(!group.support_udp(), "exit_first={exit_first}");
        let err = group
            .dial_udp(&udp_target())
            .await
            .err()
            .expect("must fail closed");
        assert!(
            matches!(err, MeowError::UdpNotSupported),
            "exit_first={exit_first}: got {err:?}"
        );
        assert!(a_udp.lock().is_empty(), "exit_first={exit_first}");
    }
}

// C6: a group or nested relay member that resolves to one proxy hop
// behaves like that leaf.
#[tokio::test]
async fn relay_udp_group_member_resolving_to_one_hop_acts_as_leaf() {
    // Selector hop.
    let b = MockProxy::new_udp("B", "10.0.0.2", 1080, 2);
    let b_udp = Arc::clone(&b.udp_dials);
    let sel: Arc<dyn Proxy> = Arc::new(SelectorGroup::new("sel", vec![b]));
    let group = RelayGroup::new("udp-sel", vec![MockProxy::direct(), sel]);
    assert!(group.support_udp());
    group.dial_udp(&udp_target()).await.expect("via selector");
    assert_eq!(*b_udp.lock(), vec!["target.example".to_string()]);

    // Selector hop whose selection has no UDP.
    let a = MockProxy::no_udp("A", "10.0.0.1", 1080, 1);
    let a_udp = Arc::clone(&a.udp_dials);
    let sel: Arc<dyn Proxy> = Arc::new(SelectorGroup::new("sel", vec![a]));
    let group = RelayGroup::new("udp-sel-no-udp", vec![MockProxy::direct(), sel]);
    assert!(!group.support_udp());
    let err = group.dial_udp(&udp_target()).await.err().expect("no UDP");
    assert!(matches!(err, MeowError::UdpNotSupported), "got {err:?}");
    assert!(a_udp.lock().is_empty());

    // Nested relay: `[DIRECT, inner[DIRECT, C]]` flattens to one proxy
    // hop.
    let c = MockProxy::new_udp("C", "10.0.0.3", 1080, 3);
    let c_udp = Arc::clone(&c.udp_dials);
    let inner: Arc<dyn Proxy> = Arc::new(RelayGroup::new("inner", vec![MockProxy::direct(), c]));
    let group = RelayGroup::new("udp-nested", vec![MockProxy::direct(), inner]);
    assert!(group.support_udp());
    group
        .dial_udp(&udp_target())
        .await
        .expect("via nested relay");
    assert_eq!(*c_udp.lock(), vec!["target.example".to_string()]);

    // Nested relay adding a second proxy hop: fail closed.
    let d = MockProxy::new_udp("D", "10.0.0.4", 1080, 4);
    let e = MockProxy::new_udp("E", "10.0.0.5", 1080, 5);
    let d_udp = Arc::clone(&d.udp_dials);
    let e_udp = Arc::clone(&e.udp_dials);
    let inner: Arc<dyn Proxy> = Arc::new(RelayGroup::new("inner", vec![MockProxy::direct(), e]));
    let group = RelayGroup::new("udp-nested-two", vec![d, inner]);
    assert!(!group.support_udp());
    let err = group.dial_udp(&udp_target()).await.err().expect("two hops");
    assert!(matches!(err, MeowError::UdpNotSupported), "got {err:?}");
    assert!(d_udp.lock().is_empty() && e_udp.lock().is_empty());
}

// C7: `support_udp` resolves groups with a peek. It must not count as
// use of a lazy group or advance round-robin, or every UDP rule match
// would move the next dial's pick.
#[tokio::test]
async fn relay_support_udp_peek_does_not_touch_groups() {
    let b1 = MockProxy::new_udp("B1", "10.0.0.2", 1080, 2);
    let b2 = MockProxy::new_udp("B2", "10.0.0.3", 1080, 3);
    let b1_udp = Arc::clone(&b1.udp_dials);
    let b2_udp = Arc::clone(&b2.udp_dials);
    let lb = Arc::new(LoadBalanceGroup::new(
        "lb",
        vec![b1, b2],
        LbStrategy::RoundRobin,
    ));
    let proxies: Vec<Arc<dyn Proxy>> = vec![MockProxy::direct(), Arc::clone(&lb) as _];
    let group = RelayGroup::new("udp-lb", proxies);

    for _ in 0..3 {
        assert!(group.support_udp());
    }
    assert_eq!(lb.usage_generation(), 0, "a peek is not a use");

    group
        .dial_udp(&udp_target())
        .await
        .expect("via load-balance");
    assert_eq!(lb.usage_generation(), 1, "the dial is a use");
    // The peeks left the round-robin counter alone, so the first dial
    // still gets B1.
    assert_eq!(b1_udp.lock().len(), 1);
    assert!(b2_udp.lock().is_empty());
}

// ─── D. Error handling — RelayHopFailed ───────────────────────────────

// D1: proxy[1] fails → RelayHopFailed { hop: 1, .. }
// NOT a raw inner error. NOT anyhow::Error.
#[tokio::test]
async fn relay_hop_failure_includes_hop_index() {
    let a = MockProxy::new("A", "10.0.0.1", 1080, 1);
    let b = MockProxy::failing("B", "10.0.0.2", 1080, MeowError::Proxy("inner".into()));

    let proxies: Vec<Arc<dyn Proxy>> = vec![a, b];
    let group = RelayGroup::new("hop1-fail", proxies);
    let meta = Metadata {
        host: "target.example".into(),
        dst_port: 443,
        ..Default::default()
    };

    let err = group.dial_tcp(&meta).await.err().expect("must error");
    assert!(
        matches!(err, MeowError::RelayHopFailed { hop: 1, .. }),
        "error must be RelayHopFailed at hop 1; got {err:?}"
    );
}

// D2: proxy[0] dial_tcp fails → RelayHopFailed { hop: 0, .. }
#[tokio::test]
async fn relay_first_hop_failure_includes_hop_0() {
    let a = MockProxy::dial_failing(
        "A",
        "10.0.0.1",
        1080,
        MeowError::Proxy("conn refused".into()),
    );
    let b = MockProxy::new("B", "10.0.0.2", 1080, 2);

    let proxies: Vec<Arc<dyn Proxy>> = vec![a, b];
    let group = RelayGroup::new("hop0-fail", proxies);
    let meta = Metadata {
        host: "target.example".into(),
        dst_port: 443,
        ..Default::default()
    };

    let err = group.dial_tcp(&meta).await.err().expect("must error");
    assert!(
        matches!(err, MeowError::RelayHopFailed { hop: 0, .. }),
        "error must be RelayHopFailed at hop 0; got {err:?}"
    );
}

// D3: proxy[2] (last) fails in 3-hop chain → RelayHopFailed { hop: 2, .. }
#[tokio::test]
async fn relay_last_hop_failure_includes_correct_index() {
    let a = MockProxy::new("A", "10.0.0.1", 1080, 1);
    let b = MockProxy::new("B", "10.0.0.2", 1080, 2);
    let c = MockProxy::failing("C", "10.0.0.3", 1080, MeowError::Proxy("timeout".into()));

    let proxies: Vec<Arc<dyn Proxy>> = vec![a, b, c];
    let group = RelayGroup::new("hop2-fail", proxies);
    let meta = Metadata {
        host: "target.example".into(),
        dst_port: 443,
        ..Default::default()
    };

    let err = group.dial_tcp(&meta).await.err().expect("must error");
    assert!(
        matches!(err, MeowError::RelayHopFailed { hop: 2, .. }),
        "error must be RelayHopFailed at hop 2; got {err:?}"
    );
}

// D4: RelayHopFailed.source contains the original inner error
#[tokio::test]
async fn relay_hop_failure_source_is_inner_error() {
    let a = MockProxy::new("A", "10.0.0.1", 1080, 1);
    let b = MockProxy::failing("B", "10.0.0.2", 1080, MeowError::Proxy("inner-msg".into()));

    let proxies: Vec<Arc<dyn Proxy>> = vec![a, b];
    let group = RelayGroup::new("source-check", proxies);
    let meta = Metadata {
        host: "target.example".into(),
        dst_port: 443,
        ..Default::default()
    };

    let err = group.dial_tcp(&meta).await.err().expect("must error");
    match err {
        MeowError::RelayHopFailed { hop: 1, source } => {
            assert!(
                matches!(*source, MeowError::Proxy(_)),
                "source must be inner Proxy error; got {source:?}"
            );
            let msg = source.to_string();
            assert!(
                msg.contains("inner-msg"),
                "source message must contain 'inner-msg'; got {msg:?}"
            );
        }
        other => panic!("expected RelayHopFailed {{ hop: 1, .. }}; got {other:?}"),
    }
}

// D5: guard-rail — no anyhow at public boundary
// (verified by the absence of anyhow::Context / .context( in relay.rs;
// cannot be a #[test] but documented here per test plan G1/D5)
//
// grep "anyhow::Context\|\.context(" crates/meow-proxy/src/group/relay.rs
// must return zero matches.

// ─── E. Nested relay (relay-of-relay) ─────────────────────────────────

// E1: outer 2-hop relay where proxy[0] is itself a 2-hop RelayGroup.
// Effective sequence: A.dial_tcp → B.connect_over → C.connect_over → D.connect_over.
// MockProxy implements connect_over today; unignored per architect R-M2 review.
#[tokio::test]
async fn relay_nested_relay_group() {
    let a = MockProxy::new("A", "10.0.0.1", 1080, 1);
    let b = MockProxy::new("B", "10.0.0.2", 1080, 2);
    let c = MockProxy::new("C", "10.0.0.3", 1080, 3);
    let d = MockProxy::new("D", "10.0.0.4", 1080, 4);
    let b_visits = Arc::clone(&b.visits);
    let c_visits = Arc::clone(&c.visits);
    let d_visits = Arc::clone(&d.visits);

    let inner_proxies: Vec<Arc<dyn Proxy>> = vec![a, b, c];
    let inner_relay: Arc<dyn Proxy> = Arc::new(RelayGroup::new("inner", inner_proxies));

    let outer_proxies: Vec<Arc<dyn Proxy>> = vec![inner_relay, d];
    let outer_group = RelayGroup::new("outer", outer_proxies);

    let meta = Metadata {
        host: "target.example".into(),
        dst_port: 443,
        ..Default::default()
    };
    outer_group.dial_tcp(&meta).await.expect("nested relay");

    assert_eq!(*b_visits.lock(), vec![2]);
    assert_eq!(*c_visits.lock(), vec![3]);
    assert_eq!(*d_visits.lock(), vec![4]);
}

// ─── H. Fail-closed hops & first-hop dialer-proxy ─────────────────────

/// A non-DIRECT member with no dialable address (REJECT, an
/// unresolvable group, …) is terminal — its `connect_over` errors — so
/// the chain must fail during flattening, before the preceding hop is
/// told to open a real connection to whatever comes after it.  The old
/// behaviour skipped the member when choosing the next hop's target,
/// which made `[entry, REJECT]` dial the *final target* first: the
/// reject fired, but only after the destination had been contacted.
#[tokio::test]
async fn reject_final_hop_fails_before_preceding_hop_dials() {
    let entry = MockProxy::new("entry", "10.0.0.1", 1080, 1);
    let entry_host = Arc::clone(&entry.last_dial_host);
    let reject: Arc<dyn Proxy> = MockProxy::reject(false);

    let group = RelayGroup::new("reject-final", vec![entry, reject]);
    let target = Metadata {
        host: "target.example".into(),
        dst_port: 443,
        ..Default::default()
    };

    let err = group.dial_tcp(&target).await.err().expect("must fail");
    assert!(
        matches!(err, MeowError::RelayHopFailed { hop: 1, .. }),
        "failure must be attributed to the REJECT hop; got {err:?}"
    );
    assert!(
        err.to_string().contains("REJECT"),
        "error must name the offending member; got: {err}"
    );
    assert!(
        entry_host.lock().is_none(),
        "preceding hop must not dial the final target: {:?}",
        entry_host.lock()
    );
}

/// Same for a middle position: nothing may touch the network.
#[tokio::test]
async fn reject_middle_hop_fails_before_any_dial() {
    let entry = MockProxy::new("entry", "10.0.0.1", 1080, 1);
    let exit = MockProxy::new("exit", "10.0.0.2", 1081, 2);
    let entry_host = Arc::clone(&entry.last_dial_host);
    let exit_visits = Arc::clone(&exit.visits);
    let reject: Arc<dyn Proxy> = MockProxy::reject(false);

    let group = RelayGroup::new("reject-mid", vec![entry, reject, exit]);
    let target = Metadata {
        host: "target.example".into(),
        dst_port: 443,
        ..Default::default()
    };

    let err = group.dial_tcp(&target).await.err().expect("must fail");
    assert!(
        matches!(err, MeowError::RelayHopFailed { hop: 1, .. }),
        "failure must be attributed to the REJECT hop; got {err:?}"
    );
    assert!(entry_host.lock().is_none(), "no hop may perform I/O");
    assert!(exit_visits.lock().is_empty());
}

/// REJECT-DROP at hop 0 must not burn the dial timeout inside its
/// black-hole sleep: flattening flags it before any hop runs.
#[tokio::test]
async fn reject_drop_first_hop_fails_fast() {
    let reject: Arc<dyn Proxy> = MockProxy::reject(true);
    let exit = MockProxy::new("exit", "10.0.0.2", 1081, 2);
    let exit_visits = Arc::clone(&exit.visits);

    let group = RelayGroup::new("reject-first", vec![reject, exit]);
    let target = Metadata {
        host: "target.example".into(),
        dst_port: 443,
        ..Default::default()
    };

    let err = group.dial_tcp(&target).await.err().expect("must fail");
    assert!(
        matches!(err, MeowError::RelayHopFailed { hop: 0, .. }),
        "failure must be attributed to hop 0; got {err:?}"
    );
    assert!(exit_visits.lock().is_empty());
}

/// `[DIRECT, REJECT]`: the DIRECT exemption lets hop 0 pass the check
/// (empty addr is legal for a transparent hop), but the chain still
/// fails closed at hop 1 — before DIRECT opens a real socket.
#[tokio::test]
async fn direct_entry_reject_fails_before_direct_dials() {
    let direct = MockProxy::direct();
    let direct_host = Arc::clone(&direct.last_dial_host);
    let reject: Arc<dyn Proxy> = MockProxy::reject(false);

    let group = RelayGroup::new("direct-reject", vec![direct, reject]);
    let target = Metadata {
        host: "target.example".into(),
        dst_port: 443,
        ..Default::default()
    };

    let err = group.dial_tcp(&target).await.err().expect("must fail");
    assert!(
        matches!(err, MeowError::RelayHopFailed { hop: 1, .. }),
        "failure must be attributed to the REJECT hop; got {err:?}"
    );
    assert!(direct_host.lock().is_none(), "DIRECT must not dial");
}

/// The fail-closed check covers every member that resolves to nothing
/// dialable — an empty Selector stays a group (`adapter_type !=
/// Direct`, `addr() == ""`), so it is terminal mid-chain like REJECT.
#[tokio::test]
async fn unresolvable_group_middle_hop_fails_before_any_dial() {
    let entry = MockProxy::new("entry", "10.0.0.1", 1080, 1);
    let exit = MockProxy::new("exit", "10.0.0.2", 1081, 2);
    let entry_host = Arc::clone(&entry.last_dial_host);
    let sel: Arc<dyn Proxy> = Arc::new(SelectorGroup::new("sel", vec![]));

    let group = RelayGroup::new("sel-mid", vec![entry, sel, exit]);
    let target = Metadata {
        host: "target.example".into(),
        dst_port: 443,
        ..Default::default()
    };

    let err = group.dial_tcp(&target).await.err().expect("must fail");
    assert!(
        matches!(err, MeowError::RelayHopFailed { hop: 1, .. }),
        "failure must be attributed to the group hop; got {err:?}"
    );
    assert!(
        err.to_string().contains("sel"),
        "error must name the offending member; got: {err}"
    );
    assert!(entry_host.lock().is_none(), "no hop may perform I/O");
}

/// Error hop indices are flattened positions: a REJECT spliced in from
/// a nested relay is reported at its global index.
#[tokio::test]
async fn reject_inside_nested_relay_reports_flattened_index() {
    let a = MockProxy::new("a", "10.0.0.1", 1080, 1);
    let b = MockProxy::new("b", "10.0.0.3", 1082, 3);
    let a_host = Arc::clone(&a.last_dial_host);
    let reject: Arc<dyn Proxy> = MockProxy::reject(false);

    let inner: Arc<dyn Proxy> = Arc::new(RelayGroup::new("inner", vec![reject, b]));
    let outer = RelayGroup::new("outer", vec![a, inner]);
    let target = Metadata {
        host: "target.example".into(),
        dst_port: 443,
        ..Default::default()
    };

    let err = outer.dial_tcp(&target).await.err().expect("must fail");
    assert!(
        matches!(err, MeowError::RelayHopFailed { hop: 1, .. }),
        "REJECT sits at flattened index 1 ([a, REJECT, b]); got {err:?}"
    );
    assert!(a_host.lock().is_none(), "no hop may perform I/O");
}

/// Regression: a `dialer-proxy` member that lands at the *global* first
/// hop through nested-relay splicing must keep its wrapper — the
/// configured front dialer has to fire.  The old guard tested a
/// per-call `depth == 0 && out.is_empty()`, but each recursion level
/// had a fresh `out`, so a nested relay's first member was peeled even
/// when it was the chain's true hop 0 — silently bypassing the dialer.
#[tokio::test]
async fn nested_relay_first_member_keeps_dialer_proxy() {
    let p = MockProxy::new("p", "10.0.0.2", 1081, 2);
    let q = MockProxy::new("q", "10.0.0.3", 1082, 3);
    let r = MockProxy::new("r", "10.0.0.4", 1083, 4);
    let s = MockProxy::new("s", "10.0.0.5", 1084, 5);
    let p_visits = Arc::clone(&p.visits);
    let r_visits = Arc::clone(&r.visits);
    let s_visits = Arc::clone(&s.visits);

    let inner: Arc<dyn Proxy> = Arc::new(RelayGroup::new("inner", vec![p, q]));
    let dpa: Arc<dyn Proxy> = Arc::new(DialerProxyAdapter::new(
        inner,
        crate::dialer::DialerTarget::new(
            "never-resolved",
            &crate::dialer::ProxyRegistry::default(),
        ),
    ));
    let mid: Arc<dyn Proxy> = Arc::new(RelayGroup::new("mid", vec![dpa, r]));
    let outer = RelayGroup::new("outer", vec![mid, s]);
    let target = Metadata {
        host: "t.example".into(),
        dst_port: 443,
        ..Default::default()
    };

    let err = outer
        .dial_tcp(&target)
        .await
        .err()
        .expect("global hop 0 must run the DPA's dial_tcp and fail on the missing dialer");
    assert!(
        matches!(err, MeowError::RelayHopFailed { hop: 0, .. }),
        "failure must be attributed to hop 0; got {err:?}"
    );
    assert!(
        err.to_string().contains("never-resolved"),
        "error must name the missing dialer, got: {err}"
    );
    // Peeling would have spliced the inner chain and run `p` directly —
    // prove nothing ran.
    assert!(p_visits.lock().is_empty());
    assert!(r_visits.lock().is_empty());
    assert!(s_visits.lock().is_empty());
}

/// The same nested shape with a *resolvable* front dialer must run the
/// whole chain through it: `outer = [mid[DPA(inner[p, q]), r], s]`
/// dials `front → p → q → r → s → target`.
#[tokio::test]
async fn nested_relay_first_member_dialer_proxy_dials_through_front() {
    let front = MockProxy::new("front", "10.0.9.9", 1090, 9);
    let p = MockProxy::new("p", "10.0.0.2", 1081, 2);
    let q = MockProxy::new("q", "10.0.0.3", 1082, 3);
    let r = MockProxy::new("r", "10.0.0.4", 1083, 4);
    let s = MockProxy::new("s", "10.0.0.5", 1084, 5);
    let front_host = Arc::clone(&front.last_dial_host);
    let p_host = Arc::clone(&p.last_dial_host);
    let q_host = Arc::clone(&q.last_dial_host);
    let r_host = Arc::clone(&r.last_dial_host);
    let s_host = Arc::clone(&s.last_dial_host);
    let p_visits = Arc::clone(&p.visits);
    let q_visits = Arc::clone(&q.visits);

    let registry = crate::dialer::ProxyRegistry::default();
    let mut proxies: std::collections::HashMap<SmolStr, Arc<dyn Proxy>> =
        std::collections::HashMap::new();
    proxies.insert(front.name().into(), front);
    registry.publish(Arc::new(proxies));

    let inner: Arc<dyn Proxy> = Arc::new(RelayGroup::new("inner", vec![p, q]));
    let dpa: Arc<dyn Proxy> = Arc::new(DialerProxyAdapter::new(
        inner,
        crate::dialer::DialerTarget::new("front", &registry),
    ));
    let mid: Arc<dyn Proxy> = Arc::new(RelayGroup::new("mid", vec![dpa, r]));
    let outer = RelayGroup::new("outer", vec![mid, s]);
    let target = Metadata {
        host: "t.example".into(),
        dst_port: 443,
        ..Default::default()
    };

    outer.dial_tcp(&target).await.expect("nested DPA chain");

    // The front dialer reaches the inner chain's entry point; each hop
    // then gets the next hop's server; the last hop gets the target.
    assert_eq!(*front_host.lock(), Some("10.0.0.2".into()));
    assert_eq!(*p_host.lock(), Some("10.0.0.3".into()));
    assert_eq!(*q_host.lock(), Some("10.0.0.4".into()));
    assert_eq!(*r_host.lock(), Some("10.0.0.5".into()));
    assert_eq!(*s_host.lock(), Some("t.example".into()));
    assert_eq!(*p_visits.lock(), vec![2]);
    assert_eq!(*q_visits.lock(), vec![3]);
}

// ─── F. AdapterType and ProxyAdapter trait methods ────────────────────

// F1: adapter_type() == AdapterType::Relay
#[test]
fn adapter_type_is_relay() {
    let proxies: Vec<Arc<dyn Proxy>> = vec![
        MockProxy::new("A", "10.0.0.1", 1080, 1),
        MockProxy::new("B", "10.0.0.2", 1080, 2),
    ];
    let group = RelayGroup::new("f1", proxies);
    assert_eq!(group.adapter_type(), AdapterType::Relay);
}

// F2: AdapterType::Relay serialises to "Relay" in JSON
#[test]
fn adapter_type_serialises_to_relay() {
    let json = serde_json::to_string(&AdapterType::Relay).unwrap();
    assert_eq!(json, r#""Relay""#);
}

// F3: group.name() returns the config name
#[test]
fn group_name_returns_config_name() {
    let proxies: Vec<Arc<dyn Proxy>> = vec![
        MockProxy::new("A", "10.0.0.1", 1080, 1),
        MockProxy::new("B", "10.0.0.2", 1080, 2),
    ];
    let group = RelayGroup::new("my-relay", proxies);
    assert_eq!(group.name(), "my-relay");
}

// F4: group.addr() returns ""
#[test]
fn group_addr_returns_empty() {
    let proxies: Vec<Arc<dyn Proxy>> = vec![
        MockProxy::new("A", "10.0.0.1", 1080, 1),
        MockProxy::new("B", "10.0.0.2", 1080, 2),
    ];
    let group = RelayGroup::new("f4", proxies);
    assert_eq!(group.addr(), "");
}

// F5: group.health() is accessible (does not panic)
#[test]
fn group_health_accessible() {
    let proxies: Vec<Arc<dyn Proxy>> = vec![
        MockProxy::new("A", "10.0.0.1", 1080, 1),
        MockProxy::new("B", "10.0.0.2", 1080, 2),
    ];
    let group = RelayGroup::new("f5", proxies);
    let _ = group.health(); // must not panic
}

// ─── G. Structural invariants ─────────────────────────────────────────

// G2: debug_assert is present in relay.rs (grep guard-rail)
// "grep debug_assert crates/meow-proxy/src/group/relay.rs" → non-empty.
// The parse-time hard-error prevents production use; debug_assert catches
// test-harness mistakes. Verified: relay_tcp() and RelayGroup::new() both
// contain debug_assert!(proxies.len() >= 2).

// G3: connect_over is not defaulted on ProxyAdapter trait — any MockProxy
// without it would fail to compile. The above MockProxy impl demonstrates
// that connect_over must be provided explicitly. If omitted, the compiler
// surfaces a default impl (Err(NotSupported)) — which is intentionally
// permissive (trait has a default body). The guard-rail is enforced by code
// review: relay tests MUST use connect_over on every hop and will observe
// wrong visit counts if connect_over is not called.
