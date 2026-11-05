use async_trait::async_trait;
use meow_common::{
    AdapterType, MeowError, Metadata, ProxyAdapter, ProxyConn, ProxyHealth, ProxyPacketConn, Result,
};
use meow_dns::Resolver;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpStream, UdpSocket};

pub struct DirectAdapter {
    /// `Direct` normally; `Compatible` for the built-in `COMPATIBLE`
    /// adapter (upstream `outbound.NewCompatible()` — a `*Direct` with the
    /// `C.Compatible` type tag, used as GLOBAL's default member).
    adapter_type: AdapterType,
    routing_mark: Option<u32>,
    /// Optional internal DNS resolver. When set, `dial_tcp` resolves
    /// hostnames via this resolver instead of the OS resolver — this is
    /// important when meow-rs *is* the system DNS, because routing a direct
    /// DNS query back through the OS would loop the query back into meow-rs.
    /// Behind a slot so `Tunnel::set_resolver` can hot-swap the generation
    /// on `PUT /configs` without rebuilding the adapter (issue #514). When
    /// built via `with_resolver_slot` the slot is shared with the tunnel,
    /// so map `DIRECT` and `TunnelInner.direct` track one generation.
    resolver: Option<meow_dns::ResolverSlot>,
    /// Wall-clock bound on `TcpStream::connect`. iOS / macOS scoped-routing
    /// and reachability-cache transients can leave a `connect()` hanging
    /// indefinitely against a destination whose route is in flux (Wi-Fi
    /// assoc churn, IPv6 RA churn, post-wake route reassessment). Without
    /// this bound the dial holds whatever upstream scheduling resource the
    /// caller allocated to it until the OS gives up (~75 s on iOS BSD-style
    /// SYN retransmit grid). `None` preserves the legacy unbounded
    /// behaviour for downstream consumers that haven't opted in.
    connect_timeout: Option<Duration>,
    health: ProxyHealth,
}

impl DirectAdapter {
    pub fn new() -> Self {
        Self {
            adapter_type: AdapterType::Direct,
            routing_mark: None,
            resolver: None,
            connect_timeout: None,
            health: ProxyHealth::new(),
        }
    }

    /// `COMPATIBLE` built-in — a direct dialer carrying the `Compatible`
    /// type tag, like upstream `NewCompatible`. Receives the same
    /// routing-mark/resolver/timeout options as `DIRECT` via the `with_*`
    /// builders.
    pub fn compatible() -> Self {
        Self {
            adapter_type: AdapterType::Compatible,
            ..Self::new()
        }
    }

    pub fn with_routing_mark(mut self, routing_mark: u32) -> Self {
        self.routing_mark = Some(routing_mark);
        self
    }

    /// Wrap `resolver` in a fresh private slot — this adapter will not
    /// follow later `Tunnel::set_resolver` swaps. Prefer
    /// [`Self::with_resolver_slot`] when a shared generation is intended.
    pub fn with_resolver(mut self, resolver: Arc<Resolver>) -> Self {
        self.resolver = Some(meow_dns::new_resolver_slot(resolver));
        self
    }

    /// Share an existing resolver slot — writes to the slot (e.g.
    /// `Tunnel::set_resolver`) are observed by this adapter on every dial
    /// (issue #514).
    pub fn with_resolver_slot(mut self, slot: meow_dns::ResolverSlot) -> Self {
        self.resolver = Some(slot);
        self
    }

