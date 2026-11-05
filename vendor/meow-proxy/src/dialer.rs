//! Pluggable TCP dialer for proxy chaining (mihomo `dialer-proxy` model).
//!
//! Each proxy adapter holds an `Arc<dyn TcpDialer>` and calls `dial()` to
//! obtain the raw underlying stream to its server.  The default
//! [`DirectDialer`] uses `meow_common::connect_tcp_host` (resolver-aware,
//! SocketProtector-aware).  When `dialer-proxy` is configured, a
//! [`ProxyDialer`] is injected instead — it tunnels through another proxy,
//! making chaining transparent to the adapter's TLS + protocol handshake.
//!
//! upstream: mihomo `component/proxydialer` + `BasicOption.NewDialer`.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Weak};

use parking_lot::RwLock;

use async_trait::async_trait;
use meow_common::{ConnType, MeowError, Metadata, Network, Proxy, ProxyConn, ProxyPacketConn};
use meow_transport::Stream;
use smol_str::SmolStr;

/// UDP association target for [`TcpDialer::dial_udp_conn`].
///
/// `Addr` is a literal endpoint, bound by whichever side dials. `Name`
/// asks the *front* of a `dialer-proxy` chain to resolve `host` with its
/// own resolver view — on the wire, where the front's protocol carries
/// domains — so the UDP association lands on the same server the front
/// picked for the chained TCP/control leg (issue #657: a local resolution
/// can pick a different backend under split-horizon or GeoDNS, and the
/// association then blackholes replies while failing closed).
///
/// A front that cannot carry a domain target refuses with
/// `ErrorKind::Unsupported`; the caller then resolves `host` itself and
/// redials with [`UdpTarget::Addr`] — today's behavior, preserved as the
/// fallback. Conns returned for `Name` are *name-bound*: their
/// `write_packet` ignores the addr arg and they may report read sources
/// that differ from any local resolution of `host`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UdpTarget {
    /// A literal endpoint.
    Addr(SocketAddr),
    /// A domain endpoint the front should resolve.
    Name { host: SmolStr, port: u16 },
}

impl UdpTarget {
    /// Build a target from a host string: IP literals collapse to
    /// [`UdpTarget::Addr`] (nothing to delegate — the stricter source
    /// filter stays available), anything else becomes [`UdpTarget::Name`].
    /// Bracketed IPv6 literals (`[::1]`, the usual `host:port` display
    /// form) unwrap first — otherwise they would masquerade as a domain
    /// and be delegated for resolution (issue #665). Shares the
    /// [`metadata_ip_literal`](meow_common::metadata_ip_literal) fold the
    /// inbound demux loops use (issue #648); `Name` keeps the caller's
    /// string verbatim so a non-IP bracketed input stays unresolvable
    /// rather than silently becoming a different name.
    pub fn named(host: &str, port: u16) -> Self {
        match meow_common::metadata_ip_literal(host) {
            Some(ip) => UdpTarget::Addr(SocketAddr::new(ip, port)),
            None => UdpTarget::Name {
                host: SmolStr::new(host),
                port,
            },
        }
    }

    /// The addr callers hand to `ProxyPacketConn::write_packet`.
    ///
    /// Bound conns (VLESS, mux, name-bound fronts) ignore the arg, so for
    /// `Name` — where the caller holds no literal — this returns an
    /// unspecified placeholder carrying only the port. Per-packet conns
    /// are only ever returned for [`UdpTarget::Addr`] (or a caller-side
    /// fallback to it), where the caller does hold the literal.
    pub fn write_dst(&self) -> SocketAddr {
        match self {
            UdpTarget::Addr(addr) => *addr,
            UdpTarget::Name { port, .. } => {
                SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), *port)
            }
        }
    }

    /// The association port — for `Name` targets the only locally-known
    /// half of the wire identity.
    pub fn port(&self) -> u16 {
        match self {
            UdpTarget::Addr(addr) => addr.port(),
            UdpTarget::Name { port, .. } => *port,
        }
    }

    /// Whether `src` is a plausible responder for this target.
    ///
    /// `Addr`: full canonical IP+port match (IPv4-mapped IPv6 forms
    /// collapse). `Name`: the front resolved the name — the wire source
    /// *should* differ from local resolution whenever views diverge (that
    /// is the point of the fix), so only the port is checkable and a
    /// foreign datagram must at least spoof the right port.
    pub fn src_matches(&self, src: SocketAddr) -> bool {
        match self {
            UdpTarget::Addr(addr) => {
                src.ip().to_canonical() == addr.ip().to_canonical() && src.port() == addr.port()
            }
            UdpTarget::Name { port, .. } => src.port() == *port,
        }
    }
}

impl std::fmt::Display for UdpTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UdpTarget::Addr(addr) => write!(f, "{addr}"),
            UdpTarget::Name { host, port } => write!(f, "{host}:{port}"),
        }
    }
}

