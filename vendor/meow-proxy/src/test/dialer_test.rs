use super::*;
use meow_common::{
    AdapterType, DelayHistory, ProxyAdapter, ProxyHealth, ProxyPacketConn, Result as MeowResult,
};
use std::sync::Mutex;

/// Front-proxy mock: captures the [`Metadata`] of every `dial_tcp` call
/// and refuses the connection. The captured metadata is what the front
/// proxy's rule engine, `/connections` list, and stats would see.
struct CapturingProxy {
    seen: Mutex<Vec<Metadata>>,
}

#[async_trait]
impl ProxyAdapter for CapturingProxy {
    fn name(&self) -> &str {
        "front"
    }
    fn adapter_type(&self) -> AdapterType {
        AdapterType::Socks5
    }
    fn addr(&self) -> &str {
        "127.0.0.1:1080"
    }
    fn support_udp(&self) -> bool {
        false
    }
    async fn dial_tcp(&self, metadata: &Metadata) -> MeowResult<Box<dyn ProxyConn>> {
        self.seen.lock().unwrap().push(metadata.clone());
        Err(meow_common::MeowError::NotSupported(
            "test mock refuses connections".to_string(),
        ))
    }
    async fn dial_udp(&self, _metadata: &Metadata) -> MeowResult<Box<dyn ProxyPacketConn>> {
        unimplemented!("test mock has no UDP")
    }
    fn health(&self) -> &ProxyHealth {
        static H: std::sync::OnceLock<ProxyHealth> = std::sync::OnceLock::new();
        H.get_or_init(ProxyHealth::new)
    }
}

impl Proxy for CapturingProxy {
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
        Vec::new()
    }
}

fn last_seen(mock: &CapturingProxy) -> Metadata {
    let seen = mock.seen.lock().unwrap();
    seen.last()
        .expect("a dial must reach the front proxy")
        .clone()
}

/// Sink conn returned by [`CapturingUdpProxy`]'s `dial_udp` — dial tests
/// never pump it, so reads pend forever and writes are black-holed.
struct SinkPacketConn;

#[async_trait]
impl ProxyPacketConn for SinkPacketConn {
    async fn read_packet(&self, _buf: &mut [u8]) -> MeowResult<(usize, SocketAddr)> {
        std::future::pending().await
    }
    async fn write_packet(&self, buf: &[u8], _addr: &SocketAddr) -> MeowResult<usize> {
        Ok(buf.len())
    }
    fn local_addr(&self) -> MeowResult<SocketAddr> {
        Ok("0.0.0.0:0".parse().unwrap())
    }
    fn close(&self) -> MeowResult<()> {
        Ok(())
    }
}

/// UDP-aware front mock: captures `dial_udp` metadata and honours the
/// `udp` flag — a false flag produces `UdpNotSupported` like a real
/// UDP-incapable front (HTTP, Reject).
struct CapturingUdpProxy {
    udp: bool,
    seen: Mutex<Vec<Metadata>>,
}

#[async_trait]
impl ProxyAdapter for CapturingUdpProxy {
    fn name(&self) -> &str {
        "udp-front"
    }
    fn adapter_type(&self) -> AdapterType {
        AdapterType::Socks5
    }
    fn addr(&self) -> &str {
        "127.0.0.1:1080"
    }
    fn support_udp(&self) -> bool {
        self.udp
    }
    async fn dial_tcp(&self, _metadata: &Metadata) -> MeowResult<Box<dyn ProxyConn>> {
        Err(meow_common::MeowError::NotSupported(
            "test mock refuses connections".to_string(),
        ))
    }
    async fn dial_udp(&self, metadata: &Metadata) -> MeowResult<Box<dyn ProxyPacketConn>> {
        self.seen.lock().unwrap().push(metadata.clone());
        if !self.udp {
            return Err(meow_common::MeowError::UdpNotSupported);
        }
        Ok(Box::new(SinkPacketConn))
    }
    fn health(&self) -> &ProxyHealth {
        static H: std::sync::OnceLock<ProxyHealth> = std::sync::OnceLock::new();
        H.get_or_init(ProxyHealth::new)
    }
}

impl Proxy for CapturingUdpProxy {
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
        Vec::new()
    }
}

