use super::*;
use meow_common::DnsMode;
use meow_dns::HostEntry;
use meow_trie::DomainTrie;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

fn fake_dest() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)), 1)
}

fn udp_metadata(dst_ip: IpAddr) -> Metadata {
    Metadata {
        dst_ip: Some(dst_ip),
        dst_port: 443,
        ..Default::default()
    }
}

fn tcp_metadata(host: &str, port: u16) -> Metadata {
    Metadata {
        host: host.into(),
        dst_port: port,
        ..Default::default()
    }
}

#[tokio::test]
async fn dial_tcp_tries_next_resolved_address_after_first_fails() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let mut hosts: DomainTrie<HostEntry> = DomainTrie::new();
    hosts.insert(
        "multi.test",
        vec![
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
        ]
        .into(),
    );
    let resolver = Arc::new(Resolver::new(
        vec![],
        vec![],
        DnsMode::Normal,
        hosts,
        true,
        true,
    ));
    let adapter = DirectAdapter::new()
        .with_resolver(resolver)
        .with_connect_timeout(Duration::from_secs(2));

    let accept = tokio::spawn(async move { listener.accept().await.unwrap() });
    let conn = adapter
        .dial_tcp(&tcp_metadata("multi.test", port))
        .await
        .expect("second resolved address should connect");
    let _ = accept.await.unwrap();
    drop(conn);
}

/// `COMPATIBLE` is a real direct dialer under the `Compatible` tag —
/// not a display alias. Prove it with a real loopback exchange.
#[tokio::test]
async fn compatible_dials_direct_tcp_and_udp() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let echo = tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 16];
        let n = s.read(&mut buf).await.unwrap();
        s.write_all(&buf[..n]).await.unwrap();
    });

    let adapter = DirectAdapter::compatible();
    assert_eq!(adapter.adapter_type(), AdapterType::Compatible);
    let mut conn = adapter
        .dial_tcp(&tcp_metadata("127.0.0.1", port))
        .await
        .expect("COMPATIBLE must dial direct");
    conn.write_all(b"ping").await.unwrap();
    let mut buf = [0u8; 4];
    conn.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"ping");
    echo.await.unwrap();

    let udp = adapter
        .dial_udp(&udp_metadata(IpAddr::V4(Ipv4Addr::LOCALHOST)))
        .await
        .expect("COMPATIBLE must bind a UDP session");
    assert!(udp.local_addr().unwrap().is_ipv4());
}

/// Regression for QUIC/HTTP3 direct: `dial_udp` must bind the reply socket
/// in the destination's address family. A v6 destination on an AF_INET
/// socket cannot be written, so the NAT session — and thus the reply
/// reader — never forms. We only assert the *bound family* here so the
/// test is independent of host IPv6 routing.
#[tokio::test]
async fn dial_udp_binds_v6_socket_for_v6_destination() {
    let adapter = DirectAdapter::new();
    let conn = adapter
        .dial_udp(&udp_metadata(IpAddr::V6(Ipv6Addr::LOCALHOST)))
        .await
        .expect("dial_udp must succeed for a v6 destination");
    let local = conn.local_addr().expect("local_addr");
    assert!(
        local.is_ipv6(),
        "v6 destination must bind a v6 socket, got {local}"
    );
}

#[tokio::test]
async fn dial_udp_binds_v4_socket_for_v4_destination() {
    let adapter = DirectAdapter::new();
    let conn = adapter
        .dial_udp(&udp_metadata(IpAddr::V4(Ipv4Addr::LOCALHOST)))
        .await
        .expect("dial_udp must succeed for a v4 destination");
    let local = conn.local_addr().expect("local_addr");
    assert!(
        local.is_ipv4(),
        "v4 destination must bind a v4 socket, got {local}"
    );
}

/// Full bidirectional round-trip over IPv6: prove server→app replies flow
/// back through the direct UDP conn. Skipped when the host has no IPv6
/// loopback (some minimal CI sandboxes).
#[tokio::test]
async fn dial_udp_v6_round_trip_delivers_reply() {
    let Ok(echo) = tokio::net::UdpSocket::bind("[::1]:0").await else {
        eprintln!("no IPv6 loopback available; skipping v6 round-trip test");
        return;
    };
    let echo_addr = echo.local_addr().unwrap();

    let adapter = DirectAdapter::new();
    let conn = adapter
        .dial_udp(&udp_metadata(echo_addr.ip()))
        .await
        .expect("dial_udp v6");

    // app → server
    conn.write_packet(b"ping", &echo_addr)
        .await
        .expect("write_packet to v6 destination must succeed");

    // server receives and replies
    let mut sbuf = [0u8; 16];
    let (n, from) = tokio::time::timeout(Duration::from_secs(2), echo.recv_from(&mut sbuf))
        .await
        .expect("echo recv timed out")
        .expect("echo recv");
    assert_eq!(&sbuf[..n], b"ping");
    echo.send_to(b"pong", from).await.expect("echo reply");

    // server → app: the reply must flow back through the same conn
    let mut cbuf = [0u8; 16];
    let (n, _) = tokio::time::timeout(Duration::from_secs(2), conn.read_packet(&mut cbuf))
        .await
        .expect("reply read timed out")
        .expect("read_packet");
    assert_eq!(&cbuf[..n], b"pong");
}

