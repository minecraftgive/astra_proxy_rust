use super::*;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;

/// Records every fd handed to it; never fails.
struct Counting {
    count: AtomicUsize,
    seen_fds: parking_lot::Mutex<Vec<RawFd>>,
}
impl Counting {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            count: AtomicUsize::new(0),
            seen_fds: parking_lot::Mutex::new(Vec::new()),
        })
    }
    fn count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }
}
impl SocketProtector for Counting {
    fn protect(&self, fd: RawFd) -> io::Result<()> {
        self.count.fetch_add(1, Ordering::SeqCst);
        self.seen_fds.lock().push(fd);
        Ok(())
    }
}

/// Always errors — used to verify failure propagation.
struct Failing;
impl SocketProtector for Failing {
    fn protect(&self, _fd: RawFd) -> io::Result<()> {
        Err(io::Error::other("protect denied"))
    }
}

/// Counts every resolver call and returns fixed `IpAddr` candidates.
struct FixedResolver {
    ips: Vec<IpAddr>,
    count: AtomicUsize,
    last_host: parking_lot::Mutex<Option<String>>,
}
impl FixedResolver {
    fn new(ip: IpAddr) -> Arc<Self> {
        Self::new_all(vec![ip])
    }

    fn new_all(ips: Vec<IpAddr>) -> Arc<Self> {
        Arc::new(Self {
            ips,
            count: AtomicUsize::new(0),
            last_host: parking_lot::Mutex::new(None),
        })
    }
    fn count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }
    fn last_host(&self) -> Option<String> {
        self.last_host.lock().clone()
    }
}
#[async_trait]
impl HostResolver for FixedResolver {
    async fn resolve(&self, host: &str) -> io::Result<IpAddr> {
        self.count.fetch_add(1, Ordering::SeqCst);
        *self.last_host.lock() = Some(host.to_string());
        self.ips
            .first()
            .copied()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no fixed address"))
    }

    async fn resolve_all(&self, host: &str) -> io::Result<Vec<IpAddr>> {
        self.count.fetch_add(1, Ordering::SeqCst);
        *self.last_host.lock() = Some(host.to_string());
        Ok(self.ips.clone())
    }
}

/// Errors on every `resolve`.
struct FailingResolver;
#[async_trait]
impl HostResolver for FailingResolver {
    async fn resolve(&self, _host: &str) -> io::Result<IpAddr> {
        Err(io::Error::other("resolve denied"))
    }

    async fn resolve_all(&self, _host: &str) -> io::Result<Vec<IpAddr>> {
        Err(io::Error::other("resolve denied"))
    }
}

/// Tracks the order of calls — used to verify protect runs BEFORE
/// connect/bind. The fd argument must already be a valid (created) socket
/// when protect is called, but the kernel `connect()` syscall must not
/// have fired yet. We approximate this by observing that the fd is open
/// (`fcntl(F_GETFD)` succeeds) but `getpeername` returns ENOTCONN.
struct OrderingProbe {
    was_pre_connect: parking_lot::Mutex<Option<bool>>,
}
impl OrderingProbe {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            was_pre_connect: parking_lot::Mutex::new(None),
        })
    }
    fn was_pre_connect(&self) -> Option<bool> {
        *self.was_pre_connect.lock()
    }
}
impl SocketProtector for OrderingProbe {
    fn protect(&self, fd: RawFd) -> io::Result<()> {
        // SAFETY: fd is owned by the caller (still in socket2 wrapper);
        // F_GETFD doesn't mutate state, getpeername reads kernel state.
        let fd_open = unsafe { libc::fcntl(fd, libc::F_GETFD) } != -1;
        let mut sa: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of_val(&sa) as libc::socklen_t;
        let getpeer =
            unsafe { libc::getpeername(fd, &mut sa as *mut _ as *mut libc::sockaddr, &mut len) };
        let not_yet_connected =
            getpeer == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOTCONN);
        *self.was_pre_connect.lock() = Some(fd_open && not_yet_connected);
        Ok(())
    }
}