#[tokio::test]
async fn proxy_dialer_udp_conn_is_inner_udp_to_remote() {
    // mihomo `proxyDialer.ListenPacket`: a chained UDP endpoint must
    // reach the front as `Inner` + `Udp` metadata addressed at the
    // *inner proxy's server endpoint* (here `remote`), not at the
    // user's target — the caller layers its own per-packet addressing
    // inside.
    let mock = Arc::new(CapturingUdpProxy {
        udp: true,
        seen: Mutex::new(Vec::new()),
    });
    let dialer = ProxyDialer::new(Arc::clone(&mock) as Arc<dyn Proxy>);
    let remote: SocketAddr = "203.0.113.7:8388".parse().unwrap();

    let conn = dialer
        .dial_udp_conn(UdpTarget::Addr(remote), false)
        .await
        .expect("UDP-capable front yields a conn");
    {
        let meta = mock.seen.lock().unwrap().last().unwrap().clone();
        assert_eq!(meta.network, Network::Udp);
        assert_eq!(meta.conn_type, ConnType::Inner);
        assert_eq!(meta.dst_ip, Some(remote.ip()));
        assert_eq!(meta.dst_port, remote.port());
        assert!(!meta.internal);
    }

    // The `internal` housekeeping marker rides the UDP path too.
    let _ = dialer.dial_udp_conn(UdpTarget::Addr(remote), true).await;
    assert!(mock.seen.lock().unwrap().last().unwrap().internal);

    // The bound-destination contract: writes target `remote`.
    assert_eq!(conn.write_packet(b"x", &remote).await.unwrap(), 1);
    assert!(dialer.supports_udp(), "capability delegates to the front");

    // A UDP-incapable front advertises false and fails the dial loudly.
    let incapable = ProxyDialer::new(Arc::new(CapturingUdpProxy {
        udp: false,
        seen: Mutex::new(Vec::new()),
    }) as Arc<dyn Proxy>);
    assert!(!incapable.supports_udp());
    match incapable
        .dial_udp_conn(UdpTarget::Addr(remote), false)
        .await
    {
        Err(e) => assert!(
            e.to_string().contains("dialer-proxy udp"),
            "front error must propagate with chain context, got: {e}"
        ),
        Ok(_) => panic!("UDP-incapable front must fail the chained dial"),
    }
}

#[tokio::test]
async fn named_dialer_udp_conn_resolves_front_per_dial() {
    let registry = ProxyRegistry::default();
    let remote: SocketAddr = "203.0.113.9:8388".parse().unwrap();

    let mock = Arc::new(CapturingUdpProxy {
        udp: true,
        seen: Mutex::new(Vec::new()),
    });
    let mut proxies: HashMap<SmolStr, Arc<dyn Proxy>> = HashMap::new();
    proxies.insert(SmolStr::from("front"), Arc::clone(&mock) as Arc<dyn Proxy>);
    registry.publish(Arc::new(proxies));

    let dialer = NamedProxyDialer::new(DialerTarget::new("front", &registry));
    assert!(dialer.supports_udp());
    dialer
        .dial_udp_conn(UdpTarget::Addr(remote), true)
        .await
        .expect("by-name UDP dial resolves");
    let meta = mock.seen.lock().unwrap().last().unwrap().clone();
    assert_eq!(meta.network, Network::Udp);
    assert_eq!(meta.dst_ip, Some(remote.ip()));
    assert!(meta.internal, "internal must survive NamedProxyDialer");

    // A rebuilt generation swapping in a UDP-incapable front flips the
    // advertisement and closes the dial — no stale capability survives.
    let mut proxies: HashMap<SmolStr, Arc<dyn Proxy>> = HashMap::new();
    proxies.insert(
        SmolStr::from("front"),
        Arc::new(CapturingUdpProxy {
            udp: false,
            seen: Mutex::new(Vec::new()),
        }) as Arc<dyn Proxy>,
    );
    registry.publish(Arc::new(proxies));
    assert!(!dialer.supports_udp());
    assert!(dialer
        .dial_udp_conn(UdpTarget::Addr(remote), false)
        .await
        .is_err());

    // A dropped registry fails closed (never a direct fallback).
    drop(registry);
    assert!(!dialer.supports_udp());
    match dialer.dial_udp_conn(UdpTarget::Addr(remote), false).await {
        Err(e) => assert!(
            e.to_string().contains("registry generation dropped"),
            "got: {e}"
        ),
        Ok(_) => panic!("dropped registry must fail closed"),
    }
}

/// `named()` collapses IP literals to `Addr` (nothing to delegate) and
/// keeps real domains as `Name`.
#[test]
fn udp_target_named_collapses_ip_literals() {
    assert_eq!(
        UdpTarget::named("127.0.0.1", 8388),
        UdpTarget::Addr("127.0.0.1:8388".parse().unwrap())
    );
    assert_eq!(
        UdpTarget::named("::1", 53),
        UdpTarget::Addr("[::1]:53".parse().unwrap())
    );
    // Bracketed IPv6 literals (the `host:port` display form) collapse
    // too — delegating them as a name would send a bogus domain to the
    // front's resolver (issue #665).
    assert_eq!(
        UdpTarget::named("[::1]", 53),
        UdpTarget::Addr("[::1]:53".parse().unwrap())
    );
    assert_eq!(
        UdpTarget::named("[fe80::1]", 5353),
        UdpTarget::Addr("[fe80::1]:5353".parse().unwrap())
    );
    // Mismatched/lone brackets stay names — unchanged behavior — and
    // a non-IP bracketed string keeps its verbatim text: stripping
    // would silently mint a *different, possibly resolvable* name.
    assert_eq!(
        UdpTarget::named("[x", 53),
        UdpTarget::Name {
            host: "[x".into(),
            port: 53
        }
    );
    assert_eq!(
        UdpTarget::named("[foo]", 53),
        UdpTarget::Name {
            host: "[foo]".into(),
            port: 53
        }
    );
    assert_eq!(
        UdpTarget::named("[]", 53),
        UdpTarget::Name {
            host: "[]".into(),
            port: 53
        }
    );
    assert_eq!(
        UdpTarget::named("ss.example.com", 8388),
        UdpTarget::Name {
            host: "ss.example.com".into(),
            port: 8388
        }
    );
}