/// Drive `apply_connect_timeout` against a future that never completes,
/// using `tokio::time::pause()` so the timeout fires in virtual time.
/// Deterministic — no real network, no wall-clock dependence on test-net
/// blackholing.
#[tokio::test(start_paused = true)]
async fn apply_connect_timeout_fires_on_pending_future() {
    let pending = std::future::pending::<std::io::Result<TcpStream>>();
    let task = tokio::spawn(apply_connect_timeout(
        pending,
        Some(Duration::from_millis(500)),
        fake_dest(),
    ));
    // Advance past the budget; the timeout must now have fired.
    tokio::time::advance(Duration::from_millis(501)).await;
    let res = task.await.expect("join");
    let err = res.expect_err("must surface TimedOut");
    match err {
        MeowError::Io(io) => {
            assert_eq!(io.kind(), std::io::ErrorKind::TimedOut);
            let msg = io.to_string();
            assert!(
                msg.contains("192.0.2.1") && msg.contains("500"),
                "error message should name the destination and budget: {msg}"
            );
        }
        other => panic!("expected MeowError::Io(TimedOut), got {other:?}"),
    }
}

/// With `timeout = None`, the helper awaits the inner future to
/// completion. Verify it does not preempt a fast-succeeding connect.
#[tokio::test]
async fn apply_connect_timeout_none_passes_through_success() {
    // Build a satisfied future by dialling a real local listener.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accept = tokio::spawn(async move {
        let (s, _) = listener.accept().await.unwrap();
        // Hold open so the client's connect completes; drop after.
        drop(s);
    });
    let connect = TcpStream::connect(addr);
    let res = apply_connect_timeout(connect, None, addr).await;
    assert!(res.is_ok(), "no timeout configured → must pass through");
    let _ = accept.await;
}

/// With `timeout = Some(..)` but the inner future is ready immediately,
/// we must NOT spuriously surface TimedOut.
#[tokio::test]
async fn apply_connect_timeout_does_not_fire_when_connect_is_fast() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accept = tokio::spawn(async move {
        let _ = listener.accept().await;
    });
    let connect = TcpStream::connect(addr);
    let res = apply_connect_timeout(connect, Some(Duration::from_secs(5)), addr).await;
    assert!(
        res.is_ok(),
        "successful local connect must not race the timeout"
    );
    let _ = accept.await;
}

/// When the inner connect itself errors (e.g. `ECONNREFUSED` from a
/// closed port), the helper surfaces the real IO error rather than
/// disguising it as a timeout.
#[tokio::test]
async fn apply_connect_timeout_propagates_io_error() {
    // Bind a listener to claim a port, then drop it so connects RST.
    let port = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    };
    let dest: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let connect = TcpStream::connect(dest);
    let res = apply_connect_timeout(connect, Some(Duration::from_secs(5)), dest).await;
    let Err(MeowError::Io(io)) = res else {
        panic!("expected MeowError::Io(_), got {res:?}");
    };
    // The exact kind is OS-dependent (ConnectionRefused on most systems)
    // — just assert it isn't TimedOut, which would be a wrong-bucket bug.
    assert_ne!(
        io.kind(),
        std::io::ErrorKind::TimedOut,
        "real IO error must not be relabeled as TimedOut: {io}"
    );
}

/// Front role: a chained `UdpTarget::Name` arrives as host-only UDP
/// metadata (issue #657). Direct is the terminal hop — it resolves via
/// the adapter's configured resolver and returns a *connected* socket
/// whose write arg is advisory.
#[tokio::test]
async fn dial_udp_host_only_resolves_and_connects() {
    let echo = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let port = echo.local_addr().unwrap().port();
    let echo_task = tokio::spawn(async move {
        let mut buf = [0u8; 16];
        let (n, src) = echo.recv_from(&mut buf).await.unwrap();
        echo.send_to(&buf[..n], src).await.unwrap();
    });

    let mut hosts: DomainTrie<HostEntry> = DomainTrie::new();
    hosts.insert("bound.test", vec![IpAddr::V4(Ipv4Addr::LOCALHOST)].into());
    let resolver = Arc::new(Resolver::new(
        vec![],
        vec![],
        DnsMode::Normal,
        hosts,
        true,
        true,
    ));
    let adapter = DirectAdapter::new().with_resolver(resolver);

    let meta = Metadata {
        network: meow_common::Network::Udp,
        host: "bound.test".into(),
        dst_port: port,
        ..Default::default()
    };
    assert!(meta.domain_udp_target().is_some());
    let conn = adapter.dial_udp(&meta).await.expect("host-only dial_udp");

    // The caller's placeholder arg is ignored — the socket is bound.
    conn.write_packet(b"ping", &"0.0.0.0:0".parse().unwrap())
        .await
        .expect("write");
    let mut buf = [0u8; 16];
    let (n, src) = conn.read_packet(&mut buf).await.expect("read");
    assert_eq!(&buf[..n], b"ping");
    assert_eq!(src, SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port));
    echo_task.await.unwrap();
}