/// Serialise tests that touch the process-global protector — they would
/// otherwise race each other and corrupt count assertions. `tokio::sync`
/// (not `parking_lot`) so the guard can be held across `.await`.
static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// ─── connect_tcp ─────────────────────────────────────────────────────────

#[tokio::test]
async fn no_protector_falls_back_to_plain_tokio() {
    let _g = LOCK.lock().await;
    clear_socket_protector();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accept = tokio::spawn(async move { listener.accept().await.unwrap() });
    let stream = connect_tcp(addr).await.expect("connect");
    let _ = accept.await.unwrap();
    // Round-trips like a normal stream.
    drop(stream);
}

#[tokio::test]
async fn connect_tcp_invokes_protector_against_real_listener() {
    let _g = LOCK.lock().await;
    let counter = Counting::new();
    set_socket_protector(Arc::clone(&counter) as Arc<dyn SocketProtector>);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accept = tokio::spawn(async move { listener.accept().await.unwrap() });
    let stream = connect_tcp(addr).await.expect("connect");
    let _ = accept.await.unwrap();
    drop(stream);
    assert_eq!(counter.count(), 1, "protector must be called exactly once");
    clear_socket_protector();
}

#[tokio::test]
async fn connect_tcp_propagates_protector_failure() {
    let _g = LOCK.lock().await;
    set_socket_protector(Arc::new(Failing) as Arc<dyn SocketProtector>);
    // Real listener so the error can only come from the protector, not
    // a network error masquerading as one.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let err = connect_tcp(addr).await.expect_err("protect should fail");
    assert!(err.to_string().contains("protect denied"), "{err}");
    clear_socket_protector();
}

#[tokio::test]
async fn protect_runs_before_connect_syscall() {
    let _g = LOCK.lock().await;
    let probe = OrderingProbe::new();
    set_socket_protector(Arc::clone(&probe) as Arc<dyn SocketProtector>);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accept = tokio::spawn(async move { listener.accept().await.unwrap() });
    let _stream = connect_tcp(addr).await.expect("connect");
    let _ = accept.await.unwrap();
    assert_eq!(
        probe.was_pre_connect(),
        Some(true),
        "fd must be open but not-yet-connected at the moment protect() runs — \
             this is the invariant Android's VpnService.protect relies on"
    );
    clear_socket_protector();
}

// ─── connect_tcp_host ────────────────────────────────────────────────────

#[tokio::test]
async fn connect_tcp_host_with_ip_literal_skips_resolver() {
    let _g = LOCK.lock().await;
    clear_socket_protector();
    clear_host_resolver();
    // Install a resolver that would fail if it were called — proving
    // the IP-literal short-circuit really did skip it.
    set_host_resolver(Arc::new(FailingResolver) as Arc<dyn HostResolver>);
    let counter = Counting::new();
    set_socket_protector(Arc::clone(&counter) as Arc<dyn SocketProtector>);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let accept = tokio::spawn(async move { listener.accept().await.unwrap() });
    let _stream = connect_tcp_host("127.0.0.1", port).await.expect("connect");
    let _ = accept.await.unwrap();
    assert_eq!(counter.count(), 1, "protector still applies to literal");

    clear_socket_protector();
    clear_host_resolver();
}

/// Issue #701: a bracketed IPv6 display form (`server: "[::1]"`) must
/// fold to the literal inside `resolve_addrs` — never reach the
/// resolver, which would fail on the bracketed "domain".
#[tokio::test]
async fn connect_tcp_host_with_bracketed_ipv6_literal_skips_resolver() {
    let _g = LOCK.lock().await;
    clear_socket_protector();
    clear_host_resolver();
    set_host_resolver(Arc::new(FailingResolver) as Arc<dyn HostResolver>);

    let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let accept = tokio::spawn(async move { listener.accept().await.unwrap() });
    let _stream = connect_tcp_host("[::1]", port).await.expect("connect");
    let _ = accept.await.unwrap();

    clear_host_resolver();
}