/// Issue #701: a bracketed IPv6 `server:` folds into `dst_ip` so the
/// chained front hop encodes an IP address; a bare parse would leave
/// `"[::1]"` in `host`, where it dies as an unresolvable "domain".
#[test]
fn inner_dial_metadata_folds_bracketed_ipv6_literal() {
    let m = inner_dial_metadata("[::1]", 443, false);
    assert_eq!(m.dst_ip, Some("::1".parse().unwrap()));
    assert_eq!(m.dst_port, 443);
    assert!(m.host.is_empty());
    // Non-IP bracketed text stays a name — verbatim (same rule as
    // UdpTarget::named's [foo] case above).
    let m = inner_dial_metadata("[foo]", 443, false);
    assert_eq!(m.dst_ip, None);
    assert_eq!(&*m.host, "[foo]");
}

/// `src_matches`: literal targets compare canonically (IPv4-mapped
/// aliases collapse); name targets compare port only — the front's
/// resolution legitimately differs from any local view (issue #657).
#[test]
fn udp_target_src_matches() {
    let addr: SocketAddr = "203.0.113.7:8388".parse().unwrap();
    let t = UdpTarget::Addr(addr);
    assert!(t.src_matches(addr));
    // IPv4-mapped IPv6 alias of the same peer.
    assert!(t.src_matches("[::ffff:203.0.113.7]:8388".parse().unwrap()));
    assert!(!t.src_matches("203.0.113.7:9999".parse().unwrap()));
    assert!(!t.src_matches("203.0.113.8:8388".parse().unwrap()));

    let t = UdpTarget::named("ss.example.com", 8388);
    assert!(t.src_matches("198.51.100.1:8388".parse().unwrap()));
    assert!(t.src_matches("[2001:db8::1]:8388".parse().unwrap()));
    assert!(!t.src_matches("198.51.100.1:5300".parse().unwrap()));
}

/// `write_dst`/`port`: literals return the real endpoint; names return
/// an unspecified placeholder carrying the port (bound conns ignore it).
#[test]
fn udp_target_write_dst() {
    let addr: SocketAddr = "203.0.113.7:8388".parse().unwrap();
    assert_eq!(UdpTarget::Addr(addr).write_dst(), addr);
    assert_eq!(UdpTarget::Addr(addr).port(), 8388);
    let name = UdpTarget::named("ss.example.com", 8388);
    assert_eq!(name.write_dst(), "0.0.0.0:8388".parse().unwrap());
    assert_eq!(name.port(), 8388);
}

/// A `Name` target arrives at the front as host-only metadata — `host`
/// populated, `dst_ip` absent — the contract `Metadata::domain_udp_target`
/// detects downstream (issue #657).
#[tokio::test]
async fn proxy_dialer_udp_conn_name_sends_host_only_metadata() {
    let mock = Arc::new(CapturingUdpProxy {
        udp: true,
        seen: Mutex::new(Vec::new()),
    });
    let dialer = ProxyDialer::new(Arc::clone(&mock) as Arc<dyn Proxy>);

    dialer
        .dial_udp_conn(UdpTarget::named("ss.example.com", 8388), true)
        .await
        .expect("UDP-capable front yields a conn");

    let meta = mock.seen.lock().unwrap().last().unwrap().clone();
    assert_eq!(meta.network, Network::Udp);
    assert_eq!(meta.conn_type, ConnType::Inner);
    assert_eq!(meta.host, "ss.example.com");
    assert_eq!(meta.dst_ip, None, "Name targets carry no local resolution");
    assert_eq!(meta.dst_port, 8388);
    assert!(meta.internal);
    let (host, port) = meta
        .domain_udp_target()
        .expect("the front adapter detects the domain request via this signal");
    assert_eq!(host.as_str(), "ss.example.com");
    assert_eq!(port, 8388);
}

/// `NamedProxyDialer` forwards a `Name` target unchanged — the front
/// resolves it, not an intermediate hop.
#[tokio::test]
async fn named_dialer_udp_conn_forwards_name_target() {
    let registry = ProxyRegistry::default();
    let mock = Arc::new(CapturingUdpProxy {
        udp: true,
        seen: Mutex::new(Vec::new()),
    });
    let mut proxies: HashMap<SmolStr, Arc<dyn Proxy>> = HashMap::new();
    proxies.insert(SmolStr::from("front"), Arc::clone(&mock) as Arc<dyn Proxy>);
    registry.publish(Arc::new(proxies));

    let dialer = NamedProxyDialer::new(DialerTarget::new("front", &registry));
    dialer
        .dial_udp_conn(UdpTarget::named("relay.example.com", 5300), false)
        .await
        .expect("by-name UDP dial resolves");
    let meta = mock.seen.lock().unwrap().last().unwrap().clone();
    assert_eq!(meta.host, "relay.example.com");
    assert_eq!(meta.dst_ip, None);
    assert_eq!(meta.dst_port, 5300);
}