/// A pluggable dialer for the underlying connection to a proxy server.
///
/// Mirrors mihomo's `C.Dialer` interface.  Adapters call `dial()` instead of
/// `meow_common::connect_tcp_host()` directly so that `dialer-proxy` can
/// inject a proxied connection transparently.
///
/// # Performance note (review M10)
///
/// Every outbound connection pays two extra heap allocations versus the old
/// direct `connect_tcp_host` path: this trait is `#[async_trait]` (the
/// future is boxed) and it returns `Box<dyn Stream>`.  The direct path
/// previously allocated nothing.  This is accepted for now — ADR-0008's
/// allocation discipline tracks per-connection overhead via the conn-rate
/// benchmark (`cargo run -p meow-bench -- --only connrate`); if that
/// benchmark regresses, the candidate fix is replacing `#[async_trait]`
/// with RPITIT (`impl Future` in the trait) and/or an unboxed stream
/// return, at the cost of the vtable-style plugin seam.
#[async_trait]
pub trait TcpDialer: Send + Sync {
    /// Dial `host:port` and return a duplex stream.
    ///
    /// `internal` is [`Metadata::is_internal`] from the caller's metadata —
    /// `true` for housekeeping traffic (health probes, provider fetches).
    /// [`ProxyDialer`] copies it onto the reconstructed metadata so a lazy
    /// front-hop group does not count internal chained dials as user
    /// traffic; dialers with no metadata to construct ignore it.
    async fn dial(&self, host: &str, port: u16, internal: bool) -> io::Result<Box<dyn Stream>>;

    /// Dial an already-resolved [`SocketAddr`].
    ///
    /// Callers holding a literal address should prefer this over
    /// `dial(&addr.ip().to_string(), addr.port())`, which allocates a `String`
    /// only for the callee to parse it straight back into an `IpAddr`.
    /// The default implementation does exactly that round-trip, so
    /// implementors that can dial an address directly should override it.
    async fn dial_addr(&self, addr: SocketAddr, internal: bool) -> io::Result<Box<dyn Stream>> {
        self.dial(&addr.ip().to_string(), addr.port(), internal)
            .await
    }

    /// Whether this dialer tunnels through another proxy (vs. direct).
    fn is_proxy(&self) -> bool {
        false
    }

    /// UDP association to `remote` — mihomo's `dialer.ListenPacket`.
    ///
    /// A direct dialer binds a real socket; a proxy dialer asks its front
    /// hop for a UDP relay association to `remote`, so the caller's
    /// datagrams ride the `dialer-proxy` chain instead of leaking the real
    /// source path. `remote` is a [`UdpTarget`]: `Addr` is literal,
    /// `Name` asks the front to resolve the destination itself (issue
    /// #657 — the front's view is the one its TCP/control leg used). The
    /// returned conn is *bound* to `remote`: `write_packet`'s addr is
    /// advisory — bound-at-dial conns (VLESS, mux, name-bound fronts)
    /// ignore it; per-packet conns stamp it, so callers hand over
    /// [`UdpTarget::write_dst`]. `read_packet`'s reported source follows
    /// [`UdpTarget::src_matches`].
    ///
    /// `internal` mirrors `dial()` — housekeeping traffic marks it so a
    /// lazy front-hop group does not count the chained dial as use.
    ///
    /// `Arc` (not `Box`) because the kcptun transport wraps the conn in a
    /// `PacketConnSocket` whose pump tasks each hold a reference.
    ///
    /// The default errors — implementations without UDP cannot carry it.
    async fn dial_udp_conn(
        &self,
        _remote: UdpTarget,
        _internal: bool,
    ) -> io::Result<Arc<dyn ProxyPacketConn>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "dialer cannot provide a UDP endpoint",
        ))
    }

    /// Whether [`dial_udp_conn`](Self::dial_udp_conn) can succeed — the
    /// dialer-layer counterpart of `ProxyAdapter::support_udp`.
    ///
    /// A by-name proxy dialer answers with the resolved front's snapshot.
    /// That answer is provisional when the front is a group (the member
    /// live at dial time decides), so callers must still fail closed on a
    /// `dial_udp_conn` error rather than trusting this flag.
    fn supports_udp(&self) -> bool {
        false
    }

    /// Connected UDP datagram endpoint for transports layered over UDP
    /// (kcptun). Equivalent to mihomo's `dialer.ListenPacket`: the direct
    /// dialer binds a real socket; a proxy dialer tunnels datagrams through
    /// the front proxy's UDP relay instead of leaking the real source path.
    ///
    /// The default errors — implementations without UDP simply cannot carry
    /// a UDP transport.
    #[cfg(feature = "kcptun")]
    async fn dial_udp_endpoint(
        &self,
        _remote: SocketAddr,
    ) -> io::Result<Box<dyn meow_transport::kcptun::SocketIo>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "dialer cannot provide a UDP endpoint",
        ))
    }
}

/// Direct TCP dialer — the default, equivalent to mihomo's `dialer.NewDialer()`.
///
/// Uses `meow_common::connect_tcp_host` which is resolver-aware and
/// SocketProtector-aware (Android `VpnService.protect(fd)` etc.).
pub struct DirectDialer;