#[tokio::test]
async fn connect_tcp_host_uses_installed_resolver_when_protected() {
    let _g = LOCK.lock().await;
    let counter = Counting::new();
    set_socket_protector(Arc::clone(&counter) as Arc<dyn SocketProtector>);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let resolver = FixedResolver::new_all(vec![
        IpAddr::V6(Ipv6Addr::LOCALHOST),
        IpAddr::from([127, 0, 0, 1]),
    ]);
    set_host_resolver(Arc::clone(&resolver) as Arc<dyn HostResolver>);

    let accept = tokio::spawn(async move { listener.accept().await.unwrap() });
    let _stream = connect_tcp_host("example.invalid", port)
        .await
        .expect("connect");
    let _ = accept.await.unwrap();

    assert_eq!(resolver.count(), 1, "resolver must be consulted once");
    assert_eq!(resolver.last_host().as_deref(), Some("example.invalid"));
    assert_eq!(
        counter.count(),
        2,
        "protector must apply to each resolved connect candidate"
    );

    clear_host_resolver();
    clear_socket_protector();
}

#[tokio::test]
async fn connect_tcp_host_propagates_resolver_failure() {
    let _g = LOCK.lock().await;
    let counter = Counting::new();
    set_socket_protector(Arc::clone(&counter) as Arc<dyn SocketProtector>);
    set_host_resolver(Arc::new(FailingResolver) as Arc<dyn HostResolver>);

    let err = connect_tcp_host("example.invalid", 80)
        .await
        .expect_err("resolver should fail");
    assert!(err.to_string().contains("resolve denied"), "{err}");

    clear_host_resolver();
    clear_socket_protector();
}

#[tokio::test]
async fn connect_tcp_host_without_protector_uses_plain_tokio() {
    let _g = LOCK.lock().await;
    clear_socket_protector();
    clear_host_resolver();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let accept = tokio::spawn(async move { listener.accept().await.unwrap() });
    let _stream = connect_tcp_host("127.0.0.1", port).await.expect("connect");
    let _ = accept.await.unwrap();
}

// ─── bind_udp ────────────────────────────────────────────────────────────

#[tokio::test]
async fn bind_udp_invokes_protector() {
    let _g = LOCK.lock().await;
    let counter = Counting::new();
    set_socket_protector(Arc::clone(&counter) as Arc<dyn SocketProtector>);
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let sock = bind_udp(addr).await.expect("bind");
    assert!(sock.local_addr().unwrap().port() != 0);
    assert_eq!(counter.count(), 1);
    clear_socket_protector();
}

#[tokio::test]
async fn bind_udp_propagates_protector_failure() {
    let _g = LOCK.lock().await;
    set_socket_protector(Arc::new(Failing) as Arc<dyn SocketProtector>);
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let err = bind_udp(addr).await.expect_err("protect should fail");
    assert!(err.to_string().contains("protect denied"), "{err}");
    clear_socket_protector();
}

#[tokio::test]
async fn bind_udp_no_protector_falls_back_to_plain_tokio() {
    let _g = LOCK.lock().await;
    clear_socket_protector();
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let sock = bind_udp(addr).await.expect("bind");
    assert!(sock.local_addr().unwrap().port() != 0);
}

// ─── resolve_host / resolve_host_all ─────────────────────────────────────

#[tokio::test]
async fn resolve_host_all_ip_literal_is_single_candidate() {
    let _g = LOCK.lock().await;
    clear_socket_protector();
    clear_host_resolver();
    // Installing a failing resolver + protector proves the literal
    // short-circuit never consults the hook chain.
    set_socket_protector(Counting::new() as Arc<dyn SocketProtector>);
    set_host_resolver(Arc::new(FailingResolver) as Arc<dyn HostResolver>);

    let addrs = resolve_host_all("192.0.2.7", 443).await.expect("literal");
    assert_eq!(addrs, vec!["192.0.2.7:443".parse().unwrap()]);

    clear_host_resolver();
    clear_socket_protector();
}