/// `DirectDialer` on a `Name` target resolves locally — there is no
/// further resolver view to delegate to — and connects the socket.
/// Loopback echo proves the bound conn actually lands datagrams.
#[tokio::test]
async fn direct_dialer_udp_conn_name_resolves_and_exchanges() {
    // `localhost` resolves to ::1 + 127.0.0.1 and the dialer picks the
    // resolver's first candidate — echo on both families so whichever
    // wins is served.
    let echo4 = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let echo6 = tokio::net::UdpSocket::bind("[::1]:0").await.unwrap();
    let port = echo4.local_addr().unwrap().port();
    let echo6 = {
        // Rebind v6 onto the same port — the dial target is one port.
        drop(echo6);
        tokio::net::UdpSocket::bind(("::1", port)).await.unwrap()
    };
    for sock in [echo4, echo6] {
        tokio::spawn(async move {
            let mut buf = [0u8; 64];
            while let Ok((n, src)) = sock.recv_from(&mut buf).await {
                let _ = sock.send_to(&buf[..n], src).await;
            }
        });
    }

    let conn = DirectDialer
        .dial_udp_conn(UdpTarget::named("localhost", port), false)
        .await
        .expect("localhost resolves");
    conn.write_packet(b"ping", &conn.local_addr().unwrap())
        .await
        .unwrap();
    let mut buf = [0u8; 64];
    let (n, src) = conn.read_packet(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"ping");
    assert!(src.ip().is_loopback(), "loopback echo source, got {src}");
}

/// A proxy whose `support_udp` consults a `dialer-proxy` dialer that
/// resolves back to itself — the `node → group → node` dynamic-cycle
/// shape, reproduced without a group. Without the
/// [`SUPPORTS_UDP_DEPTH`] guard this recurses until the stack blows.
struct LoopyProxy {
    dialer: NamedProxyDialer,
}

#[async_trait]
impl ProxyAdapter for LoopyProxy {
    fn name(&self) -> &str {
        "loopy"
    }
    fn adapter_type(&self) -> AdapterType {
        AdapterType::Socks5
    }
    fn addr(&self) -> &str {
        ""
    }
    fn support_udp(&self) -> bool {
        self.dialer.supports_udp()
    }
    async fn dial_tcp(&self, _metadata: &Metadata) -> MeowResult<Box<dyn ProxyConn>> {
        Err(meow_common::MeowError::NotSupported("loopy".into()))
    }
    async fn dial_udp(&self, _metadata: &Metadata) -> MeowResult<Box<dyn ProxyPacketConn>> {
        Err(meow_common::MeowError::UdpNotSupported)
    }
    fn health(&self) -> &ProxyHealth {
        static H: std::sync::OnceLock<ProxyHealth> = std::sync::OnceLock::new();
        H.get_or_init(ProxyHealth::new)
    }
}

impl Proxy for LoopyProxy {
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
        Vec::new()
    }
}

#[test]
fn supports_udp_bounds_dynamic_cycles() {
    let registry = ProxyRegistry::default();
    let mut proxies: HashMap<SmolStr, Arc<dyn Proxy>> = HashMap::new();
    proxies.insert(
        SmolStr::from("loopy"),
        Arc::new(LoopyProxy {
            dialer: NamedProxyDialer::new(DialerTarget::new("loopy", &registry)),
        }) as Arc<dyn Proxy>,
    );
    registry.publish(Arc::new(proxies));

    let dialer = NamedProxyDialer::new(DialerTarget::new("loopy", &registry));
    assert!(
        !dialer.supports_udp(),
        "a cyclic capability query must bottom out at the depth guard"
    );
}

#[tokio::test]
async fn proxy_dialer_dials_are_inner_tcp_connections() {
    // Review B2: `Metadata::default()` carries `ConnType::Http` (the
    // first enum variant), but a dialer-proxy chained relay is an
    // internal connection. The front proxy must see `ConnType::Inner`
    // + `Network::Tcp` so its rules route it as infrastructure traffic
    // and `/connections`/stats don't mislabel it as an HTTP inbound.
    let mock = Arc::new(CapturingProxy {
        seen: Mutex::new(Vec::new()),
    });
    let dialer = ProxyDialer::new(Arc::clone(&mock) as Arc<dyn Proxy>);

    // Hostname target — dial() must produce host + port metadata.
    let _ = dialer.dial("chain.example", 443, false).await;
    let meta = last_seen(&mock);
    assert_eq!(meta.conn_type, ConnType::Inner);
    assert_eq!(meta.network, Network::Tcp);
    assert_eq!(meta.host.as_str(), "chain.example");
    assert_eq!(meta.dst_port, 443);
    assert!(!meta.internal);

    // Internal traffic keeps the `Inner` conn_type but carries the
    // marker through the reconstruction, so a lazy front-hop group
    // would not count this dial as user traffic.
    let _ = dialer.dial("chain.example", 443, true).await;
    let meta = last_seen(&mock);
    assert_eq!(meta.conn_type, ConnType::Inner);
    assert!(meta.internal);

    // IP-literal target through dial() — typed dst_ip, no host string.
    let _ = dialer.dial("192.0.2.9", 853, false).await;
    let meta = last_seen(&mock);
    assert_eq!(meta.conn_type, ConnType::Inner);
    assert_eq!(meta.network, Network::Tcp);
    assert_eq!(meta.dst_ip, Some("192.0.2.9".parse().unwrap()));
    assert_eq!(meta.dst_port, 853);
    assert!(!meta.internal);

    // The internal arm of the same IP-literal path — the marker rides
    // the `Ok(ip)` rebuild branch too.
    let _ = dialer.dial("192.0.2.9", 853, true).await;
    let meta = last_seen(&mock);
    assert_eq!(meta.conn_type, ConnType::Inner);
    assert_eq!(meta.dst_ip, Some("192.0.2.9".parse().unwrap()));
    assert!(meta.internal);

    // SocketAddr target through dial_addr() — typed dst_ip + marker.
    let addr: SocketAddr = "[2001:db8::1]:443".parse().unwrap();
    let _ = dialer.dial_addr(addr, true).await;
    let meta = last_seen(&mock);
    assert_eq!(meta.conn_type, ConnType::Inner);
    assert_eq!(meta.network, Network::Tcp);
    assert_eq!(meta.dst_ip, Some(addr.ip()));
    assert!(meta.internal);
    assert_eq!(meta.dst_port, 443);

    assert_eq!(mock.seen.lock().unwrap().len(), 5);
}