#[async_trait]
impl TcpDialer for DirectDialer {
    async fn dial(&self, host: &str, port: u16, _internal: bool) -> io::Result<Box<dyn Stream>> {
        let tcp = meow_common::connect_tcp_host(host, port).await?;
        // Preserve TCP_NODELAY (disable Nagle) — all call sites that
        // previously called `tcp.set_nodelay(true)` on the raw
        // `TcpStream` now rely on the dialer to do it once here.
        let _ = tcp.set_nodelay(true);
        Ok(Box::new(tcp))
    }

    async fn dial_addr(&self, addr: SocketAddr, _internal: bool) -> io::Result<Box<dyn Stream>> {
        // Skip the default's `to_string()` + re-parse: `connect_tcp` takes the
        // `SocketAddr` as-is and keeps the SocketProtector hook.
        let tcp = meow_common::connect_tcp(addr).await?;
        let _ = tcp.set_nodelay(true);
        Ok(Box::new(tcp))
    }

    async fn dial_udp_conn(
        &self,
        remote: UdpTarget,
        _internal: bool,
    ) -> io::Result<Arc<dyn ProxyPacketConn>> {
        // A `Name` target has no remote to delegate resolution to — the
        // local resolver is the only view a direct dial can take. Resolve
        // here and fall into the literal path per candidate.
        let candidates = match &remote {
            UdpTarget::Addr(addr) => vec![*addr],
            UdpTarget::Name { host, port } => meow_common::resolve_host_all(host, *port).await?,
        };
        // Try candidates in resolver order: on a single-stack network the
        // resolver can order an unreachable family first and a UDP
        // connect() fails immediately with ENETUNREACH — falling through
        // keeps the association alive like the SS raw-socket arm does.
        let mut last_err: Option<io::Error> = None;
        for remote in candidates {
            // Same bind-family + protect-hook dance as the SS UDP relay
            // path: `bind_udp` routes the fd through the installed
            // SocketProtector (Android VpnService.protect) before connect.
            let bind_addr: SocketAddr = if remote.is_ipv4() {
                "0.0.0.0:0".parse().expect("static")
            } else {
                "[::]:0".parse().expect("static")
            };
            let udp = match meow_common::bind_udp(bind_addr).await {
                Ok(udp) => udp,
                Err(e) => {
                    // An errno-backed failure (e.g. EMFILE on socket())
                    // outranks a later errno-less error — it carries the
                    // local-vs-member classification (issue #668).
                    last_err = MeowError::prefer_errno_io(last_err, e);
                    continue;
                }
            };
            match udp.connect(remote).await {
                Ok(()) => {
                    return Ok(Arc::new(ConnectedUdpConn {
                        socket: udp,
                        remote,
                    }));
                }
                Err(e) => last_err = MeowError::prefer_errno_io(last_err, e),
            }
        }
        Err(last_err
            .unwrap_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no UDP dial candidates")))
    }

    fn supports_udp(&self) -> bool {
        true
    }

    #[cfg(feature = "kcptun")]
    async fn dial_udp_endpoint(
        &self,
        remote: SocketAddr,
    ) -> io::Result<Box<dyn meow_transport::kcptun::SocketIo>> {
        // Not shared with `dial_udp_conn` above: kcptun needs the raw
        // `UdpSocket` for `SocketIo::apply_socket_options` (sockbuf/DSCP),
        // which `ProxyPacketConn` does not expose.
        let bind_addr: SocketAddr = if remote.is_ipv4() {
            "0.0.0.0:0".parse().expect("static")
        } else {
            "[::]:0".parse().expect("static")
        };
        let udp = meow_common::bind_udp(bind_addr).await?;
        udp.connect(remote).await?;
        Ok(Box::new(udp))
    }
}

/// `ProxyPacketConn` over a connected raw UDP socket — the direct arm of
/// [`TcpDialer::dial_udp_conn`]. Datagrams always go to `remote`;
/// `write_packet`'s addr is ignored, matching the bound-destination
/// semantics a front proxy applies to a chained association.
struct ConnectedUdpConn {
    socket: tokio::net::UdpSocket,
    remote: SocketAddr,
}

#[async_trait]
impl ProxyPacketConn for ConnectedUdpConn {
    async fn read_packet(&self, buf: &mut [u8]) -> meow_common::Result<(usize, SocketAddr)> {
        let n = self.socket.recv(buf).await.map_err(MeowError::Io)?;
        Ok((n, self.remote))
    }

    async fn write_packet(&self, buf: &[u8], _addr: &SocketAddr) -> meow_common::Result<usize> {
        self.socket.send(buf).await.map_err(MeowError::Io)
    }

    fn local_addr(&self) -> meow_common::Result<SocketAddr> {
        self.socket.local_addr().map_err(MeowError::Io)
    }