#[tokio::test]
async fn resolve_host_all_uses_installed_resolver_when_protected() {
    let _g = LOCK.lock().await;
    set_socket_protector(Counting::new() as Arc<dyn SocketProtector>);
    let resolver = FixedResolver::new_all(vec![
        IpAddr::from([127, 0, 0, 1]),
        IpAddr::from([127, 0, 0, 2]),
    ]);
    set_host_resolver(Arc::clone(&resolver) as Arc<dyn HostResolver>);

    let addrs = resolve_host_all("example.invalid", 8388)
        .await
        .expect("resolve");
    assert_eq!(
        addrs,
        vec![
            "127.0.0.1:8388".parse().unwrap(),
            "127.0.0.2:8388".parse().unwrap()
        ]
    );
    assert_eq!(resolver.count(), 1, "resolver must be consulted once");

    clear_host_resolver();
    clear_socket_protector();
}

#[tokio::test]
async fn resolve_host_first_matches_resolve_host_all_head() {
    let _g = LOCK.lock().await;
    clear_socket_protector();
    clear_host_resolver();

    // `localhost` resolves through the system resolver (/etc/hosts) and
    // may legitimately return both families — exactly the multi-candidate
    // shape resolve_host_all exists for.
    let all = resolve_host_all("localhost", 1080).await.expect("resolve");
    assert!(!all.is_empty(), "resolve_host_all never returns empty Ok");
    let single = resolve_host("localhost", 1080).await.expect("resolve");
    assert_eq!(
        single, all[0],
        "resolve_host must stay an alias for the first candidate"
    );
}

// ─── registry semantics ─────────────────────────────────────────────────

#[tokio::test]
async fn protector_set_get_clear_round_trip() {
    let _g = LOCK.lock().await;
    clear_socket_protector();
    assert!(socket_protector().is_none());

    let c = Counting::new();
    set_socket_protector(Arc::clone(&c) as Arc<dyn SocketProtector>);
    assert!(socket_protector().is_some(), "registered protector visible");

    clear_socket_protector();
    assert!(
        socket_protector().is_none(),
        "clear must remove the registration"
    );
}

#[tokio::test]
async fn protector_replacement_takes_effect_for_subsequent_lookups() {
    let _g = LOCK.lock().await;
    clear_socket_protector();
    let first = Counting::new();
    set_socket_protector(Arc::clone(&first) as Arc<dyn SocketProtector>);

    let second = Counting::new();
    set_socket_protector(Arc::clone(&second) as Arc<dyn SocketProtector>);

    // The currently-installed pointer is the second one.
    let snap = socket_protector().expect("protector installed");
    let _ = snap.protect(0); // arbitrary fd; Counting accepts any
    assert_eq!(first.count(), 0, "old protector must be detached");
    assert_eq!(second.count(), 1, "new protector receives calls");

    clear_socket_protector();
}

#[tokio::test]
async fn host_resolver_set_get_clear_round_trip() {
    let _g = LOCK.lock().await;
    clear_host_resolver();
    assert!(host_resolver().is_none());

    let r = FixedResolver::new(IpAddr::from([127, 0, 0, 1]));
    set_host_resolver(Arc::clone(&r) as Arc<dyn HostResolver>);
    assert!(host_resolver().is_some());

    clear_host_resolver();
    assert!(host_resolver().is_none());
}

/// Stands in for a `#PROXY`-tagged nameserver: answering a lookup means
/// dialing a proxy, which resolves that proxy's own server hostname.
struct ReentrantResolver {
    calls: AtomicUsize,
    inner: parking_lot::Mutex<Option<io::Result<Vec<SocketAddr>>>>,
}
#[async_trait]
impl HostResolver for ReentrantResolver {
    async fn resolve(&self, host: &str) -> io::Result<IpAddr> {
        self.resolve_all(host).await.map(|ips| ips[0])
    }