#[tokio::test]
async fn named_proxy_dialer_forwards_internal_marker() {
    // `NamedProxyDialer` is the dialer every `dialer-proxy` adapter
    // actually receives — the production path for #555. It must
    // forward `internal` into `ProxyDialer`'s reconstruction, or the
    // lazy front-hop group counts housekeeping dials as use while
    // every `ProxyDialer` test still passes.
    let registry = ProxyRegistry::default();
    let mock = Arc::new(CapturingProxy {
        seen: Mutex::new(Vec::new()),
    });
    let mut proxies: HashMap<SmolStr, Arc<dyn Proxy>> = HashMap::new();
    proxies.insert(SmolStr::from("front"), Arc::clone(&mock) as Arc<dyn Proxy>);
    registry.publish(Arc::new(proxies));

    let dialer = NamedProxyDialer::new(DialerTarget::new("front", &registry));
    let _ = dialer.dial("chain.example", 443, true).await;
    let meta = last_seen(&mock);
    assert_eq!(meta.conn_type, ConnType::Inner);
    assert!(meta.internal, "internal must survive NamedProxyDialer");

    let _ = dialer.dial("chain.example", 443, false).await;
    assert!(!last_seen(&mock).internal, "user dials stay unmarked");

    let addr: SocketAddr = "192.0.2.10:8443".parse().unwrap();
    let _ = dialer.dial_addr(addr, true).await;
    assert!(last_seen(&mock).internal, "dial_addr forwards it too");
}

#[tokio::test]
async fn internal_marker_survives_reconstruction_for_lazy_groups() {
    // #555: housekeeping dials chained through `dialer-proxy` reach the
    // front hop as `ConnType::Inner` — the `internal` flag must carry
    // the usage-accounting marker across the reconstruction, or a lazy
    // front-hop group counts every probe/fetch as use and its probe
    // loop never goes idle.
    use crate::group::fallback::FallbackGroup;
    use crate::group::test_support::MockProxy;

    let member = MockProxy::new("member");
    let group = Arc::new(FallbackGroup::new("lazy-fb", vec![member]));
    let dialer = ProxyDialer::new(Arc::clone(&group) as Arc<dyn Proxy>);

    // Housekeeping dial (probe/fetch) — the marker reaches the group.
    let _ = dialer.dial("chain.example", 443, true).await;
    assert_eq!(
        group.usage_generation(),
        0,
        "internal chained dial must not record group use"
    );

    // Real user traffic through the same chain still counts.
    let _ = dialer.dial("chain.example", 443, false).await;
    assert_eq!(
        group.usage_generation(),
        1,
        "user chained dial records group use"
    );
}

/// `DialerTarget` holds the registry cell weakly (issue #533): it resolves
/// while a `ProxyRegistry` clone keeps the generation alive and fails
/// closed once the last owner drops — the cycle the strong edge used to
/// pin forever now dies with its generation.
#[test]
fn target_resolves_while_owned_and_fails_closed_after_drop() {
    let registry = ProxyRegistry::default();
    let target = DialerTarget::new("front", &registry);
    // Unpublished cell: alive but empty.
    assert!(target.resolve().is_none());

    let mut proxies: HashMap<SmolStr, Arc<dyn Proxy>> = HashMap::new();
    proxies.insert(
        SmolStr::from("front"),
        Arc::new(CapturingProxy {
            seen: Mutex::new(Vec::new()),
        }),
    );
    registry.publish(Arc::new(proxies));
    assert!(target.resolve().is_some());

    // A clone keeps the cell alive; dropping one handle is not enough.
    let retained = registry.clone();
    drop(registry);
    assert!(target.resolve().is_some());
    drop(retained);
    assert!(target.resolve().is_none());
}