    /// Contract no-op like `Socks5UdpConn::close`: a connected socket has no
    /// association state to release — it dies on the last `Arc` drop and a
    /// parked `recv` is not interrupted.
    fn close(&self) -> meow_common::Result<()> {
        Ok(())
    }
}

/// Proxy dialer — tunnels through another proxy.  Equivalent to mihomo's
/// `proxyDialer.DialContext()`.
///
/// Calls `proxy.dial_tcp()` with metadata targeting `host:port`, then adapts
/// the returned `ProxyConn` into a `Stream` for the caller's transport chain.
///
/// This is the *unguarded* inner of the by-name path: it carries no
/// `scoped_chain_dial` depth accounting, so nothing should be built on it
/// that can re-enter by-name resolution — use [`NamedProxyDialer`] for a
/// `dialer-proxy` chain edge (issue #489).
pub struct ProxyDialer {
    proxy: Arc<dyn Proxy>,
}

impl ProxyDialer {
    pub fn new(proxy: Arc<dyn Proxy>) -> Self {
        Self { proxy }
    }

    /// Dial the front proxy with a fully-formed [`Metadata`] target.
    async fn dial_metadata(&self, meta: Metadata) -> io::Result<Box<dyn Stream>> {
        let conn = self
            .proxy
            .dial_tcp(&meta)
            .await
            .map_err(|e| e.into_io_error("dialer-proxy"))?;
        // `Box<dyn ProxyConn>` is unsized (!Sized), so it cannot satisfy
        // the `Any` bound required by the blanket `Stream` impl.  `ConnStream`
        // is a sized newtype that forwards `AsyncRead`/`AsyncWrite` through
        // the boxed conn, bridging `ProxyConn` → `Stream`.
        Ok(Box::new(ConnStream(conn)))
    }
}

#[cfg(feature = "kcptun")]
mod packet_conn_socket {
    //! `SocketIo` over a front proxy's UDP relay — the `dialer-proxy`
    //! counterpart of `DirectDialer`'s raw `UdpSocket`. `ProxyPacketConn`
    //! is async rather than poll-based, so two pump tasks bridge it to
    //! bounded channels; the poll side then carries ordinary channel
    //! backpressure semantics. Both tasks exit — and the association
    //! closes — when the socket drops.

    use std::io;
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};

    use meow_transport::kcptun::SocketIo;
    use tokio::io::ReadBuf;
    use tokio::sync::mpsc;
    use tokio_util::sync::PollSender;

    /// Datagram queue depth either way — sized like a socket buffer, not a
    /// stream queue: a full queue drops a datagram and KCP retransmits.
    const QUEUE: usize = 256;

    pub struct PacketConnSocket {
        /// `Mutex` only because `Receiver`/`PollSender` are `!Sync`;
        /// `&mut` poll methods go through `get_mut`, never the lock.
        inbound: Mutex<mpsc::Receiver<Vec<u8>>>,
        outbound: Mutex<PollSender<Vec<u8>>>,
        /// Aborted on drop: the read task parks inside `read_packet` and
        /// would otherwise outlive the socket, pinning the front-proxy UDP
        /// association open across KCP session churn. The write pump exits
        /// on its own once the `PollSender` side closes — but only after
        /// the in-flight `write_packet` completes, so a wedged front-proxy
        /// conn would pin it indefinitely; abort that one too.
        read_task: tokio::task::JoinHandle<()>,
        write_task: tokio::task::JoinHandle<()>,
    }

    impl Drop for PacketConnSocket {
        fn drop(&mut self) {
            self.read_task.abort();
            self.write_task.abort();
        }
    }

    impl PacketConnSocket {
        pub fn new(conn: Arc<dyn meow_common::ProxyPacketConn>, remote: SocketAddr) -> Self {
            let (in_tx, inbound) = mpsc::channel(QUEUE);
            let (outbound, mut out_rx) = mpsc::channel::<Vec<u8>>(QUEUE);

            let reader = Arc::clone(&conn);
            let read_task = tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                // try_send drops on a full queue: KCP retransmits, same as
                // a full kernel socket buffer upstream.
                while let Ok((n, _src)) = reader.read_packet(&mut buf).await {
                    let _ = in_tx.try_send(buf[..n].to_vec());
                }
                let _ = reader.close();
            });
            let write_task = tokio::spawn(async move {
                while let Some(pkt) = out_rx.recv().await {
                    if conn.write_packet(&pkt, &remote).await.is_err() {
                        break;
                    }
                }
                let _ = conn.close();
            });

            Self {
                inbound: Mutex::new(inbound),
                outbound: Mutex::new(PollSender::new(outbound)),
                read_task,
                write_task,
            }
        }
    }

    impl SocketIo for PacketConnSocket {
        fn poll_send(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
            let sender = self.outbound.get_mut().unwrap();
            match sender.poll_reserve(cx) {
                // `send_item` after a successful reserve cannot fail to
                // enqueue — the permit is already held.
                Poll::Ready(Ok(())) => match sender.send_item(buf.to_vec()) {
                    Ok(()) => Poll::Ready(Ok(buf.len())),
                    Err(_) => Poll::Ready(Err(closed())),
                },
                Poll::Ready(Err(_)) => Poll::Ready(Err(closed())),
                Poll::Pending => Poll::Pending,
            }
        }

        fn poll_recv(
            &mut self,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            match self.inbound.get_mut().unwrap().poll_recv(cx) {
                // Datagram semantics: an oversized packet truncates.
                Poll::Ready(Some(pkt)) => {
                    let n = pkt.len().min(buf.remaining());
                    buf.put_slice(&pkt[..n]);
                    Poll::Ready(Ok(()))
                }
                // The read pump exited — the association is gone.
                Poll::Ready(None) => Poll::Ready(Err(closed())),
                Poll::Pending => Poll::Pending,
            }
        }
    }

    fn closed() -> io::Error {
        io::Error::new(io::ErrorKind::BrokenPipe, "udp endpoint closed")
    }
}

