//! Outbound-socket protector hook for Android `VpnService.protect(fd)`.
//!
//! When meow-rs runs *inside* an Android VPN app, every outbound socket it
//! opens must bypass the VPN itself — otherwise packets to proxy upstreams
//! loop back into the tunnel and deadlock. Android exposes a per-fd hook
//! for this: `android.net.VpnService.protect(int fd)`. This module is the
//! single place the JNI bridge installs that hook; the proxy adapters that
//! open outbound sockets dial through [`connect_tcp`] / [`connect_tcp_host`]
//! / [`bind_udp`], which call the installed protector before `connect()` /
//! `bind()` so the very first SYN / UDP packet already bypasses the tunnel.
//!
//! ## Why the second hook (`HostResolver`)
//!
//! `VpnService.protect(fd)` only protects sockets meow-rs creates itself.
//! Libc's `getaddrinfo` (called by `tokio::net::lookup_host` and
//! `TcpStream::connect("host:port")`) opens its own DNS sockets that
//! meow-rs never sees, so on a VPN-active device those DNS queries route
//! *through* the VPN — i.e. through meow-rs's own tunnel — which then needs
//! to dial a proxy upstream, which needs DNS, which loops. To break that
//! loop, [`connect_tcp_host`] consults an optionally-installed
//! `HostResolver` (typically backed by meow-rs's own `meow_dns::Resolver`
//! whose upstream sockets *are* protected) before falling back to the
//! system resolver.
//!
//! The `SocketProtector` (raw-fd) hook is compiled only on Android in
//! production — that's the platform whose VPN model demands it; for tests we
//! also enable it on any unix host so CI can exercise it against real
//! loopback sockets. The `HostResolver` hook carries no platform dependency
//! and is compiled everywhere: Android installs it alongside the protector,
//! iOS installs it on its own (the NE process needs the off-tunnel, async,
//! coalesced resolution but not raw-fd protection).

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::RwLock;
use tokio::net::{TcpStream, ToSocketAddrs, UdpSocket};

// ─── Host-resolver hook (cross-platform) ──────────────────────────────────
//
// Unlike the `SocketProtector` (which protects raw fds and is meaningful only
// inside Android's `VpnService` model — see the `android` module below), the
// hostname→IP hook is useful on every VPN platform. Routing a hostname dial
// through meow-rs's own `meow_dns::Resolver` (whose upstream sockets either
// bypass the tunnel or are protected) instead of libc's `getaddrinfo` gives
// us three things the system resolver can't: it never loops a lookup back
// through our own tunnel, it runs fully async (no blocking-pool thread per
// `getaddrinfo`, so a wake-time burst of dials can't exhaust the pool), and
// it coalesces + caches concurrent lookups for the same host. The registry
// itself carries no platform dependency, so it is compiled unconditionally;
// only the *installation* is platform-specific (Android JNI bridge / iOS FFI
// engine start).

/// Hostname → IP resolution hook consulted by [`connect_tcp_host`],
/// [`resolve_host`] and [`resolve_host_all`] whenever one is installed.
/// Typically backed by `meow_dns::ResolverHostHook`.
#[async_trait]
pub trait HostResolver: Send + Sync {
    /// Resolve `host` to one `IpAddr`. Returning `Err` aborts the dial.
    async fn resolve(&self, host: &str) -> io::Result<IpAddr>;

    /// Resolve `host` to all candidate addresses, in resolver order.
    ///
    /// Implementations that only support single-address lookup may rely on
    /// this default. Multi-address resolvers should override it so callers can
    /// fall back across families or stale endpoints before failing the dial.
    async fn resolve_all(&self, host: &str) -> io::Result<Vec<IpAddr>> {
        self.resolve(host).await.map(|ip| vec![ip])
    }