    /// Bound `TcpStream::connect` on `dial_tcp`. Returns `MeowError::Io`
    /// with `ErrorKind::TimedOut` if the connect exceeds `timeout`. See
    /// the `connect_timeout` field doc for the motivating failure mode
    /// (iOS routing-cache transients) and meow-ios'
    /// `docs/INVESTIGATION-2026-05-18-tcp-direct-rule-disconnect.md` for
    /// the device-side trace.
    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = Some(timeout);
        self
    }

    /// Read back the configured connect bound. `None` = unbounded. Lets
    /// config-layer tests assert the `tcp-connect-timeout` /
    /// `connect-timeout` wiring without dialing anything.
    pub fn connect_timeout(&self) -> Option<Duration> {
        self.connect_timeout
    }

    /// Determine the concrete `SocketAddr` candidates to dial for `metadata`,
    /// avoiding the OS resolver whenever possible.
    async fn resolve_targets(&self, metadata: &Metadata) -> Result<Vec<SocketAddr>> {
        // 1. Destination already resolved (e.g. by rule-matching pre_resolve,
        //    or when the client supplied an IP literal).
        if let Some(ip) = metadata.dst_ip {
            return Ok(vec![SocketAddr::new(ip, metadata.dst_port)]);
        }

        // 2. `host` is an IP literal — no DNS needed. The fold also
        //    unwraps a bracketed display form (e.g. a SOCKS5 domain
        //    BND.ADDR of `[::1]`) so it cannot fall into DNS (issue #701).
        if let Some(ip) = meow_common::metadata_ip_literal(&metadata.host) {
            return Ok(vec![SocketAddr::new(ip, metadata.dst_port)]);
        }

        // 3. Resolve via meow-rs's internal resolver if available. Falls back
        //    to the OS resolver only when no resolver was injected (tests,
        //    standalone usage).
        if !metadata.host.is_empty() {
            if let Some(resolver) = &self.resolver {
                let resolver = Arc::clone(&resolver.read());
                return match resolver.resolve_ips(&metadata.host).await {
                    Ok(Some(ips)) if !ips.is_empty() => Ok(ips
                        .into_iter()
                        .map(|ip| SocketAddr::new(ip, metadata.dst_port))
                        .collect()),
                    // The lookup itself failed — the io error keeps its
                    // raw_os_error, so a local EMFILE while opening the
                    // resolver socket stays `is_local_resource_error`
                    // instead of dead-marking this member (#682).
                    Err(e) => Err(MeowError::Io(e)),
                    Ok(None) => Err(MeowError::Dns(format!(
                        "direct: failed to resolve {}",
                        metadata.host
                    ))),
                    Ok(Some(_)) => Err(MeowError::Dns(format!(
                        "direct: no address for {}",
                        metadata.host
                    ))),
                };
            }

            // Legacy fallback: let tokio use getaddrinfo. Only reachable when
            // no resolver was injected — production code paths always inject.
            let addrs: Vec<SocketAddr> =
                tokio::net::lookup_host((&*metadata.host, metadata.dst_port))
                    .await
                    .map_err(MeowError::Io)?
                    .collect();
            if addrs.is_empty() {
                return Err(MeowError::Dns(format!(
                    "direct: no address for {}:{}",
                    metadata.host, metadata.dst_port
                )));
            }
            return Ok(addrs);
        }

        Err(MeowError::Proxy(
            "direct: metadata has no destination".into(),
        ))
    }
}

impl Default for DirectAdapter {
    fn default() -> Self {
        Self::new()
    }
}

// Wrapper for TcpStream that implements ProxyConn
struct DirectConn(TcpStream);

impl tokio::io::AsyncRead for DirectConn {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for DirectConn {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

impl Unpin for DirectConn {}
impl ProxyConn for DirectConn {}

// UDP wrapper
struct DirectPacketConn {
    socket: UdpSocket,
    /// `Some` when the socket is `connect()`ed to a name-resolved target
    /// (a chained `UdpTarget::Name`, issue #657): writes ignore the
    /// advisory arg and reads report the connected peer.
    bound: Option<SocketAddr>,
}

#[async_trait]
impl ProxyPacketConn for DirectPacketConn {
    async fn read_packet(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr)> {
        self.socket.recv_from(buf).await.map_err(MeowError::Io)
    }