#[cfg(feature = "kcptun")]
use packet_conn_socket::PacketConnSocket;

/// Build the `Metadata` for a chained front-hop dial by `host`/`port`.
///
/// An IP-literal `host` becomes a typed `dst_ip` so the front proxy encodes
/// an IP address rather than a domain name that happens to look like one.
/// `conn_type` is explicit `Inner`: `Metadata::default()` would yield
/// `ConnType::Http` (the first enum variant), but this is an internal
/// chained-relay dial — the front proxy's rules, /connections list, and
/// stats must not classify it as an HTTP inbound.
///
/// `internal` carries the caller's usage-accounting marker through the
/// reconstruction — otherwise a probe or fetch chained through
/// `dialer-proxy` would reach a lazy front-hop group as ordinary `Inner`
/// traffic and be counted as use, keeping the group's probe loop awake
/// forever (issue #555).
fn inner_dial_metadata(host: &str, port: u16, internal: bool) -> Metadata {
    // Bracketed IPv6 literals (`server: "[::1]"`) fold to the typed
    // address too — a bare `parse` would leave them in `host`, where the
    // front hop treats them as a domain that cannot resolve (issue #701).
    match meow_common::metadata_ip_literal(host) {
        Some(ip) => Metadata {
            network: Network::Tcp,
            conn_type: ConnType::Inner,
            dst_ip: Some(ip),
            dst_port: port,
            internal,
            ..Default::default()
        },
        None => Metadata {
            network: Network::Tcp,
            conn_type: ConnType::Inner,
            host: host.into(),
            dst_port: port,
            internal,
            ..Default::default()
        },
    }
}

/// Same as [`inner_dial_metadata`] for a typed `SocketAddr`: carries the
/// literal in `dst_ip` rather than rendering it into `host`, so adapters
/// emit an IP-typed address — what mihomo does and what SOCKS5/Trojan/VLESS
/// address encoding expects.
fn inner_dial_metadata_at(addr: SocketAddr, internal: bool) -> Metadata {
    Metadata {
        network: Network::Tcp,
        conn_type: ConnType::Inner,
        dst_ip: Some(addr.ip()),
        dst_port: addr.port(),
        internal,
        ..Default::default()
    }
}

#[async_trait]
impl TcpDialer for ProxyDialer {
    async fn dial(&self, host: &str, port: u16, internal: bool) -> io::Result<Box<dyn Stream>> {
        self.dial_metadata(inner_dial_metadata(host, port, internal))
            .await
    }

    async fn dial_addr(&self, addr: SocketAddr, internal: bool) -> io::Result<Box<dyn Stream>> {
        self.dial_metadata(inner_dial_metadata_at(addr, internal))
            .await
    }

    fn is_proxy(&self) -> bool {
        true
    }

    async fn dial_udp_conn(
        &self,
        remote: UdpTarget,
        internal: bool,
    ) -> io::Result<Arc<dyn ProxyPacketConn>> {
        // `proxyDialer.ListenPacket` upstream: the datagram endpoint is the
        // front proxy's UDP relay association to `remote`. `ConnType::Inner`
        // like `dial()` — this is infrastructure traffic, not user inbound.
        //
        // A `Name` target populates `host` alone (`dst_ip` absent): the
        // front binds an association to the *name*, resolving with its own
        // resolver view — the front's server view, where the front's
        // protocol can carry the domain — so the UDP leg lands on the same
        // endpoint as the chained TCP/control leg (issue #657). Fronts
        // that cannot express a domain target refuse with `NotSupported`,
        // and the caller falls back to a local-resolution `Addr` dial.
        let meta = match &remote {
            UdpTarget::Addr(addr) => Metadata {
                network: Network::Udp,
                conn_type: ConnType::Inner,
                dst_ip: Some(addr.ip()),
                dst_port: addr.port(),
                internal,
                ..Default::default()
            },
            UdpTarget::Name { host, port } => {
                // Fold a hand-constructed `Name` carrying a bracketed
                // literal — `UdpTarget::named` already folds, but the
                // enum fields are public (#701).
                if let Some(ip) = meow_common::metadata_ip_literal(host) {
                    Metadata {
                        network: Network::Udp,
                        conn_type: ConnType::Inner,
                        dst_ip: Some(ip),
                        dst_port: *port,
                        internal,
                        ..Default::default()
                    }
                } else {
                    Metadata {
                        network: Network::Udp,
                        conn_type: ConnType::Inner,
                        host: host.clone(),
                        dst_port: *port,
                        internal,
                        ..Default::default()
                    }
                }
            }
        };
        // Capability errors keep their class across the `io::Error`
        // boundary so the adapter can reconstitute `NotSupported`, and
        // `MeowError::Io` chains pass their errno through — a UDP-less
        // front or local fd exhaustion must not cost the member its
        // health (issues #663/#668).
        let conn = self
            .proxy
            .dial_udp(&meta)
            .await
            .map_err(|e| e.into_io_error("dialer-proxy udp"))?;
        Ok(Arc::from(conn))
    }