    /// Resolve `host` using only answers the implementation already holds —
    /// static mappings, warm cache — without any lookup that could itself
    /// need an outbound dial.
    ///
    /// Consulted instead of [`Self::resolve_all`] when the hook is re-entered
    /// — a nameserver that dials a proxy to answer a query, for the very
    /// hostname that proxy is reached at. `None` means "nothing known", and the
    /// caller falls through to the system resolver. The default answers
    /// nothing, which is always safe.
    fn resolve_all_local(&self, host: &str) -> Option<Vec<IpAddr>> {
        let _ = host;
        None
    }

    /// Async lookup for a re-entered hook, tried after
    /// [`Self::resolve_all_local`]. Implementations that must never reach the
    /// system resolver (e.g. encrypted-only DNS) answer here using a path
    /// that cannot re-enter, or return `Some(Err(..))` to fail closed. `None`
    /// (the default) keeps the historical system-resolver fallback.
    async fn resolve_all_nested(&self, host: &str) -> Option<io::Result<Vec<IpAddr>>> {
        let _ = host;
        None
    }
}

static RESOLVER: RwLock<Option<Arc<dyn HostResolver>>> = RwLock::new(None);

/// Install the global host resolver. Call once at startup, right after the
/// meow-rs `Resolver` is built. Safe to re-install (e.g. config reload) — the
/// new resolver takes effect on the next hostname dial.
pub fn set_host_resolver(resolver: Arc<dyn HostResolver>) {
    *RESOLVER.write() = Some(resolver);
}

/// Remove the currently installed host resolver, if any. Subsequent
/// hostname dials fall back to the system resolver.
pub fn clear_host_resolver() {
    *RESOLVER.write() = None;
}

/// Snapshot of the currently-installed host resolver.
pub fn host_resolver() -> Option<Arc<dyn HostResolver>> {
    RESOLVER.read().clone()
}

tokio::task_local! {
    /// Set for the duration of a [`HostResolver`] call made from
    /// [`resolve_addrs`]. See [`in_host_resolver`] for why.
    static IN_HOST_RESOLVER: ();
}

/// True when the current task is already inside a [`HostResolver`] call.
///
/// The hook is backed by meow-rs's own `meow_dns::Resolver`, and a nameserver
/// may be tagged `#PROXY` — i.e. answering the query means dialing a proxy,
/// which means resolving *that* proxy's server hostname, which re-enters the
/// hook. If the two hostnames coincide (the usual case: the proxy carrying
/// DNS is the one being dialed) the resolver's single-flight `inflight` map
/// would make the inner lookup wait on the outer one, deadlocking the dial
/// until it times out. Detecting the re-entry and falling back to the system
/// resolver for the inner lookup breaks the cycle. The whole chain — hook →
/// resolver → DNS client → proxy dial → hook — runs on one task, so a
/// task-local is a precise fence: unrelated concurrent dials keep using the
/// configured DNS.
fn in_host_resolver() -> bool {
    IN_HOST_RESOLVER.try_with(|()| ()).is_ok()
}

// ─── System-resolver result cache (Android / iOS only) ─────────────────────
//
// Backstop for the path that still hits libc `getaddrinfo` (no `HostResolver`
// installed, or one that defers to the system). A proxy upstream is dialed
// once per outbound flow, so a wake-from-sleep burst would otherwise fire one
// blocking `getaddrinfo` per flow against the same hostname while the network
// path is still transitioning. Caching the resolved addresses for a short TTL
// collapses that burst to a single lookup. Gated to the VPN platforms so
// desktop / CI dial behaviour is byte-for-byte unchanged.

/// How long a system-resolver result is reused before re-querying.
#[cfg(any(target_os = "android", target_os = "ios"))]
const SYS_DNS_TTL: std::time::Duration = std::time::Duration::from_secs(60);

#[cfg(any(target_os = "android", target_os = "ios"))]
fn sys_dns_cache(
) -> &'static RwLock<std::collections::HashMap<String, (Vec<IpAddr>, std::time::Instant)>> {
    static C: std::sync::OnceLock<
        RwLock<std::collections::HashMap<String, (Vec<IpAddr>, std::time::Instant)>>,
    > = std::sync::OnceLock::new();
    C.get_or_init(|| RwLock::new(std::collections::HashMap::new()))
}