/// `NamedProxyDialer` is the chained path most adapters take — its
/// fail-closed arm must surface the dead-registry error, never fall
/// through to direct egress (issue #533 review).
#[tokio::test]
async fn named_proxy_dialer_fails_closed_when_registry_drops() {
    let registry = ProxyRegistry::default();
    let dialer = NamedProxyDialer::new(DialerTarget::new("ghost", &registry));
    drop(registry);

    let err = dialer
        .dial("example.com", 443, false)
        .await
        .err()
        .expect("a dead registry must fail the dial");
    assert!(
        err.to_string().contains("registry generation dropped"),
        "expected the dead-cell error, got: {err}"
    );
}

/// A `dialer-proxy` cycle that static analysis cannot see — here a
/// provider-member-style loop where `loop`'s own dial re-enters the
/// by-name dialer — must degrade to a dial error at
/// [`MAX_DIALER_CHAIN_DEPTH`] instead of recursing until the native
/// stack overflows (issue #489).
#[tokio::test]
async fn named_dialer_recursion_is_depth_bounded() {
    struct LoopProxy {
        dialer: NamedProxyDialer,
    }

    #[async_trait]
    impl ProxyAdapter for LoopProxy {
        fn name(&self) -> &str {
            "loop"
        }
        fn adapter_type(&self) -> AdapterType {
            AdapterType::Socks5
        }
        fn addr(&self) -> &str {
            "127.0.0.1:1"
        }
        fn support_udp(&self) -> bool {
            false
        }
        async fn dial_tcp(&self, _metadata: &Metadata) -> MeowResult<Box<dyn ProxyConn>> {
            // Re-enter the by-name chain — the loop only terminates via
            // the depth guard.
            self.dialer
                .dial("127.0.0.1", 1, false)
                .await
                .map_err(|e| meow_common::MeowError::Proxy(e.to_string()))?;
            Err(meow_common::MeowError::NotSupported(
                "unreachable past the depth bound".to_string(),
            ))
        }
        async fn dial_udp(&self, _metadata: &Metadata) -> MeowResult<Box<dyn ProxyPacketConn>> {
            unimplemented!("test mock has no UDP")
        }
        fn health(&self) -> &ProxyHealth {
            static H: std::sync::OnceLock<ProxyHealth> = std::sync::OnceLock::new();
            H.get_or_init(ProxyHealth::new)
        }
    }

    impl Proxy for LoopProxy {
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
            Vec::new()
        }
    }

    let registry = ProxyRegistry::default();
    let target = DialerTarget::new("loop", &registry);
    let looper: Arc<dyn Proxy> = Arc::new(LoopProxy {
        dialer: NamedProxyDialer::new(target),
    });
    registry.publish(Arc::new(HashMap::from([(SmolStr::from("loop"), looper)])));

    // Dial through the chain; recursion must stop at the bound with a
    // named error, not a stack overflow.
    let dialer = NamedProxyDialer::new(DialerTarget::new("loop", &registry));
    let err = dialer
        .dial("203.0.113.1", 443, false)
        .await
        .err()
        .expect("the cycle must surface as an error");
    assert!(
        err.to_string().contains("exceeds"),
        "depth bound must be the surfaced error: {err}"
    );
}

/// The bound is pinned to exactly [`MAX_DIALER_CHAIN_DEPTH`] hops: a
/// self-looping front must be entered that many times before the guard
/// fires — a bound of 1 or 2 would still produce an "exceeds" error
/// while breaking legitimate multi-hop chains.
#[tokio::test]
async fn named_dialer_recursion_hops_are_counted() {
    struct CountingLoop {
        dialer: NamedProxyDialer,
        dials: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl ProxyAdapter for CountingLoop {
        fn name(&self) -> &str {
            "loop"
        }
        fn adapter_type(&self) -> AdapterType {
            AdapterType::Socks5
        }
        fn addr(&self) -> &str {
            "127.0.0.1:1"
        }
        fn support_udp(&self) -> bool {
            false
        }
        async fn dial_tcp(&self, _metadata: &Metadata) -> MeowResult<Box<dyn ProxyConn>> {
            self.dials
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.dialer
                .dial("127.0.0.1", 1, false)
                .await
                .map_err(|e| meow_common::MeowError::Proxy(e.to_string()))?;
            Err(meow_common::MeowError::NotSupported(
                "unreachable past the depth bound".to_string(),
            ))
        }
        async fn dial_udp(&self, _metadata: &Metadata) -> MeowResult<Box<dyn ProxyPacketConn>> {
            unimplemented!("test mock has no UDP")
        }
        fn health(&self) -> &ProxyHealth {
            static H: std::sync::OnceLock<ProxyHealth> = std::sync::OnceLock::new();
            H.get_or_init(ProxyHealth::new)
        }
    }

    impl Proxy for CountingLoop {
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
            Vec::new()
        }
    }

    let registry = ProxyRegistry::default();
    let dials = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let looper: Arc<dyn Proxy> = Arc::new(CountingLoop {
        dialer: NamedProxyDialer::new(DialerTarget::new("loop", &registry)),
        dials: Arc::clone(&dials),
    });
    registry.publish(Arc::new(HashMap::from([(SmolStr::from("loop"), looper)])));

    let dialer = NamedProxyDialer::new(DialerTarget::new("loop", &registry));
    dialer
        .dial("127.0.0.1", 1, false)
        .await
        .err()
        .expect("the cycle must surface as an error");
    assert_eq!(
        dials.load(std::sync::atomic::Ordering::Relaxed),
        MAX_DIALER_CHAIN_DEPTH,
        "the guard must admit exactly MAX_DIALER_CHAIN_DEPTH hops"
    );
}