    async fn resolve_all(&self, _host: &str) -> io::Result<Vec<IpAddr>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        // The nested dial the DNS upstream would make.
        *self.inner.lock() = Some(resolve_host_all("localhost", 53).await);
        Ok(vec![IpAddr::from([203, 0, 113, 7])])
    }
}

#[tokio::test]
async fn nested_dial_inside_the_hook_uses_the_system_resolver() {
    let _g = LOCK.lock().await;
    clear_host_resolver();
    let r = Arc::new(ReentrantResolver {
        calls: AtomicUsize::new(0),
        inner: parking_lot::Mutex::new(None),
    });
    set_host_resolver(Arc::clone(&r) as Arc<dyn HostResolver>);

    let addrs = resolve_host_all("proxy.example", 443)
        .await
        .expect("outer lookup answered by the hook");
    assert_eq!(addrs, vec![SocketAddr::from(([203, 0, 113, 7], 443))]);

    // One hook call, not two: the nested dial must not re-enter it, or a
    // real resolver's single-flight map would deadlock the pair.
    assert_eq!(r.calls.load(Ordering::SeqCst), 1);
    let inner = r.inner.lock().take().expect("nested dial ran");
    let inner = inner.expect("system resolver answered the nested dial");
    assert!(
        inner.iter().all(|a| a.ip().is_loopback()),
        "nested dial must come from the system resolver, got {inner:?}"
    );

    clear_host_resolver();
}

/// Same nesting, but the hook knows the inner host statically (a `hosts:`
/// entry or a warm cache line), so the system resolver is never reached.
struct LocalAnswerResolver {
    local_calls: AtomicUsize,
    inner: parking_lot::Mutex<Option<io::Result<Vec<SocketAddr>>>>,
}
#[async_trait]
impl HostResolver for LocalAnswerResolver {
    async fn resolve(&self, host: &str) -> io::Result<IpAddr> {
        self.resolve_all(host).await.map(|ips| ips[0])
    }

    async fn resolve_all(&self, _host: &str) -> io::Result<Vec<IpAddr>> {
        *self.inner.lock() = Some(resolve_host_all("node.example", 8388).await);
        Ok(vec![IpAddr::from([203, 0, 113, 7])])
    }

    fn resolve_all_local(&self, _host: &str) -> Option<Vec<IpAddr>> {
        self.local_calls.fetch_add(1, Ordering::SeqCst);
        Some(vec![IpAddr::from([192, 0, 2, 55])])
    }
}

#[tokio::test]
async fn nested_dial_prefers_the_hooks_local_answer() {
    let _g = LOCK.lock().await;
    clear_host_resolver();
    let r = Arc::new(LocalAnswerResolver {
        local_calls: AtomicUsize::new(0),
        inner: parking_lot::Mutex::new(None),
    });
    set_host_resolver(Arc::clone(&r) as Arc<dyn HostResolver>);

    resolve_host_all("proxy.example", 443).await.unwrap();

    assert_eq!(r.local_calls.load(Ordering::SeqCst), 1);
    let inner = r.inner.lock().take().unwrap().unwrap();
    assert_eq!(
        inner,
        vec![SocketAddr::from(([192, 0, 2, 55], 8388))],
        "the nested hop must take the hook's local answer, not the OS resolver"
    );

    clear_host_resolver();
}

#[tokio::test]
async fn hook_applies_again_after_the_nested_scope_ends() {
    let _g = LOCK.lock().await;
    clear_host_resolver();
    let r = FixedResolver::new(IpAddr::from([198, 51, 100, 4]));
    set_host_resolver(Arc::clone(&r) as Arc<dyn HostResolver>);

    for _ in 0..2 {
        let addrs = resolve_host_all("node.example", 8388).await.unwrap();
        assert_eq!(addrs, vec![SocketAddr::from(([198, 51, 100, 4], 8388))]);
    }
    assert_eq!(r.count(), 2, "the guard must not leak across calls");

    clear_host_resolver();
}