    fn supports_udp(&self) -> bool {
        // Point-in-time snapshot: a group front reports its live
        // selection's capability, so `dial_udp_conn` remains the
        // authoritative gate. Guarded the same way `NamedProxyDialer` is —
        // every in-tree `ProxyDialer` is ephemeral under a named frame, but
        // `ProxyDialer::new` is `pub` and an embedder could inject one where
        // the front group resolves back to the caller's own adapter.
        let Some(_depth) = SupportsUdpDepth::enter() else {
            return false;
        };
        self.proxy.support_udp()
    }

    #[cfg(feature = "kcptun")]
    async fn dial_udp_endpoint(
        &self,
        remote: SocketAddr,
    ) -> io::Result<Box<dyn meow_transport::kcptun::SocketIo>> {
        // `internal` stays false by the same rule as mux session
        // establishment: the pooled kcptun session is the ONLY dial signal
        // a lazy front hop sees for this chain — per-stream `open_stream`
        // calls never re-dial while a session lives — so marking it
        // internal would leave the front hop permanently "unused" while
        // user traffic flows (issue #555 boundary).
        let conn = self.dial_udp_conn(UdpTarget::Addr(remote), false).await?;
        Ok(Box::new(PacketConnSocket::new(conn, remote)))
    }
}

/// An immutable snapshot of a finished config build, shared with every
/// `dialer-proxy` bound from it.
pub type ProxySnapshot = Arc<HashMap<SmolStr, Arc<dyn Proxy>>>;

/// Shared cell a [`ProxyRegistry`] publishes into and [`DialerTarget`]s
/// resolve through. The cell is the *generation anchor*: keeping it alive
/// keeps the published snapshot — and every adapter inside it — alive.
pub type RegistryCell = RwLock<Option<ProxySnapshot>>;

/// The proxies built from one config, published when the build completes and
/// consulted by name on every chained dial.
///
/// mihomo resolves `dialer-proxy` the same way (`component/proxydialer/byname.go`).
/// Capturing the front proxy as an `Arc` while the config is still being built
/// freezes a stale entry instead: proxy groups clone their members before the
/// dialer pass replaces them, and a group-valued dialer does not exist yet when
/// the outbound chaining through it is built (issue #513).
///
/// # Ownership (issue #533)
///
/// [`DialerTarget`] holds the cell **weakly** — a strong edge would close
/// `registry → snapshot → adapter → registry` into a reference cycle that
/// leaks every superseded route generation. Whoever retains adapters built
/// from a build must therefore also retain this handle for as long as those
/// adapters may dial: the tunnel keeps it inside `RouteTable`, and provider
/// fetch contexts keep a clone so a retained download adapter still resolves
/// its front hop after later rebuilds. When the last handle drops, chained
/// adapters of that generation fail closed at dial time.
#[derive(Clone, Default)]
pub struct ProxyRegistry {
    proxies: Arc<RegistryCell>,
}

impl ProxyRegistry {
    /// Publish the finished registry. Called once per config build, after every
    /// leaf proxy and group exists; a rebuild publishes into its own registry,
    /// so adapters from a previous config keep resolving their own snapshot.
    pub fn publish(&self, proxies: ProxySnapshot) {
        // Swap under the lock but drop the superseded snapshot — which runs
        // every adapter destructor — outside it.
        let old = self.proxies.write().replace(proxies);
        drop(old);
    }

    /// The weak edge [`DialerTarget`] stores. An upgrade succeeds only while
    /// some [`ProxyRegistry`] clone still owns the cell.
    pub fn downgrade(&self) -> Weak<RegistryCell> {
        Arc::downgrade(&self.proxies)
    }

    /// Resolve `name` against the currently published snapshot — `None` when
    /// no generation has been published yet or the name is absent. For
    /// one-shot lookups (provider `proxy:` / subscription fetches); adapters
    /// that dial repeatedly keep a [`DialerTarget`] instead.
    pub fn resolve_name(&self, name: &str) -> Option<Arc<dyn Proxy>> {
        self.proxies
            .read()
            .as_ref()
            .and_then(|map| map.get(name).cloned())
    }
}

impl std::fmt::Debug for ProxyRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The snapshot holds `Arc<dyn Proxy>` handles — not Debug — so report
        // only whether a generation is published, not its contents.
        f.debug_struct("ProxyRegistry")
            .field("published", &self.proxies.read().is_some())
            .finish()
    }
}