/// Cached system-resolver addresses for `host`, or `None` on miss/expiry.
#[cfg(any(target_os = "android", target_os = "ios"))]
fn sys_cache_get(host: &str, port: u16) -> Option<Vec<SocketAddr>> {
    let guard = sys_dns_cache().read();
    let (ips, at) = guard.get(host)?;
    if at.elapsed() > SYS_DNS_TTL {
        return None;
    }
    Some(ips.iter().map(|ip| SocketAddr::new(*ip, port)).collect())
}
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn sys_cache_get(_host: &str, _port: u16) -> Option<Vec<SocketAddr>> {
    None
}

/// Record a fresh system-resolver result for `host`.
#[cfg(any(target_os = "android", target_os = "ios"))]
fn sys_cache_put(host: &str, addrs: &[SocketAddr]) {
    let ips: Vec<IpAddr> = addrs.iter().map(SocketAddr::ip).collect();
    sys_dns_cache()
        .write()
        .insert(host.to_string(), (ips, std::time::Instant::now()));
}
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn sys_cache_put(_host: &str, _addrs: &[SocketAddr]) {}

/// Drop any cached entry for `host` (called when every candidate failed to
/// connect, so the next dial re-resolves instead of reusing a dead address).
#[cfg(any(target_os = "android", target_os = "ios"))]
fn sys_cache_evict(host: &str) {
    sys_dns_cache().write().remove(host);
}
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn sys_cache_evict(_host: &str) {}

// In production we only compile the protector hook on Android — that's the
// platform whose VPN model demands `VpnService.protect(fd)` and we don't want
// non-Android binaries paying any footprint for it. For tests we additionally
// enable the module on any unix host so CI (which does not target Android)
// can actually exercise the protector path against real loopback sockets.
#[cfg(any(target_os = "android", all(test, unix)))]
mod android {
    use super::*;
    use std::os::fd::{AsRawFd, RawFd};
    use std::sync::Arc;

    use parking_lot::RwLock;

    /// Hook invoked on every outbound socket fd just before `connect()` /
    /// `bind()`. Typically a thin JNI shim around
    /// `android.net.VpnService.protect(int)`.
    ///
    /// Implementations must not block — the call runs on the async runtime
    /// worker that is dialing the socket.
    pub trait SocketProtector: Send + Sync {
        /// Protect `fd`. Returning `Err` aborts the connect/bind and the
        /// error propagates back to the caller.
        fn protect(&self, fd: RawFd) -> io::Result<()>;
    }

    static PROTECTOR: RwLock<Option<Arc<dyn SocketProtector>>> = RwLock::new(None);

    /// Install the global socket protector. Call once during VPN startup,
    /// before any proxy adapter dials.
    ///
    /// Re-installing is allowed (e.g. VPN tear-down / re-create); the new
    /// protector takes effect on the next outbound socket.
    pub fn set_socket_protector(protector: Arc<dyn SocketProtector>) {
        *PROTECTOR.write() = Some(protector);
    }

    /// Remove the currently installed protector, if any.
    pub fn clear_socket_protector() {
        *PROTECTOR.write() = None;
    }

    /// Snapshot of the currently-installed protector. Exposed so callers
    /// that build sockets through `socket2` directly (e.g. for `SO_MARK`)
    /// can still apply protect on the fd they own.
    pub fn socket_protector() -> Option<Arc<dyn SocketProtector>> {
        PROTECTOR.read().clone()
    }