/// The flip side of the bound: a *legal* multi-hop chain must dial
/// end to end. A → B → C exercises the depth counter on the success
/// path — an off-by-one that rejects depth ≥ 2 would fail here.
#[tokio::test]
async fn named_dialer_legal_multi_hop_chain_dials() {
    /// A front that forwards to another named hop, or records the dial
    /// when built without one.
    struct Hop {
        name: &'static str,
        next: Option<NamedProxyDialer>,
        seen: Mutex<Vec<Metadata>>,
    }

    #[async_trait]
    impl ProxyAdapter for Hop {
        fn name(&self) -> &str {
            self.name
        }
        fn adapter_type(&self) -> AdapterType {
            AdapterType::Socks5
        }
        fn addr(&self) -> &str {
            "127.0.0.1:1"
        }
        fn support_udp(&self) -> bool {
            false
        }
        async fn dial_tcp(&self, metadata: &Metadata) -> MeowResult<Box<dyn ProxyConn>> {
            if let Some(dialer) = &self.next {
                // `dial` yields a transport `Stream`, not a `ProxyConn` —
                // the terminal always refuses, so this only ever
                // propagates the downstream error.
                dialer
                    .dial("127.0.0.1", 1, false)
                    .await
                    .map_err(|e| meow_common::MeowError::Proxy(e.to_string()))?;
                return Err(meow_common::MeowError::NotSupported(
                    "unreachable: the terminal always refuses".to_string(),
                ));
            }
            self.seen.lock().unwrap().push(metadata.clone());
            Err(meow_common::MeowError::NotSupported(
                "terminal hop refuses".to_string(),
            ))
        }
        async fn dial_udp(&self, _metadata: &Metadata) -> MeowResult<Box<dyn ProxyPacketConn>> {
            unimplemented!("test mock has no UDP")
        }
        fn health(&self) -> &ProxyHealth {
            static H: std::sync::OnceLock<ProxyHealth> = std::sync::OnceLock::new();
            H.get_or_init(ProxyHealth::new)
        }
    }

    impl Proxy for Hop {
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
            Vec::new()
        }
    }

    let registry = ProxyRegistry::default();
    let terminal = Arc::new(Hop {
        name: "c",
        next: None,
        seen: Mutex::new(Vec::new()),
    });
    let hop_b = Arc::new(Hop {
        name: "b",
        next: Some(NamedProxyDialer::new(DialerTarget::new("c", &registry))),
        seen: Mutex::new(Vec::new()),
    });
    let hop_a = Arc::new(Hop {
        name: "a",
        next: Some(NamedProxyDialer::new(DialerTarget::new("b", &registry))),
        seen: Mutex::new(Vec::new()),
    });
    registry.publish(Arc::new(HashMap::from([
        (SmolStr::from("a"), hop_a as Arc<dyn Proxy>),
        (SmolStr::from("b"), hop_b as Arc<dyn Proxy>),
        (SmolStr::from("c"), Arc::clone(&terminal) as Arc<dyn Proxy>),
    ])));

    let dialer = NamedProxyDialer::new(DialerTarget::new("a", &registry));
    dialer
        .dial("127.0.0.1", 1, false)
        .await
        .err()
        .expect("the terminal hop refuses");
    assert_eq!(
        terminal.seen.lock().unwrap().len(),
        1,
        "a 3-hop chain must reach the terminal front"
    );
}

/// `NamedProxyDialer` must produce the same `Inner`/`Tcp` metadata the
/// `ProxyDialer` path produces — a regression would misclassify chained
/// dials in the front proxy's rules, /connections, and stats.
#[tokio::test]
async fn named_dialer_metadata_matches_proxy_dialer() {
    let registry = ProxyRegistry::default();
    let front = Arc::new(CapturingProxy {
        seen: Mutex::new(Vec::new()),
    });
    registry.publish(Arc::new(HashMap::from([(
        SmolStr::from("front"),
        Arc::clone(&front) as Arc<dyn Proxy>,
    )])));

    let dialer = NamedProxyDialer::new(DialerTarget::new("front", &registry));
    // Hostname → `host` field; IP literal → typed `dst_ip`.
    let _ = dialer.dial("example.com", 443, false).await;
    let _ = dialer.dial("203.0.113.7", 8443, false).await;
    let _ = dialer
        .dial_addr("[2001:db8::1]:1080".parse().unwrap(), false)
        .await;

    let seen = front.seen.lock().unwrap();
    let [host_meta, ip_meta, v6_meta] = seen.as_slice() else {
        panic!("three dials must reach the front: {seen:?}")
    };
    assert_eq!(host_meta.host, "example.com");
    assert_eq!(host_meta.dst_ip, None);
    assert_eq!(host_meta.dst_port, 443);
    assert_eq!(ip_meta.dst_ip, Some("203.0.113.7".parse().unwrap()));
    assert!(ip_meta.host.is_empty());
    assert_eq!(v6_meta.dst_ip, Some("2001:db8::1".parse().unwrap()));
    for m in seen.iter() {
        assert_eq!(m.conn_type, ConnType::Inner);
        assert_eq!(m.network, Network::Tcp);
    }
}