/// The front hop of a `dialer-proxy` chain, addressed by name.
#[derive(Clone)]
pub struct DialerTarget {
    name: SmolStr,
    registry: Weak<RegistryCell>,
}

impl DialerTarget {
    pub fn new(name: impl Into<SmolStr>, registry: &ProxyRegistry) -> Self {
        Self {
            name: name.into(),
            registry: registry.downgrade(),
        }
    }

    /// Registry key of the front proxy, as written in the config.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// `None` when the registry generation is gone, has not been published
    /// yet, or no longer holds the name. Callers must fail loudly — falling
    /// back to a direct dial would leak past a chain the user configured for
    /// policy reasons.
    pub fn resolve(&self) -> Option<Arc<dyn Proxy>> {
        self.registry.upgrade().and_then(|cell| {
            cell.read()
                .as_ref()
                .and_then(|map| map.get(&self.name).cloned())
        })
    }

    /// Error for an unresolvable target, worded for the two error types the
    /// chained dial paths report through. Distinguishes a name absent from a
    /// live registry (config names a nonexistent hop — a config bug) from a
    /// dropped generation (the route table that owned the cell was swapped —
    /// a reload crossed an in-flight dial, issue #533).
    pub fn missing_error(&self) -> String {
        if self.registry.upgrade().is_none() {
            format!(
                "dialer-proxy '{}': registry generation dropped (config reloaded mid-dial)",
                self.name
            )
        } else {
            format!("dialer-proxy '{}' is not in the proxy registry", self.name)
        }
    }
}

/// [`TcpDialer`] for a `dialer-proxy` front hop that is resolved by name at
/// dial time. Equivalent to mihomo's `proxydialer.NewByNameDialer`.
pub struct NamedProxyDialer {
    target: DialerTarget,
}

impl NamedProxyDialer {
    pub fn new(target: DialerTarget) -> Self {
        Self { target }
    }

    async fn dial_front(&self, meta: Metadata) -> io::Result<Box<dyn Stream>> {
        let front = self
            .target
            .resolve()
            .ok_or_else(|| io::Error::other(self.target.missing_error()))?;
        ProxyDialer::new(front).dial_metadata(meta).await
    }
}

tokio::task_local! {
    /// Depth of nested `dialer-proxy` chained dials on the current task.
    ///
    /// Static configs are cycle-checked at build time
    /// (`reject_group_membership_cycles`), but a *provider-sourced* node may
    /// also carry `dialer-proxy`, and group membership over provider slots is
    /// dynamic — `node → dialer G → G selects node` recurses in one task's
    /// poll chain and would exhaust the native stack. The counter lives on
    /// the task rather than in `Metadata` because every hop rebuilds the
    /// metadata it passes down (issue #489).
    static DIALER_CHAIN_DEPTH: usize;
}

/// Bound on nested `dialer-proxy` hops: the chain recurses inside one
/// task's poll chain, so an unbounded chain would exhaust the native
/// stack. 16 is far above any sane front-hop depth.
pub(crate) const MAX_DIALER_CHAIN_DEPTH: usize = 16;