    pub(super) async fn connect_tcp_protected(
        dest: SocketAddr,
        protector: &dyn SocketProtector,
    ) -> io::Result<TcpStream> {
        use socket2::{Domain, Protocol, Socket, Type};

        let domain = if dest.is_ipv4() {
            Domain::IPV4
        } else {
            Domain::IPV6
        };
        let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
        protector.protect(socket.as_raw_fd())?;
        socket.set_nonblocking(true)?;

        match socket.connect(&dest.into()) {
            Ok(()) => {}
            Err(e) if e.raw_os_error() == Some(libc::EINPROGRESS) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e),
        }

        let std_stream: std::net::TcpStream = socket.into();
        let stream = TcpStream::from_std(std_stream)?;
        stream.writable().await?;
        if let Some(err) = stream.take_error()? {
            return Err(err);
        }
        Ok(stream)
    }

    pub(super) fn bind_udp_protected(
        local: SocketAddr,
        protector: &dyn SocketProtector,
    ) -> io::Result<UdpSocket> {
        use socket2::{Domain, Protocol, Socket, Type};

        let domain = if local.is_ipv4() {
            Domain::IPV4
        } else {
            Domain::IPV6
        };
        let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
        protector.protect(socket.as_raw_fd())?;
        socket.set_nonblocking(true)?;
        socket.bind(&local.into())?;

        let std_socket: std::net::UdpSocket = socket.into();
        UdpSocket::from_std(std_socket)
    }
}

#[cfg(any(target_os = "android", all(test, unix)))]
pub use android::{
    clear_socket_protector, set_socket_protector, socket_protector, SocketProtector,
};

/// Dial an outbound TCP stream to an already-resolved [`SocketAddr`]. On
/// Android, applies the installed `SocketProtector` (if any) to the
/// socket fd before `connect()` so the connection bypasses the VPN. On
/// every other target this is equivalent to [`TcpStream::connect`].
///
/// Callers that only have a hostname must use [`connect_tcp_host`] —
/// passing a hostname here is a compile error by construction (the type
/// is `SocketAddr`, not `ToSocketAddrs`). That split exists to prevent
/// silent regressions back into the system resolver; see the module docs
/// for the loop-routing failure mode.
pub async fn connect_tcp(addr: SocketAddr) -> io::Result<TcpStream> {
    #[cfg(any(target_os = "android", all(test, unix)))]
    {
        if let Some(p) = android::socket_protector() {
            return android::connect_tcp_protected(addr, p.as_ref()).await;
        }
    }
    // TUN global-route loop avoidance (#375): when an outbound interface is
    // installed, bind the socket to it before connect() so the SYN already
    // takes the physical route past the TUN default routes.
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    {
        if crate::outbound_iface::binds_peer(addr.ip())
            && crate::outbound_iface::outbound_interface().is_some()
        {
            return connect_tcp_iface_bound(addr).await;
        }
    }
    TcpStream::connect(addr).await
}

/// Dial with the socket bound to the installed outbound interface
/// (`SO_BINDTODEVICE` / `IP_BOUND_IF` / `IP_UNICAST_IF`) before `connect()`.
/// Uses `tokio::net::TcpSocket` for the async connect so connection errors
/// surface here, not on first I/O.
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
pub(crate) async fn connect_tcp_iface_bound(addr: SocketAddr) -> io::Result<TcpStream> {
    let domain = if addr.is_ipv4() {
        socket2::Domain::IPV4
    } else {
        socket2::Domain::IPV6
    };
    let socket = socket2::Socket::new(domain, socket2::Type::STREAM, Some(socket2::Protocol::TCP))?;
    crate::outbound_iface::apply_outbound_interface(&socket, domain)?;
    socket.set_nonblocking(true)?;
    let tokio_socket = tokio::net::TcpSocket::from_std_stream(socket.into());
    tokio_socket.connect(addr).await
}