    async fn write_packet(&self, buf: &[u8], addr: &SocketAddr) -> Result<usize> {
        match self.bound {
            // send_to() with the caller's placeholder arg would fail with
            // EISCONN on a connected socket.
            Some(_) => self.socket.send(buf).await.map_err(MeowError::Io),
            None => self.socket.send_to(buf, addr).await.map_err(MeowError::Io),
        }
    }

    fn local_addr(&self) -> Result<SocketAddr> {
        self.socket.local_addr().map_err(MeowError::Io)
    }

    fn close(&self) -> Result<()> {
        Ok(())
    }
}

/// Wrap a connect future in the adapter-configured timeout. Returns the
/// stream on success, or a `MeowError::Io(TimedOut)` whose message identifies
/// the destination and the budget that elapsed when the budget is hit.
///
/// Lives next to `dial_tcp` rather than inline so the timeout behaviour can
/// be exercised in tests against a deterministic future (e.g. `pending()`)
/// instead of relying on a real-network black-hole — see the unit tests at
/// the bottom of this file.
async fn apply_connect_timeout<F>(
    connect: F,
    timeout: Option<Duration>,
    dest: SocketAddr,
) -> Result<TcpStream>
where
    F: std::future::Future<Output = std::io::Result<TcpStream>>,
{
    match timeout {
        Some(t) => match tokio::time::timeout(t, connect).await {
            Ok(res) => res.map_err(MeowError::Io),
            Err(_) => Err(MeowError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("direct: connect to {dest} timed out after {t:?}"),
            ))),
        },
        None => connect.await.map_err(MeowError::Io),
    }
}

/// Create a TCP socket with an optional routing mark (SO_MARK on Linux)
/// set BEFORE connecting, so the SYN packet is already marked. On Android
/// the installed `meow_common::SocketProtector` is applied to the socket
/// fd (also pre-connect) so the dial bypasses VpnService when meow-rs runs
/// inside a VPN app.
async fn connect_with_mark(
    dest: SocketAddr,
    routing_mark: Option<u32>,
) -> std::io::Result<TcpStream> {
    #[cfg(target_os = "linux")]
    if let Some(mark) = routing_mark {
        use socket2::{Domain, Protocol, Socket, Type};

        let domain = if dest.is_ipv4() {
            Domain::IPV4
        } else {
            Domain::IPV6
        };

        let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
        socket.set_mark(mark)?;
        // TUN global-route loop avoidance (#375): this branch bypasses
        // `meow_common::connect_tcp`, so apply the outbound-interface
        // binding here too (no-op when none is installed).
        meow_common::apply_outbound_interface(&socket, domain)?;
        socket.set_nonblocking(true)?;

        match socket.connect(&dest.into()) {
            Ok(()) => {}
            Err(e) if e.raw_os_error() == Some(libc::EINPROGRESS) => {}
            Err(e) => return Err(e),
        }

        let std_stream: std::net::TcpStream = socket.into();
        return TcpStream::from_std(std_stream);
    }

    #[cfg(not(target_os = "linux"))]
    let _ = routing_mark;

    meow_common::connect_tcp(dest).await
}

#[async_trait]
impl ProxyAdapter for DirectAdapter {
    fn name(&self) -> &str {
        match self.adapter_type {
            AdapterType::Compatible => "COMPATIBLE",
            _ => "DIRECT",
        }
    }

    fn adapter_type(&self) -> AdapterType {
        self.adapter_type
    }

    fn addr(&self) -> &str {
        ""
    }

    fn support_udp(&self) -> bool {
        true
    }

    async fn dial_tcp(&self, metadata: &Metadata) -> Result<Box<dyn ProxyConn>> {
        let dests = self.resolve_targets(metadata).await?;
        let mut last_err = None;

        for dest in dests {
            match apply_connect_timeout(
                connect_with_mark(dest, self.routing_mark),
                self.connect_timeout,
                dest,
            )
            .await
            {
                Ok(stream) => return Ok(Box::new(DirectConn(stream))),
                // An errno-backed connect failure (e.g. EMFILE on
                // socket()) outranks a later context-only error — the
                // errno is what dead-mark classification reads (issue
                // #668); this error also escapes through relay hop-0 /
                // dialer-proxy front boundaries.
                Err(err) => last_err = MeowError::prefer_errno(last_err, err),
            }
        }

        Err(last_err.unwrap_or_else(|| MeowError::Proxy("direct: no reachable address".into())))
    }