std::thread_local! {
    /// Depth of nested `supports_udp` resolution on the current thread —
    /// the synchronous counterpart of `DIALER_CHAIN_DEPTH`. A dynamic
    /// `node → group → node` cycle recurses through `front.support_udp()`
    /// with no await point for [`scoped_chain_dial`] to bound, so the sync
    /// capability query needs its own limit.
    static SUPPORTS_UDP_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// RAII decrement for [`SUPPORTS_UDP_DEPTH`] — bounds `supports_udp`
/// recursion the way [`scoped_chain_dial`] bounds dial recursion.
struct SupportsUdpDepth;

impl SupportsUdpDepth {
    /// `None` once the chain exceeds [`MAX_DIALER_CHAIN_DEPTH`] (or during
    /// thread-local teardown) — treat as "capability unknown", which is
    /// fail-safe: a cyclic chain can't carry UDP anyway.
    fn enter() -> Option<Self> {
        SUPPORTS_UDP_DEPTH
            .try_with(|d| (d.get() < MAX_DIALER_CHAIN_DEPTH).then(|| d.set(d.get() + 1)))
            .ok()
            .flatten()
            .map(|()| Self)
    }
}

impl Drop for SupportsUdpDepth {
    fn drop(&mut self) {
        let _ = SUPPORTS_UDP_DEPTH.try_with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// Run `f` as one nested `dialer-proxy` hop; `over_limit` produces the
/// caller's error type when the chain exceeds [`MAX_DIALER_CHAIN_DEPTH`].
/// Every by-name dial entry point — [`NamedProxyDialer`]'s TCP and UDP
/// endpoint paths and `DialerProxyAdapter`'s relay path — funnels through
/// this so a cycle that only becomes reachable through dynamic group
/// membership degrades to a dial error instead of unbounded same-task
/// recursion.
///
/// Mux-enabled nodes are the one shape that never reaches this guard: the
/// nested dial pends on the session mutex the outer frame holds and
/// surfaces as a session-setup timeout instead — bounded, but diagnosed as
/// a timeout rather than a cycle.
pub(crate) async fn scoped_chain_dial<F, T, E>(
    target_name: &str,
    over_limit: impl FnOnce(&str) -> E,
    f: F,
) -> Result<T, E>
where
    F: std::future::Future<Output = Result<T, E>>,
{
    let depth = DIALER_CHAIN_DEPTH.try_with(|d| *d).unwrap_or(0);
    if depth >= MAX_DIALER_CHAIN_DEPTH {
        return Err(over_limit(target_name));
    }
    DIALER_CHAIN_DEPTH.scope(depth + 1, f).await
}

impl NamedProxyDialer {
    async fn dial_inner(&self, meta: Metadata) -> io::Result<Box<dyn Stream>> {
        scoped_chain_dial(
            self.target.name(),
            |name| {
                io::Error::other(format!(
                    "dialer-proxy '{name}': chain exceeds {MAX_DIALER_CHAIN_DEPTH} hops; \
                     a provider member or group is routing the dial back into itself"
                ))
            },
            self.dial_front(meta),
        )
        .await
    }
}

#[async_trait]
impl TcpDialer for NamedProxyDialer {
    async fn dial(&self, host: &str, port: u16, internal: bool) -> io::Result<Box<dyn Stream>> {
        self.dial_inner(inner_dial_metadata(host, port, internal))
            .await
    }

    async fn dial_addr(&self, addr: SocketAddr, internal: bool) -> io::Result<Box<dyn Stream>> {
        self.dial_inner(inner_dial_metadata_at(addr, internal))
            .await
    }

    fn is_proxy(&self) -> bool {
        true
    }

    async fn dial_udp_conn(
        &self,
        remote: UdpTarget,
        internal: bool,
    ) -> io::Result<Arc<dyn ProxyPacketConn>> {
        // Same guard as `dial_inner`: a UDP-capable node's association
        // setup calls this through the injected dialer, so
        // `node → group → node` cycles recurse here without ever touching
        // `dial`/`dial_addr`.
        scoped_chain_dial(
            self.target.name(),
            |name| {
                io::Error::other(format!(
                    "dialer-proxy '{name}': chain exceeds {MAX_DIALER_CHAIN_DEPTH} hops; \
                     a provider member or group is routing the dial back into itself"
                ))
            },
            async {
                let front = self
                    .target
                    .resolve()
                    .ok_or_else(|| io::Error::other(self.target.missing_error()))?;
                ProxyDialer::new(front)
                    .dial_udp_conn(remote, internal)
                    .await
            },
        )
        .await
    }

    fn supports_udp(&self) -> bool {
        // Depth-bounded like the dial path: a provider-sourced dynamic
        // cycle (`node → group → node`) would otherwise recurse through
        // `front.support_udp()` until the native stack exhausts. The guard
        // lives in `ProxyDialer::supports_udp` so exactly one depth slot is
        // consumed per named hop — and direct `ProxyDialer` injection stays
        // covered too. `false` on overflow is fail-safe: the dial would
        // fail the same guard.
        self.target
            .resolve()
            .is_some_and(|front| ProxyDialer::new(front).supports_udp())
    }

    #[cfg(feature = "kcptun")]
    async fn dial_udp_endpoint(
        &self,
        remote: SocketAddr,
    ) -> io::Result<Box<dyn meow_transport::kcptun::SocketIo>> {
        // Same guard as `dial_inner`: a kcptun node's session setup calls
        // this through the injected dialer, so `node → group → node` cycles
        // recurse here without ever touching `dial`/`dial_addr`.
        scoped_chain_dial(
            self.target.name(),
            |name| {
                io::Error::other(format!(
                    "dialer-proxy '{name}': chain exceeds {MAX_DIALER_CHAIN_DEPTH} hops; \
                     a provider member or group is routing the dial back into itself"
                ))
            },
            async {
                let front = self
                    .target
                    .resolve()
                    .ok_or_else(|| io::Error::other(self.target.missing_error()))?;
                ProxyDialer::new(front).dial_udp_endpoint(remote).await
            },
        )
        .await
    }
}

/// Wrap a `Box<dyn ProxyConn>` as a `meow_transport::Stream`.
///
/// `Stream` requires `Sized + Any`; `Box<dyn ProxyConn>` is `!Sized`, so it
/// cannot use the blanket `Stream` impl.  This newtype forwards
/// `AsyncRead`/`AsyncWrite` through the boxed conn.
///
/// Downcast-based optimizations are unaffected: the transport layers wrap
/// whatever they are handed in their own concrete type (e.g.
/// `RealityTlsStream` holds its inner stream as a `Box<dyn Stream>`), so
/// `as_any_mut()` still sees that outer type and the Reality raw-passthrough
/// shortcut fires the same whether the bottom of the stack is a `TcpStream` or
/// a `ConnStream`.
pub struct ConnStream(pub Box<dyn ProxyConn>);

impl tokio::io::AsyncRead for ConnStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for ConnStream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        std::pin::Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

impl Unpin for ConnStream {}

#[cfg(test)]
#[path = "test/dialer_test.rs"]
mod tests;