/// Single resolution chokepoint shared by [`connect_tcp_host`],
/// [`resolve_host`] and [`resolve_host_all`]. Resolution order:
///
/// 1. IP literal → returned verbatim (no lookup, no hook).
/// 2. Installed [`HostResolver`] (e.g. `meow_dns::ResolverHostHook`) → all
///    addresses it knows about. Preferred on every platform when present: it
///    runs async (no blocking-pool `getaddrinfo`), caches + coalesces
///    concurrent lookups, and on a VPN-active device its upstream sockets
///    don't loop the query back through our own tunnel. Consulted
///    independently of whether a `SocketProtector` is installed — iOS uses
///    the resolver hook without a protector.
///    Skipped when the call is itself nested inside a hook call (see
///    [`in_host_resolver`]).
/// 3. Short-TTL cache of prior system-resolver results (Android / iOS only)
///    → collapses a wake-time burst of dials to one `getaddrinfo`.
/// 4. System resolver (`tokio::net::lookup_host`), result cached.
///
/// Never returns an empty `Vec`.
async fn resolve_addrs(host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
    // `metadata_ip_literal` folds bracketed IPv6 display forms (`[::1]`)
    // into the literal — a `server:`/`BND.ADDR` string may arrive in that
    // shape and must not fall through to DNS (issue #701).
    if let Some(ip) = crate::metadata_ip_literal(host) {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }

    match host_resolver() {
        Some(r) if in_host_resolver() => {
            // Re-entered: answer from what the hook already knows, and only
            // fall through to the system resolver when it knows nothing.
            if let Some(ips) = r.resolve_all_local(host) {
                if !ips.is_empty() {
                    return Ok(ips
                        .into_iter()
                        .map(|ip| SocketAddr::new(ip, port))
                        .collect());
                }
            }
            if let Some(result) = r.resolve_all_nested(host).await {
                let ips = result?;
                if ips.is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("resolve: no address for {host}:{port}"),
                    ));
                }
                return Ok(ips
                    .into_iter()
                    .map(|ip| SocketAddr::new(ip, port))
                    .collect());
            }
            tracing::debug!(
                host,
                "resolve: re-entrant host-resolver call (a DNS upstream dials through a \
                 proxy) — using the system resolver for this hop to avoid a lookup cycle"
            );
        }
        Some(r) => return resolve_via_hook(r.as_ref(), host, port).await,
        None => {}
    }

    if let Some(cached) = sys_cache_get(host, port) {
        return Ok(cached);
    }

    #[cfg(any(target_os = "android", all(test, unix)))]
    {
        if android::socket_protector().is_some() {
            tracing::warn!(
                host,
                "resolve: protector installed but no HostResolver — falling back \
                 to system resolver, which may loop DNS through the VPN"
            );
        }
    }

    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port)).await?.collect();
    if addrs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("resolve: no address for {host}:{port}"),
        ));
    }
    sys_cache_put(host, &addrs);
    Ok(addrs)
}

/// Run one [`HostResolver`] lookup with [`IN_HOST_RESOLVER`] set, so a dial
/// made while answering it takes the system-resolver path instead of
/// re-entering the hook.
async fn resolve_via_hook(
    r: &dyn HostResolver,
    host: &str,
    port: u16,
) -> io::Result<Vec<SocketAddr>> {
    let ips = IN_HOST_RESOLVER.scope((), r.resolve_all(host)).await?;
    if ips.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("resolve: no address for {host}:{port}"),
        ));
    }
    Ok(ips
        .into_iter()
        .map(|ip| SocketAddr::new(ip, port))
        .collect())
}