    async fn dial_udp(&self, metadata: &Metadata) -> Result<Box<dyn ProxyPacketConn>> {
        // Bind the reply socket in the destination's address family. Hardcoding
        // an IPv4 (`0.0.0.0:0`) bind here broke QUIC/HTTP3 direct connections:
        // HTTP/3 origins are almost always dual-stack and the client prefers
        // IPv6 (Happy Eyeballs), so `send_to()` to a v6 destination failed on
        // the AF_INET socket. `handle_udp`'s initial write then errored and the
        // NAT session was never inserted, so no reply reader could ever form —
        // server→app QUIC replies had no socket to arrive on.
        //
        // A chained `UdpTarget::Name` reaching a `direct` front arrives as
        // host-only metadata (issue #657): direct is the terminal hop —
        // there is no further resolver view to delegate to, so resolve
        // through this adapter's configured resolver (the front's own view)
        // and *connect* the socket. The conn is bound: the caller holds no
        // literal for a name target and its write arg is only an advisory
        // placeholder.
        if metadata.domain_udp_target().is_some() {
            let mut last_err = None;
            for addr in self.resolve_targets(metadata).await? {
                let bind: SocketAddr = if addr.is_ipv4() {
                    "0.0.0.0:0".parse().expect("static")
                } else {
                    "[::]:0".parse().expect("static")
                };
                let socket = match meow_common::bind_udp(bind).await {
                    Ok(s) => s,
                    Err(e) => {
                        last_err = MeowError::prefer_errno_io(last_err, e);
                        continue;
                    }
                };
                match socket.connect(addr).await {
                    Ok(()) => {
                        return Ok(Box::new(DirectPacketConn {
                            socket,
                            bound: Some(addr),
                        }));
                    }
                    Err(e) => last_err = MeowError::prefer_errno_io(last_err, e),
                }
            }
            return Err(MeowError::Io(last_err.unwrap_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!(
                        "direct udp: no candidates for {}",
                        metadata.remote_address()
                    ),
                )
            })));
        }

        // The NAT key in `handle_udp` is `(src, dst)`, so a single direct UDP
        // session only ever targets one destination → one address family; we
        // bind the matching family up front. Falls back to IPv4 when the
        // destination family is unknown (preserves the legacy behaviour).
        let dst_is_v6 = match metadata.dst_ip {
            Some(ip) => ip.is_ipv6(),
            None => meow_common::metadata_ip_literal(&metadata.host).is_some_and(|ip| ip.is_ipv6()),
        };
        let bind_addr = if dst_is_v6 { "[::]:0" } else { "0.0.0.0:0" };
        let socket = meow_common::bind_udp(bind_addr)
            .await
            .map_err(MeowError::Io)?;
        Ok(Box::new(DirectPacketConn {
            socket,
            bound: None,
        }))
    }

    /// Pass the stream through unchanged.
    ///
    /// A direct hop in a relay chain is a no-op — useful for
    /// `relay: [direct, ss-node]` topologies where the first hop is a
    /// plain TCP connection without any proxy framing.
    ///
    /// upstream: adapter/outbound/direct.go — no DialContextWithDialer defined;
    /// relay skips direct hops by convention.  Class A ADR-0002: we make it
    /// explicit so the compiler enforces the override.
    async fn connect_over(
        &self,
        stream: Box<dyn ProxyConn>,
        _metadata: &Metadata,
    ) -> Result<Box<dyn ProxyConn>> {
        Ok(stream)
    }

    fn health(&self) -> &ProxyHealth {
        &self.health
    }
}

#[cfg(test)]
#[path = "test/direct_test.rs"]
mod tests;