/// `EMFILE` on unix, `WSAEMFILE` on Windows — both are in
/// `is_local_resource_error`'s set.
#[cfg(unix)]
const EMFILE: i32 = 24;
#[cfg(windows)]
const EMFILE: i32 = 10024;

/// `Proxy` whose `dial_tcp` fails with an errno-bearing io error —
/// the dns-via-proxy upstream path hits a local-resource failure
/// without touching real sockets (#682).
struct ErrnoProxy {
    health: ProxyHealth,
}

#[async_trait]
impl ProxyAdapter for ErrnoProxy {
    fn name(&self) -> &str {
        "errno"
    }
    fn adapter_type(&self) -> AdapterType {
        AdapterType::Direct
    }
    fn addr(&self) -> &str {
        ""
    }
    fn support_udp(&self) -> bool {
        false
    }
    async fn dial_tcp(&self, _m: &Metadata) -> Result<Box<dyn ProxyConn>> {
        Err(MeowError::Io(std::io::Error::from_raw_os_error(EMFILE)))
    }
    async fn dial_udp(&self, _m: &Metadata) -> Result<Box<dyn ProxyPacketConn>> {
        unimplemented!("errno mock has no UDP")
    }
    fn health(&self) -> &ProxyHealth {
        &self.health
    }
}

impl meow_common::Proxy for ErrnoProxy {
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
    fn delay_history(&self) -> Vec<meow_common::DelayHistory> {
        Vec::new()
    }
}

/// Issue #701: a bracketed IPv6 display form in `metadata.host` (e.g. a
/// SOCKS5 domain `BND.ADDR` of `[::1]`, or a `server: "[::1]"` config)
/// folds to the literal instead of falling into DNS, where the
/// bracketed "domain" can never resolve.
#[tokio::test]
async fn resolve_targets_folds_bracketed_ipv6_host() {
    // No resolver injected: without the fold the bracketed "domain"
    // hits getaddrinfo and fails — the Ok arm below cannot be reached
    // from DNS on this input.
    let adapter = DirectAdapter::new();
    let metadata = tcp_metadata("[::1]", 443);
    let addrs = adapter
        .resolve_targets(&metadata)
        .await
        .expect("bracketed literal must fold, not resolve");
    assert_eq!(addrs, vec!["[::1]:443".parse().unwrap()]);
}

/// #682 end-to-end: a resolver whose upstream fails with EMFILE must
/// reach `resolve_targets` as `MeowError::Io` carrying the errno — the
/// classifier then treats it as local resource pressure instead of a
/// member-health failure.
#[tokio::test]
async fn resolve_targets_surfaces_resolver_io_errno() {
    use meow_dns::NameServerEntry;
    use smol_str::SmolStr;
    use std::collections::HashMap;

    let mut registry: HashMap<SmolStr, Arc<dyn meow_common::Proxy>> = HashMap::new();
    registry.insert(
        SmolStr::from("emfile"),
        Arc::new(ErrnoProxy {
            health: ProxyHealth::new(),
        }),
    );
    let resolver = Resolver::new_with_bootstrap_with_proxies(
        vec![NameServerEntry::parse("udp://203.0.113.53#emfile").unwrap()],
        vec![],
        vec![],
        DnsMode::Normal,
        DomainTrie::new(),
        true,
        true,
        None,
        None,
        &registry,
        false,
    )
    .await
    .expect("resolver builds");
    let adapter = DirectAdapter::new().with_resolver(Arc::new(resolver));

    let err = adapter
        .resolve_targets(&tcp_metadata("dial.example", 443))
        .await
        .expect_err("errno-bearing lookup failure must surface");
    assert!(
        matches!(&err, MeowError::Io(io) if io.raw_os_error() == Some(EMFILE)),
        "errno must survive into MeowError::Io, got {err:?}"
    );
    assert!(
        err.is_local_resource_error(),
        "the classifier must see local resource pressure: {err:?}"
    );
}