/// Dial an outbound TCP stream to `host:port`. The hostname is resolved
/// through `resolve_addrs` (installed `HostResolver` → cache → system
/// resolver), then each candidate is dialed via [`connect_tcp`] so the
/// `SocketProtector` (Android) still applies. If every candidate fails to
/// connect, any cached system-resolver entry is evicted so the next dial
/// re-resolves instead of reusing a dead address.
///
/// IP literals short-circuit inside `resolve_addrs` — no resolver hook is
/// consulted, but the protector still applies to the literal dial.
pub async fn connect_tcp_host(host: &str, port: u16) -> io::Result<TcpStream> {
    let addrs = resolve_addrs(host, port).await?;
    let mut last_err: Option<io::Error> = None;
    for addr in &addrs {
        match connect_tcp(*addr).await {
            Ok(stream) => return Ok(stream),
            Err(e) => {
                // An errno-backed failure (e.g. EMFILE on socket()) must
                // not be masked by a later errno-less error — it carries
                // the local-vs-member classification (issue #668).
                last_err = crate::MeowError::prefer_errno_io(last_err, e);
            }
        }
    }
    // Every candidate failed — drop any cached system entry so the next dial
    // re-resolves. No-op for the literal / resolver-hook paths (not cached).
    sys_cache_evict(host);
    Err(last_err.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "connect_tcp_host: no addresses resolved",
        )
    }))
}

/// Resolve `host` to a single [`SocketAddr`] (the first candidate from
/// [`resolve_host_all`]).
///
/// Prefer [`resolve_host_all`] for call sites that can try candidates in
/// order (e.g. a UDP `connect()` loop): taking only the first address
/// silently drops the family fallback that `TcpStream::connect` gets for
/// free, which strands the caller on an unreachable family when the
/// resolver orders AAAA first on an IPv4-only network (or vice versa).
pub async fn resolve_host(host: &str, port: u16) -> io::Result<SocketAddr> {
    resolve_host_all(host, port).await.map(|addrs| addrs[0])
}

/// Resolve `host` to every candidate [`SocketAddr`], in resolver order,
/// via `resolve_addrs`. Never returns an empty `Vec` — no-address
/// resolution is an `Err`, so callers may index `[0]` safely.
pub async fn resolve_host_all(host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
    resolve_addrs(host, port).await
}

/// Bind an outbound UDP socket. On Android, applies the installed
/// `SocketProtector` (if any) to the socket fd before `bind()`. On every
/// other target this is equivalent to [`UdpSocket::bind`].
pub async fn bind_udp<A: ToSocketAddrs>(local: A) -> io::Result<UdpSocket> {
    #[cfg(any(target_os = "android", all(test, unix)))]
    {
        if let Some(p) = android::socket_protector() {
            let resolved = tokio::net::lookup_host(local)
                .await?
                .next()
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "bind_udp: no address resolved")
                })?;
            return android::bind_udp_protected(resolved, p.as_ref());
        }
    }
    // TUN global-route loop avoidance (#375): bind the socket to the
    // installed outbound interface before the local-address bind, mirroring
    // `connect_tcp` — so the first datagram already bypasses the TUN routes.
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    {
        if crate::outbound_iface::outbound_interface().is_some() {
            let resolved = tokio::net::lookup_host(local)
                .await?
                .next()
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "bind_udp: no address resolved")
                })?;
            return bind_udp_iface_bound(resolved);
        }
    }
    UdpSocket::bind(local).await
}

/// Bind a UDP socket to the installed outbound interface
/// (`SO_BINDTODEVICE` / `IP_BOUND_IF` / `IP_UNICAST_IF`), then to `local`.
/// A socket pinned to a loopback `local` address is left unbound where the
/// platform's binding would break it (see `outbound_iface::binds_peer`).
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
pub(crate) fn bind_udp_iface_bound(local: SocketAddr) -> io::Result<UdpSocket> {
    let domain = if local.is_ipv4() {
        socket2::Domain::IPV4
    } else {
        socket2::Domain::IPV6
    };
    let socket = socket2::Socket::new(domain, socket2::Type::DGRAM, Some(socket2::Protocol::UDP))?;
    if crate::outbound_iface::binds_peer(local.ip()) {
        crate::outbound_iface::apply_outbound_interface(&socket, domain)?;
    }
    socket.bind(&local.into())?;
    socket.set_nonblocking(true)?;
    UdpSocket::from_std(socket.into())
}

#[cfg(all(test, unix))]
#[path = "test/socket_protect_test.rs"]
mod tests;