/// The relay-wrapper entry point needs the same bound: a
/// `DialerProxyAdapter` registered under the very name it resolves
/// recurses through `relay_tcp` on every hop (issue #489).
#[tokio::test]
async fn dialer_proxy_adapter_recursion_is_depth_bounded() {
    let registry = ProxyRegistry::default();
    let target = DialerTarget::new("x", &registry);
    let inner: Arc<dyn Proxy> = Arc::new(CapturingProxy {
        seen: Mutex::new(Vec::new()),
    });
    let dpa: Arc<dyn Proxy> = Arc::new(crate::DialerProxyAdapter::new(inner, target));
    registry.publish(Arc::new(HashMap::from([(
        SmolStr::from("x"),
        Arc::clone(&dpa),
    )])));

    let err = dpa
        .dial_tcp(&Metadata::default())
        .await
        .err()
        .expect("the self-referencing wrapper must fail");
    assert!(
        err.to_string().contains("exceeds"),
        "depth bound must be the surfaced error: {err}"
    );
}

/// `PacketConnSocket` bridges an async `ProxyPacketConn` onto the
/// poll-based `SocketIo` surface kcptun consumes: poll_send queues into
/// the write pump (which `write_packet`s the bound remote), poll_recv
/// surfaces pumped inbound datagrams, and a dead read pump turns recv
/// into BrokenPipe.
#[cfg(feature = "kcptun")]
#[tokio::test]
async fn packet_conn_socket_bridges_poll_and_async() {
    use futures::future::poll_fn;
    use meow_transport::kcptun::SocketIo;
    use tokio::io::ReadBuf;

    /// A datagram conn fed by a channel: `read_packet` parks on the
    /// receiver until the test pushes a datagram; dropping `tx` makes
    /// the pump error out, mimicking a dead association.
    struct MockConn {
        remote: SocketAddr,
        // tokio Mutex: the std MutexGuard is !Send and cannot be held
        // across `recv().await` inside the spawned read pump.
        read_rx: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>>,
        written: Mutex<Vec<Vec<u8>>>,
        closed: std::sync::atomic::AtomicBool,
    }

    #[async_trait]
    impl ProxyPacketConn for MockConn {
        async fn read_packet(&self, buf: &mut [u8]) -> MeowResult<(usize, SocketAddr)> {
            match self.read_rx.lock().await.recv().await {
                Some(pkt) => {
                    let n = pkt.len().min(buf.len());
                    buf[..n].copy_from_slice(&pkt[..n]);
                    Ok((n, self.remote))
                }
                None => Err(MeowError::Proxy("mock conn closed".into())),
            }
        }
        async fn write_packet(&self, data: &[u8], addr: &SocketAddr) -> MeowResult<usize> {
            assert_eq!(*addr, self.remote);
            self.written.lock().unwrap().push(data.to_vec());
            Ok(data.len())
        }
        fn local_addr(&self) -> MeowResult<SocketAddr> {
            Ok(self.remote)
        }
        fn close(&self) -> MeowResult<()> {
            self.closed
                .store(true, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }
    }

    let remote: SocketAddr = "10.0.0.1:7777".parse().unwrap();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let conn = Arc::new(MockConn {
        remote,
        read_rx: tokio::sync::Mutex::new(rx),
        written: Mutex::new(Vec::new()),
        closed: std::sync::atomic::AtomicBool::new(false),
    });
    let conn_cloned = Arc::clone(&conn);
    let conn_dyn: Arc<dyn ProxyPacketConn> = conn_cloned;
    let mut sock = super::packet_conn_socket::PacketConnSocket::new(conn_dyn, remote);

    // Outbound: poll_send → write pump → conn.write_packet(remote).
    poll_fn(|cx| sock.poll_send(cx, b"ping"))
        .await
        .expect("poll_send");
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(conn.written.lock().unwrap().as_slice(), &[b"ping"]);

    // Inbound: a pushed datagram surfaces through poll_recv.
    tx.send(b"pong".to_vec()).unwrap();
    let mut raw = [0u8; 64];
    let mut rb = ReadBuf::new(&mut raw);
    poll_fn(|cx| sock.poll_recv(cx, &mut rb))
        .await
        .expect("poll_recv");
    assert_eq!(rb.filled(), b"pong");

    // Oversized inbound datagram truncates like a datagram socket.
    tx.send(vec![7u8; 60000]).unwrap();
    let mut small = [0u8; 16];
    let mut rb2 = ReadBuf::new(&mut small);
    poll_fn(|cx| sock.poll_recv(cx, &mut rb2))
        .await
        .expect("truncated recv");
    assert_eq!(rb2.filled().len(), 16);

    // Dead read pump (channel closed) → recv errors BrokenPipe.
    drop(tx);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let mut sink = [0u8; 64];
    let mut rb3 = ReadBuf::new(&mut sink);
    let r = poll_fn(|cx| sock.poll_recv(cx, &mut rb3)).await;
    assert_eq!(r.unwrap_err().kind(), std::io::ErrorKind::BrokenPipe);
}
