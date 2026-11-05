use std::net::{IpAddr, Ipv4Addr};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::*;
use meow_common::MeowError;

/// "command not supported" — only the refusal test needs it.
const REPLY_COMMAND_NOT_SUPPORTED: u8 = 0x07;

// ─── Mock SOCKS5 server ───────────────────────────────────────────────────

enum AuthMode {
    NoAuth,
    UserPass {
        user: &'static str,
        pass: &'static str,
    },
    ForceNoAuth, // advertise only 0x00 even when client offers 0x02
    NoAcceptable,
}

enum ConnectResult {
    Success,
    Fail(u8),
}

struct MockServer {
    auth_mode: AuthMode,
    connect_result: ConnectResult,
}

impl MockServer {
    fn new_no_auth() -> Self {
        Self {
            auth_mode: AuthMode::NoAuth,
            connect_result: ConnectResult::Success,
        }
    }
    fn new_user_pass(user: &'static str, pass: &'static str) -> Self {
        Self {
            auth_mode: AuthMode::UserPass { user, pass },
            connect_result: ConnectResult::Success,
        }
    }
    fn new_no_acceptable() -> Self {
        Self {
            auth_mode: AuthMode::NoAcceptable,
            connect_result: ConnectResult::Success,
        }
    }
    fn new_force_no_auth() -> Self {
        Self {
            auth_mode: AuthMode::ForceNoAuth,
            connect_result: ConnectResult::Success,
        }
    }
    fn with_connect_fail(mut self, rep: u8) -> Self {
        self.connect_result = ConnectResult::Fail(rep);
        self
    }

    async fn spawn(self) -> (std::net::SocketAddr, tokio::task::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let handle = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut captured_req = Vec::new();

            // Method negotiation
            let mut hdr = [0u8; 2];
            s.read_exact(&mut hdr).await.unwrap();
            let n_methods = hdr[1] as usize;
            let mut methods = vec![0u8; n_methods];
            s.read_exact(&mut methods).await.unwrap();
            captured_req.extend_from_slice(&hdr);
            captured_req.extend_from_slice(&methods);

            let chosen = match &self.auth_mode {
                AuthMode::NoAuth | AuthMode::ForceNoAuth => METHOD_NO_AUTH,
                AuthMode::UserPass { .. } => METHOD_USER_PASS,
                AuthMode::NoAcceptable => METHOD_NO_ACCEPTABLE,
            };
            s.write_all(&[VERSION, chosen]).await.unwrap();

            if chosen == METHOD_NO_ACCEPTABLE {
                return captured_req;
            }

            // Sub-negotiation (if chosen = 0x02)
            if chosen == METHOD_USER_PASS {
                let mut auth_hdr = [0u8; 2];
                s.read_exact(&mut auth_hdr).await.unwrap();
                let ulen = auth_hdr[1] as usize;
                let mut user_bytes = vec![0u8; ulen];
                s.read_exact(&mut user_bytes).await.unwrap();
                let mut plen_buf = [0u8; 1];
                s.read_exact(&mut plen_buf).await.unwrap();
                let plen = plen_buf[0] as usize;
                let mut pass_bytes = vec![0u8; plen];
                s.read_exact(&mut pass_bytes).await.unwrap();

                let ok = match &self.auth_mode {
                    AuthMode::UserPass { user, pass } => {
                        user_bytes == user.as_bytes() && pass_bytes == pass.as_bytes()
                    }
                    _ => false,
                };
                let status = if ok { AUTH_SUCCESS } else { 0x01u8 };
                s.write_all(&[AUTH_VERSION, status]).await.unwrap();
                if !ok {
                    return captured_req;
                }
            }

            // CONNECT request
            let mut req_hdr = [0u8; 4];
            s.read_exact(&mut req_hdr).await.unwrap();
            captured_req.extend_from_slice(&req_hdr);
            let atyp = req_hdr[3];
            let addr_len = match atyp {
                ATYP_IPV4 => 4,
                ATYP_DOMAIN => {
                    let mut l = [0u8; 1];
                    s.read_exact(&mut l).await.unwrap();
                    captured_req.push(l[0]);
                    l[0] as usize
                }
                ATYP_IPV6 => 16,
                _ => 0,
            };
            let mut addr_port = vec![0u8; addr_len + 2];
            s.read_exact(&mut addr_port).await.unwrap();
            captured_req.extend_from_slice(&addr_port);

            // Reply
            let rep = match &self.connect_result {
                ConnectResult::Success => REPLY_SUCCESS,
                ConnectResult::Fail(r) => *r,
            };
            // Bound addr: IPv4 0.0.0.0:0
            s.write_all(&[VERSION, rep, RESERVED, ATYP_IPV4, 0, 0, 0, 0, 0, 0])
                .await
                .unwrap();

            if rep == REPLY_SUCCESS {
                // Echo payload
                let mut buf = [0u8; 256];
                loop {
                    match s.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let _ = s.write_all(&buf[..n]).await;
                        }
                    }
                }
            }

            captured_req
        });

        (addr, handle)
    }
}

fn make_adapter(server: &str, port: u16, auth: Option<(String, String)>) -> Socks5Adapter {
    Socks5Adapter::new(
        server,
        server,
        port,
        auth,
        false,
        false,
        Arc::new(crate::dialer::DirectDialer),
    )
}

/// Reports itself as proxied without needing a real front proxy.
struct FakeProxyDialer;

#[async_trait]
impl crate::dialer::TcpDialer for FakeProxyDialer {
    async fn dial(
        &self,
        _host: &str,
        _port: u16,
        _internal: bool,
    ) -> std::io::Result<Box<dyn meow_transport::Stream>> {
        Err(std::io::Error::other("test dialer never connects"))
    }

    fn is_proxy(&self) -> bool {
        true
    }
}

/// Fail-closed under `dialer-proxy`: a front that cannot carry UDP must
/// refuse the association — `NotSupported`, the capability-refusal class
/// exempt from group dead-marking — rather than egressing the relay
/// datagrams from a raw socket on the real source path.
///
/// Enforced in `dial_udp` because the tunnel's UDP dispatch calls it
/// directly without consulting `support_udp()`.
#[tokio::test]
async fn udp_associate_is_refused_under_proxy_dialer() {
    let adapter = Socks5Adapter::new(
        "front",
        "127.0.0.1",
        1080,
        None,
        false,
        false,
        Arc::new(FakeProxyDialer),
    )
    .with_udp(true);

    // `ProxyPacketConn` is not `Debug`, so match rather than `expect_err`.
    match adapter.dial_udp(&meta_with_host("example.com", 443)).await {
        Err(MeowError::NotSupported(m)) => assert!(
            m.contains("dialer-proxy"),
            "refusal should name dialer-proxy, got: {m}"
        ),
        Err(other) => panic!("expected NotSupported, got: {other:?}"),
        Ok(_) => panic!("UDP ASSOCIATE must be refused under a proxy dialer"),
    }

    assert!(
        !adapter.support_udp(),
        "advertised capability must agree with the refusal"
    );
}

#[test]
fn udp_still_advertised_under_direct_dialer() {
    assert!(make_adapter("127.0.0.1", 1080, None)
        .with_udp(true)
        .support_udp());
}

/// Captures the `internal` flag the adapter hands to its dialer —
/// proves `dial_tcp` maps `Metadata::is_internal()` through, which is
/// what keeps probes/housekeeping chained via `dialer-proxy` from
/// counting as use of a lazy front-hop group (#555).
struct CaptureFlagDialer {
    seen: std::sync::Mutex<Option<bool>>,
}

#[async_trait]
impl crate::dialer::TcpDialer for CaptureFlagDialer {
    async fn dial(
        &self,
        _host: &str,
        _port: u16,
        internal: bool,
    ) -> std::io::Result<Box<dyn meow_transport::Stream>> {
        *self.seen.lock().unwrap() = Some(internal);
        Err(std::io::Error::other("test dialer never connects"))
    }
}

#[tokio::test]
async fn dial_tcp_forwards_internal_marker_to_dialer() {
    let dialer = Arc::new(CaptureFlagDialer {
        seen: std::sync::Mutex::new(None),
    });
    let adapter = Socks5Adapter::new(
        "front",
        "127.0.0.1",
        1080,
        None,
        false,
        false,
        Arc::clone(&dialer) as Arc<dyn crate::dialer::TcpDialer>,
    );

    // Probe-shaped metadata (the `ConnType::Tunnel` marker) must reach
    // the dialer as `internal: true`.
    let probe = Metadata {
        conn_type: meow_common::ConnType::Tunnel,
        host: "example.com".into(),
        dst_port: 443,
        ..Default::default()
    };
    let _ = adapter.dial_tcp(&probe).await;
    assert_eq!(
        *dialer.seen.lock().unwrap(),
        Some(true),
        "probe dial must propagate internal=true"
    );

    // User-shaped metadata keeps `internal: false`.
    let _ = adapter.dial_tcp(&meta_with_host("example.com", 443)).await;
    assert_eq!(
        *dialer.seen.lock().unwrap(),
        Some(false),
        "user dial must propagate internal=false"
    );
}

// ─── Chained UDP (`dialer-proxy`) ────────────────────────────────────────

/// Mock SOCKS5 server that answers `UDP ASSOCIATE` with a fixed
/// `BND.ADDR`, then holds the control connection open until the client
/// drops it (association lifetime, RFC 1928 §7).
async fn spawn_associate_server(
    bnd: SocketAddr,
    rep: u8,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        // Method negotiation → no-auth.
        let mut hdr = [0u8; 2];
        s.read_exact(&mut hdr).await.unwrap();
        let mut methods = vec![0u8; hdr[1] as usize];
        s.read_exact(&mut methods).await.unwrap();
        s.write_all(&[VERSION, METHOD_NO_AUTH]).await.unwrap();
        // Request header; assert it is UDP ASSOCIATE.
        let mut req = [0u8; 4];
        s.read_exact(&mut req).await.unwrap();
        assert_eq!(req[0], VERSION);
        assert_eq!(req[1], CMD_UDP_ASSOCIATE);
        drain_socks5_addr(&mut s, req[3]).await.unwrap();
        // Reply VER REP RSV ATYP BND.ADDR BND.PORT.
        let mut resp = vec![VERSION, rep, RESERVED];
        match bnd.ip() {
            IpAddr::V4(v4) => {
                resp.push(ATYP_IPV4);
                resp.extend_from_slice(&v4.octets());
            }
            IpAddr::V6(v6) => {
                resp.push(ATYP_IPV6);
                resp.extend_from_slice(&v6.octets());
            }
        }
        resp.extend_from_slice(&bnd.port().to_be_bytes());
        s.write_all(&resp).await.unwrap();
        // Hold the control conn open until the client hangs up.
        let mut sink = [0u8; 64];
        while s.read(&mut sink).await.unwrap_or(0) > 0 {}
    });
    (addr, handle)
}

/// Variant answering `BND.ADDR` as a domain — servers that relay on a
/// named interface. Exercises the `RelayAddr::Domain` → `UdpTarget::Name`
/// path (issue #657).
async fn spawn_associate_server_domain(
    bnd_host: &str,
    bnd_port: u16,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let host = bnd_host.to_string();
    let handle = tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut hdr = [0u8; 2];
        s.read_exact(&mut hdr).await.unwrap();
        let mut methods = vec![0u8; hdr[1] as usize];
        s.read_exact(&mut methods).await.unwrap();
        s.write_all(&[VERSION, METHOD_NO_AUTH]).await.unwrap();
        let mut req = [0u8; 4];
        s.read_exact(&mut req).await.unwrap();
        assert_eq!(req[1], CMD_UDP_ASSOCIATE);
        drain_socks5_addr(&mut s, req[3]).await.unwrap();
        let mut resp = vec![VERSION, REPLY_SUCCESS, RESERVED, ATYP_DOMAIN];
        resp.push(u8::try_from(host.len()).unwrap());
        resp.extend_from_slice(host.as_bytes());
        resp.extend_from_slice(&bnd_port.to_be_bytes());
        s.write_all(&resp).await.unwrap();
        let mut sink = [0u8; 64];
        while s.read(&mut sink).await.unwrap_or(0) > 0 {}
    });
    (addr, handle)
}

/// Scripted front `ProxyPacketConn`: reads yield injected
/// `(datagram, wire-src)` pairs, writes record `(frame, wire-dst)`.
struct ScriptedFront {
    inbound: tokio::sync::Mutex<tokio::sync::mpsc::Receiver<(Vec<u8>, SocketAddr)>>,
    written: std::sync::Mutex<Vec<(Vec<u8>, SocketAddr)>>,
    bound: SocketAddr,
    /// Truncate every `write_packet` to half the frame — exercises the
    /// datagram-atomicity check on the chained arm.
    short_write: std::sync::atomic::AtomicBool,
    /// Set by `close()` — proves the Chained arm delegates teardown.
    closed: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl ProxyPacketConn for ScriptedFront {
    async fn read_packet(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr)> {
        let (data, src) = self
            .inbound
            .lock()
            .await
            .recv()
            .await
            .ok_or_else(|| MeowError::Proxy("scripted front closed".into()))?;
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        Ok((n, src))
    }

    async fn write_packet(&self, buf: &[u8], addr: &SocketAddr) -> Result<usize> {
        self.written.lock().unwrap().push((buf.to_vec(), *addr));
        if self.short_write.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(buf.len() / 2);
        }
        Ok(buf.len())
    }

    fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.bound)
    }

    fn close(&self) -> Result<()> {
        self.closed
            .store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }
}

/// `dialer-proxy` fake: the TCP control conn is a real connection to the
/// mock SOCKS5 server; `dial_udp_conn` returns the scripted front conn —
/// or a scripted error, for the stale-capability-snapshot arm.
struct ChainedDialer {
    server_addr: std::net::SocketAddr,
    front: std::sync::Mutex<Option<Arc<ScriptedFront>>>,
    udp_dialed: std::sync::Mutex<Vec<crate::dialer::UdpTarget>>,
    udp_ok: bool,
    /// When set, `dial_udp_conn` fails with this `ErrorKind` — models a
    /// group front that rotated to a UDP-less member between the
    /// `supports_udp()` snapshot and the dial.
    dial_udp_err: Option<std::io::ErrorKind>,
    /// When set, `UdpTarget::Name` dials fail `Unsupported` (a front
    /// that cannot carry a domain) while literal `Addr` dials succeed —
    /// exercises the caller's local-resolution fallback (issue #657).
    refuse_name_targets: bool,
    /// `internal` flags seen on `dial_udp_conn` — proves metadata
    /// propagation through the chained leg.
    seen_internal: std::sync::Mutex<Vec<bool>>,
}

#[async_trait]
impl crate::dialer::TcpDialer for ChainedDialer {
    async fn dial(
        &self,
        _host: &str,
        _port: u16,
        _internal: bool,
    ) -> std::io::Result<Box<dyn meow_transport::Stream>> {
        Ok(Box::new(TcpStream::connect(self.server_addr).await?))
    }

    fn is_proxy(&self) -> bool {
        true
    }

    fn supports_udp(&self) -> bool {
        self.udp_ok
    }

    async fn dial_udp_conn(
        &self,
        remote: crate::dialer::UdpTarget,
        internal: bool,
    ) -> std::io::Result<Arc<dyn ProxyPacketConn>> {
        self.seen_internal.lock().unwrap().push(internal);
        if let Some(kind) = self.dial_udp_err {
            return Err(std::io::Error::new(kind, "scripted dial_udp_conn failure"));
        }
        if self.refuse_name_targets && matches!(remote, crate::dialer::UdpTarget::Name { .. }) {
            self.udp_dialed.lock().unwrap().push(remote);
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "front cannot carry domain targets",
            ));
        }
        self.udp_dialed.lock().unwrap().push(remote);
        self.front
            .lock()
            .unwrap()
            .clone()
            .map(|f| f as Arc<dyn ProxyPacketConn>)
            .ok_or_else(|| std::io::Error::other("no scripted front"))
    }
}

fn encode_socks5_datagram(src: SocketAddr, payload: &[u8]) -> Vec<u8> {
    let mut pkt: SmallVec<[u8; 1500]> = SmallVec::new();
    encode_udp_header(&mut pkt, &src);
    pkt.extend_from_slice(payload);
    pkt.to_vec()
}

/// The chained dial must reach the *advertised* relay endpoint — the
/// inner datagram target stays inside the SOCKS5 UDP header.
#[tokio::test]
async fn chained_udp_dials_front_to_advertised_relay() {
    let relay: SocketAddr = "10.200.1.1:5300".parse().unwrap();
    let (srv, server) = spawn_associate_server(relay, REPLY_SUCCESS).await;
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let front = Arc::new(ScriptedFront {
        inbound: tokio::sync::Mutex::new(rx),
        written: std::sync::Mutex::new(Vec::new()),
        bound: relay,
        short_write: std::sync::atomic::AtomicBool::new(false),
        closed: std::sync::atomic::AtomicBool::new(false),
    });
    let dialer = Arc::new(ChainedDialer {
        server_addr: srv,
        front: std::sync::Mutex::new(Some(Arc::clone(&front))),
        udp_dialed: std::sync::Mutex::new(Vec::new()),
        udp_ok: true,
        dial_udp_err: None,
        refuse_name_targets: false,
        seen_internal: std::sync::Mutex::new(Vec::new()),
    });
    let adapter = Socks5Adapter::new(
        "s",
        "127.0.0.1",
        srv.port(),
        None,
        false,
        false,
        Arc::clone(&dialer) as Arc<dyn crate::dialer::TcpDialer>,
    )
    .with_udp(true);
    assert!(adapter.support_udp(), "capable front must advertise UDP");

    let conn = adapter
        .dial_udp(&meta_with_host("example.com", 443))
        .await
        .expect("dial_udp");
    assert_eq!(
        dialer.udp_dialed.lock().unwrap().as_slice(),
        &[crate::dialer::UdpTarget::Addr(relay)],
        "the front association must be bound to the advertised relay"
    );
    assert_eq!(
        dialer.seen_internal.lock().unwrap().as_slice(),
        &[false],
        "user metadata must propagate internal=false to the front dial"
    );

    // Write: wire dst is `relay`; the caller's target lives in the header.
    let target: SocketAddr = "1.2.3.4:443".parse().unwrap();
    conn.write_packet(b"payload", &target).await.unwrap();
    let (frame, wire_dst) = front.written.lock().unwrap().remove(0);
    assert_eq!(wire_dst, relay);
    let mut dec = frame.clone();
    let (data_len, inner_src) = decode_udp_datagram(&mut dec, frame.len()).unwrap();
    assert_eq!(inner_src, target);
    assert_eq!(&dec[..data_len], b"payload");

    // Read: a well-formed reply from the relay decodes back to the target.
    tx.send((encode_socks5_datagram(target, b"pong"), relay))
        .await
        .unwrap();
    let mut buf = [0u8; 256];
    let (n, src) = conn.read_packet(&mut buf).await.unwrap();
    assert_eq!(src, target);
    assert_eq!(&buf[..n], b"pong");

    // close() delegates to the front conn and aborts the control-conn
    // guard; the drop path then finishes the teardown chain — the mock
    // server observes EOF and exits (RFC 1928 §7 association teardown).
    conn.close().unwrap();
    assert!(
        front.closed.load(std::sync::atomic::Ordering::Relaxed),
        "close() must delegate to the front conn"
    );
    drop(conn);
    tokio::time::timeout(std::time::Duration::from_secs(2), server)
        .await
        .expect("server must observe control-conn EOF")
        .expect("server task");
}

/// A server-side ASSOCIATE refusal (`rep != 0`) surfaces as
/// `Socks5ConnectFailed` — same on the chained arm — and the control
/// conn is dropped so the server sees the teardown.
#[tokio::test]
async fn chained_udp_associate_refusal_is_connect_failed() {
    let relay: SocketAddr = "10.200.1.9:5300".parse().unwrap();
    let (srv, server) = spawn_associate_server(relay, REPLY_COMMAND_NOT_SUPPORTED).await;
    let (_tx, rx) = tokio::sync::mpsc::channel(8);
    let front = Arc::new(ScriptedFront {
        inbound: tokio::sync::Mutex::new(rx),
        written: std::sync::Mutex::new(Vec::new()),
        bound: relay,
        short_write: std::sync::atomic::AtomicBool::new(false),
        closed: std::sync::atomic::AtomicBool::new(false),
    });
    let dialer = Arc::new(ChainedDialer {
        server_addr: srv,
        front: std::sync::Mutex::new(Some(Arc::clone(&front))),
        udp_dialed: std::sync::Mutex::new(Vec::new()),
        udp_ok: true,
        dial_udp_err: None,
        refuse_name_targets: false,
        seen_internal: std::sync::Mutex::new(Vec::new()),
    });
    let adapter = Socks5Adapter::new(
        "s",
        "127.0.0.1",
        srv.port(),
        None,
        false,
        false,
        Arc::clone(&dialer) as Arc<dyn crate::dialer::TcpDialer>,
    )
    .with_udp(true);

    match adapter.dial_udp(&meta_with_host("example.com", 443)).await {
        Err(MeowError::Socks5ConnectFailed(REPLY_COMMAND_NOT_SUPPORTED)) => {}
        Err(e) => panic!("expected Socks5ConnectFailed, got: {e:?}"),
        Ok(_) => panic!("a refused ASSOCIATE must not produce a conn"),
    }
    assert!(
        dialer.udp_dialed.lock().unwrap().is_empty(),
        "no front association may be spent on a refused ASSOCIATE"
    );
    tokio::time::timeout(std::time::Duration::from_secs(2), server)
        .await
        .expect("server must observe control-conn EOF")
        .expect("server task");
}

/// Chained reads restore the connected-socket filter: datagrams whose
/// wire source is a specified address other than the relay — and
/// malformed relay datagrams — are per-datagram drops, never fatal.
#[tokio::test]
async fn chained_udp_drops_foreign_source_and_junk() {
    let relay: SocketAddr = "10.200.1.2:5300".parse().unwrap();
    let (srv, _server) = spawn_associate_server(relay, REPLY_SUCCESS).await;
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let front = Arc::new(ScriptedFront {
        inbound: tokio::sync::Mutex::new(rx),
        written: std::sync::Mutex::new(Vec::new()),
        bound: relay,
        short_write: std::sync::atomic::AtomicBool::new(false),
        closed: std::sync::atomic::AtomicBool::new(false),
    });
    let dialer = Arc::new(ChainedDialer {
        server_addr: srv,
        front: std::sync::Mutex::new(Some(Arc::clone(&front))),
        udp_dialed: std::sync::Mutex::new(Vec::new()),
        udp_ok: true,
        dial_udp_err: None,
        refuse_name_targets: false,
        seen_internal: std::sync::Mutex::new(Vec::new()),
    });
    let adapter = Socks5Adapter::new(
        "s",
        "127.0.0.1",
        srv.port(),
        None,
        false,
        false,
        dialer as Arc<dyn crate::dialer::TcpDialer>,
    )
    .with_udp(true);
    let conn = adapter
        .dial_udp(&meta_with_host("example.com", 443))
        .await
        .expect("dial_udp");

    let target: SocketAddr = "5.6.7.8:53".parse().unwrap();
    // Foreign wire source → dropped.
    let foreign: SocketAddr = "9.9.9.9:9999".parse().unwrap();
    tx.send((encode_socks5_datagram(target, b"spoof"), foreign))
        .await
        .unwrap();
    // Malformed relay datagram → dropped.
    tx.send((b"\xde\xad".to_vec(), relay)).await.unwrap();
    // FRAG ≠ 0 (we do not reassemble) → dropped.
    let mut frag = encode_socks5_datagram(target, b"frag");
    frag[2] = 1;
    tx.send((frag, relay)).await.unwrap();
    // Bound conns report no wire source (`0.0.0.0:0` — e.g. a VLESS
    // UoT front): the filter must skip the check, not wedge them.
    let unspecified: SocketAddr = "0.0.0.0:0".parse().unwrap();
    tx.send((encode_socks5_datagram(target, b"ok"), unspecified))
        .await
        .unwrap();

    let mut buf = [0u8; 256];
    let (n, src) = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        conn.read_packet(&mut buf),
    )
    .await
    .expect("read must skip junk, not hang")
    .unwrap();
    assert_eq!(src, target);
    assert_eq!(&buf[..n], b"ok");
}

/// A `0.0.0.0` BND address ("same host as the control connection") is
/// rewritten to the server endpoint before the front is dialed — as
/// `UdpTarget::Name` of the server name (which collapses to `Addr`
/// here because this test's server is an IP literal). Matches mihomo's
/// unspecified-BND substitution, extended by issue #657 to keep the
/// domain form for domain-named servers.
#[tokio::test]
async fn chained_udp_wildcard_bnd_rewritten_to_server_ip() {
    let relay: SocketAddr = "0.0.0.0:5301".parse().unwrap();
    let (srv, _server) = spawn_associate_server(relay, REPLY_SUCCESS).await;
    let (_tx, rx) = tokio::sync::mpsc::channel(8);
    let front = Arc::new(ScriptedFront {
        inbound: tokio::sync::Mutex::new(rx),
        written: std::sync::Mutex::new(Vec::new()),
        bound: "127.0.0.1:5301".parse().unwrap(),
        short_write: std::sync::atomic::AtomicBool::new(false),
        closed: std::sync::atomic::AtomicBool::new(false),
    });
    let dialer = Arc::new(ChainedDialer {
        server_addr: srv,
        front: std::sync::Mutex::new(Some(Arc::clone(&front))),
        udp_dialed: std::sync::Mutex::new(Vec::new()),
        udp_ok: true,
        dial_udp_err: None,
        refuse_name_targets: false,
        seen_internal: std::sync::Mutex::new(Vec::new()),
    });
    let adapter = Socks5Adapter::new(
        "s",
        "127.0.0.1",
        srv.port(),
        None,
        false,
        false,
        Arc::clone(&dialer) as Arc<dyn crate::dialer::TcpDialer>,
    )
    .with_udp(true);
    let _conn = adapter
        .dial_udp(&meta_with_host("example.com", 443))
        .await
        .expect("dial_udp");
    let dialed = dialer.udp_dialed.lock().unwrap().clone();
    assert_eq!(dialed.len(), 1);
    assert_eq!(
        dialed[0],
        crate::dialer::UdpTarget::Addr("127.0.0.1:5301".parse().unwrap()),
    );
}

/// Stale capability snapshot: `supports_udp()` passed, but the front
/// refuses at dial time (e.g. a group rotated to a UDP-less member).
/// The dialer's `ErrorKind::Unsupported` must reconstitute
/// `MeowError::NotSupported` — the exact variant that stays exempt from
/// group dead-marking.
#[tokio::test]
async fn chained_udp_maps_unsupported_to_notsupported() {
    let relay: SocketAddr = "10.200.1.3:5300".parse().unwrap();
    let (srv, _server) = spawn_associate_server(relay, REPLY_SUCCESS).await;
    let (_tx, rx) = tokio::sync::mpsc::channel(8);
    let front = Arc::new(ScriptedFront {
        inbound: tokio::sync::Mutex::new(rx),
        written: std::sync::Mutex::new(Vec::new()),
        bound: relay,
        short_write: std::sync::atomic::AtomicBool::new(false),
        closed: std::sync::atomic::AtomicBool::new(false),
    });
    let dialer = Arc::new(ChainedDialer {
        server_addr: srv,
        front: std::sync::Mutex::new(Some(Arc::clone(&front))),
        udp_dialed: std::sync::Mutex::new(Vec::new()),
        udp_ok: true,
        dial_udp_err: Some(std::io::ErrorKind::Unsupported),
        refuse_name_targets: false,
        seen_internal: std::sync::Mutex::new(Vec::new()),
    });
    let adapter = Socks5Adapter::new(
        "s",
        "127.0.0.1",
        srv.port(),
        None,
        false,
        false,
        dialer as Arc<dyn crate::dialer::TcpDialer>,
    )
    .with_udp(true);

    match adapter.dial_udp(&meta_with_host("example.com", 443)).await {
        Err(MeowError::NotSupported(_)) => {}
        Err(e) => panic!("expected NotSupported, got: {e:?}"),
        Ok(_) => panic!("a front refusal must not produce a live conn"),
    }
}

/// Chained writes must reject a frame that exceeds the u16 limit a
/// stream-framed front could carry, and any short write on the front
/// conn (datagram atomicity — a torn SOCKS5 datagram would desync).
#[tokio::test]
async fn chained_udp_rejects_oversized_and_short_writes() {
    let relay: SocketAddr = "10.200.1.4:5300".parse().unwrap();
    let (srv, _server) = spawn_associate_server(relay, REPLY_SUCCESS).await;
    let (_tx, rx) = tokio::sync::mpsc::channel(8);
    let front = Arc::new(ScriptedFront {
        inbound: tokio::sync::Mutex::new(rx),
        written: std::sync::Mutex::new(Vec::new()),
        bound: relay,
        short_write: std::sync::atomic::AtomicBool::new(false),
        closed: std::sync::atomic::AtomicBool::new(false),
    });
    let dialer = Arc::new(ChainedDialer {
        server_addr: srv,
        front: std::sync::Mutex::new(Some(Arc::clone(&front))),
        udp_dialed: std::sync::Mutex::new(Vec::new()),
        udp_ok: true,
        dial_udp_err: None,
        refuse_name_targets: false,
        seen_internal: std::sync::Mutex::new(Vec::new()),
    });
    let adapter = Socks5Adapter::new(
        "s",
        "127.0.0.1",
        srv.port(),
        None,
        false,
        false,
        dialer as Arc<dyn crate::dialer::TcpDialer>,
    )
    .with_udp(true);
    let conn = adapter
        .dial_udp(&meta_with_host("example.com", 443))
        .await
        .expect("dial_udp");
    let target: SocketAddr = "1.2.3.4:443".parse().unwrap();

    // header(10) + u16::MAX payload already overflows the frame limit.
    let huge = vec![0u8; u16::MAX as usize];
    assert!(
        conn.write_packet(&huge, &target).await.is_err(),
        "oversized frame must be rejected, not wrapped into the stream"
    );

    front
        .short_write
        .store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(
        conn.write_packet(b"payload", &target).await.is_err(),
        "short write on the front conn must error"
    );
}

/// A domain-form BND (`RelayAddr::Domain`) dials the front as
/// `UdpTarget::Name` — the front resolves the relay with the same view
/// that answered the control leg (issue #657).
#[tokio::test]
async fn chained_udp_domain_bnd_dials_name_target() {
    let (srv, _server) = spawn_associate_server_domain("relay.internal", 5300).await;
    let (_tx, rx) = tokio::sync::mpsc::channel(8);
    let front = Arc::new(ScriptedFront {
        inbound: tokio::sync::Mutex::new(rx),
        written: std::sync::Mutex::new(Vec::new()),
        bound: "127.0.0.1:5300".parse().unwrap(),
        short_write: std::sync::atomic::AtomicBool::new(false),
        closed: std::sync::atomic::AtomicBool::new(false),
    });
    let dialer = Arc::new(ChainedDialer {
        server_addr: srv,
        front: std::sync::Mutex::new(Some(Arc::clone(&front))),
        udp_dialed: std::sync::Mutex::new(Vec::new()),
        udp_ok: true,
        dial_udp_err: None,
        refuse_name_targets: false,
        seen_internal: std::sync::Mutex::new(Vec::new()),
    });
    let adapter = Socks5Adapter::new(
        "s",
        "127.0.0.1",
        srv.port(),
        None,
        false,
        false,
        Arc::clone(&dialer) as Arc<dyn crate::dialer::TcpDialer>,
    )
    .with_udp(true);
    let _conn = adapter
        .dial_udp(&meta_with_ipv4(Ipv4Addr::new(8, 8, 8, 8), 53))
        .await
        .expect("dial_udp");
    assert_eq!(
        dialer.udp_dialed.lock().unwrap().as_slice(),
        &[crate::dialer::UdpTarget::Name {
            host: "relay.internal".into(),
            port: 5300
        }],
        "domain BND must reach the front as a name, not a local resolution"
    );
}

/// A wildcard BND on a *domain-named* server dials `UdpTarget::Name` of
/// the server name — "same host as the control connection" must mean
/// the front's resolution of that name, not ours (issue #657).
#[tokio::test]
async fn chained_udp_wildcard_bnd_domain_server_dials_name() {
    let relay: SocketAddr = "0.0.0.0:5301".parse().unwrap();
    let (srv, _server) = spawn_associate_server(relay, REPLY_SUCCESS).await;
    let (_tx, rx) = tokio::sync::mpsc::channel(8);
    let front = Arc::new(ScriptedFront {
        inbound: tokio::sync::Mutex::new(rx),
        written: std::sync::Mutex::new(Vec::new()),
        bound: "127.0.0.1:5301".parse().unwrap(),
        short_write: std::sync::atomic::AtomicBool::new(false),
        closed: std::sync::atomic::AtomicBool::new(false),
    });
    let dialer = Arc::new(ChainedDialer {
        server_addr: srv,
        front: std::sync::Mutex::new(Some(Arc::clone(&front))),
        udp_dialed: std::sync::Mutex::new(Vec::new()),
        udp_ok: true,
        dial_udp_err: None,
        refuse_name_targets: false,
        seen_internal: std::sync::Mutex::new(Vec::new()),
    });
    let adapter = Socks5Adapter::new(
        "s",
        "localhost",
        srv.port(),
        None,
        false,
        false,
        Arc::clone(&dialer) as Arc<dyn crate::dialer::TcpDialer>,
    )
    .with_udp(true);
    let _conn = adapter
        .dial_udp(&meta_with_ipv4(Ipv4Addr::new(8, 8, 8, 8), 53))
        .await
        .expect("dial_udp");
    assert_eq!(
        dialer.udp_dialed.lock().unwrap().as_slice(),
        &[crate::dialer::UdpTarget::Name {
            host: "localhost".into(),
            port: 5301
        }],
    );
}

/// A front that cannot carry a domain target answers `Unsupported`;
/// the adapter resolves the name locally and redials the literal —
/// the pre-#657 behavior, still fail-closed through the chain.
#[tokio::test]
async fn chained_udp_name_unsupported_falls_back_to_local_literal() {
    let relay: SocketAddr = "0.0.0.0:5301".parse().unwrap();
    let (srv, _server) = spawn_associate_server(relay, REPLY_SUCCESS).await;
    let (_tx, rx) = tokio::sync::mpsc::channel(8);
    let front = Arc::new(ScriptedFront {
        inbound: tokio::sync::Mutex::new(rx),
        written: std::sync::Mutex::new(Vec::new()),
        bound: "127.0.0.1:5301".parse().unwrap(),
        short_write: std::sync::atomic::AtomicBool::new(false),
        closed: std::sync::atomic::AtomicBool::new(false),
    });
    let dialer = Arc::new(ChainedDialer {
        server_addr: srv,
        front: std::sync::Mutex::new(Some(Arc::clone(&front))),
        udp_dialed: std::sync::Mutex::new(Vec::new()),
        udp_ok: true,
        dial_udp_err: None,
        refuse_name_targets: true,
        seen_internal: std::sync::Mutex::new(Vec::new()),
    });
    let adapter = Socks5Adapter::new(
        "s",
        "localhost",
        srv.port(),
        None,
        false,
        false,
        Arc::clone(&dialer) as Arc<dyn crate::dialer::TcpDialer>,
    )
    .with_udp(true);
    let _conn = adapter
        .dial_udp(&meta_with_ipv4(Ipv4Addr::new(8, 8, 8, 8), 53))
        .await
        .expect("name refusal falls back to a local literal dial");
    let dialed = dialer.udp_dialed.lock().unwrap().clone();
    assert_eq!(dialed.len(), 2, "name attempt, then literal retry");
    assert_eq!(
        dialed[0],
        crate::dialer::UdpTarget::Name {
            host: "localhost".into(),
            port: 5301
        }
    );
    let crate::dialer::UdpTarget::Addr(addr) = dialed[1] else {
        panic!("fallback must dial a literal, got {:?}", dialed[1]);
    };
    assert!(addr.ip().is_loopback(), "localhost resolves loopback");
    assert_eq!(addr.port(), 5301);
}

/// For a name-bound association the wire source filter is port-only —
/// the front resolved the name with its own view, so the relay source
/// IP legitimately differs from any local resolution (issue #657).
#[tokio::test]
async fn chained_udp_name_relay_src_filter_is_port_only() {
    let (srv, _server) = spawn_associate_server_domain("relay.internal", 5300).await;
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let front = Arc::new(ScriptedFront {
        inbound: tokio::sync::Mutex::new(rx),
        written: std::sync::Mutex::new(Vec::new()),
        bound: "127.0.0.1:5300".parse().unwrap(),
        short_write: std::sync::atomic::AtomicBool::new(false),
        closed: std::sync::atomic::AtomicBool::new(false),
    });
    let dialer = Arc::new(ChainedDialer {
        server_addr: srv,
        front: std::sync::Mutex::new(Some(Arc::clone(&front))),
        udp_dialed: std::sync::Mutex::new(Vec::new()),
        udp_ok: true,
        dial_udp_err: None,
        refuse_name_targets: false,
        seen_internal: std::sync::Mutex::new(Vec::new()),
    });
    let adapter = Socks5Adapter::new(
        "s",
        "127.0.0.1",
        srv.port(),
        None,
        false,
        false,
        Arc::clone(&dialer) as Arc<dyn crate::dialer::TcpDialer>,
    )
    .with_udp(true);
    let conn = adapter
        .dial_udp(&meta_with_ipv4(Ipv4Addr::new(8, 8, 8, 8), 53))
        .await
        .expect("dial_udp");

    // A datagram arriving from the *front's* resolution of
    // relay.internal — a different IP than any local answer, same port.
    let inner: SocketAddr = "8.8.8.8:53".parse().unwrap();
    let mut frame: SmallVec<[u8; 1500]> = SmallVec::new();
    encode_udp_header(&mut frame, &inner);
    frame.extend_from_slice(b"reply");
    tx.send((frame.to_vec(), "198.51.100.99:5300".parse().unwrap()))
        .await
        .unwrap();
    // A foreign source on the wrong port must be dropped.
    let mut junk: SmallVec<[u8; 1500]> = SmallVec::new();
    encode_udp_header(&mut junk, &inner);
    junk.extend_from_slice(b"junk");
    tx.send((junk.to_vec(), "198.51.100.99:9999".parse().unwrap()))
        .await
        .unwrap();

    let mut buf = [0u8; 128];
    // The wrong-port datagram may arrive first; the reader must skip it.
    let (n, src) = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        conn.read_packet(&mut buf),
    )
    .await
    .expect("no read timeout")
    .unwrap();
    assert_eq!(&buf[..n], b"reply");
    assert_eq!(src, inner);
}

/// This adapter as a *front*: host-only metadata (the dialer layer's
/// encoding of `UdpTarget::Name`) stamps every datagram's SOCKS5 DST
/// field with ATYP_DOMAIN — the server resolves the name (issue #657).
#[tokio::test]
async fn front_udp_domain_target_stamps_domain_header() {
    let relay: SocketAddr = "127.0.0.1:5300".parse().unwrap();
    let (srv, _server) = spawn_associate_server(relay, REPLY_SUCCESS).await;
    let (_tx, rx) = tokio::sync::mpsc::channel(8);
    let front = Arc::new(ScriptedFront {
        inbound: tokio::sync::Mutex::new(rx),
        written: std::sync::Mutex::new(Vec::new()),
        bound: relay,
        short_write: std::sync::atomic::AtomicBool::new(false),
        closed: std::sync::atomic::AtomicBool::new(false),
    });
    let dialer = Arc::new(ChainedDialer {
        server_addr: srv,
        front: std::sync::Mutex::new(Some(Arc::clone(&front))),
        udp_dialed: std::sync::Mutex::new(Vec::new()),
        udp_ok: true,
        dial_udp_err: None,
        refuse_name_targets: false,
        seen_internal: std::sync::Mutex::new(Vec::new()),
    });
    let adapter = Socks5Adapter::new(
        "s",
        "127.0.0.1",
        srv.port(),
        None,
        false,
        false,
        Arc::clone(&dialer) as Arc<dyn crate::dialer::TcpDialer>,
    )
    .with_udp(true);
    // The dialer-layer encoding of `UdpTarget::Name`: host set, no dst_ip.
    let meta = Metadata {
        network: meow_common::Network::Udp,
        host: "back.internal".into(),
        dst_port: 8388,
        ..Default::default()
    };
    let conn = adapter.dial_udp(&meta).await.expect("dial_udp");
    conn.write_packet(b"payload", &"0.0.0.0:8388".parse().unwrap())
        .await
        .unwrap();

    let written = front.written.lock().unwrap().clone();
    assert_eq!(written.len(), 1);
    let (frame, dst) = &written[0];
    // RSV(2) FRAG(1) ATYP_DOMAIN LEN host PORT — then payload.
    assert_eq!(&frame[..4], &[0, 0, 0, ATYP_DOMAIN]);
    assert_eq!(frame[4] as usize, "back.internal".len());
    assert_eq!(&frame[5..5 + 13], b"back.internal");
    assert_eq!(
        u16::from_be_bytes([frame[18], frame[19]]),
        8388,
        "domain header port"
    );
    assert_eq!(&frame[20..], b"payload");
    // The wire destination is the relay, not the domain placeholder.
    assert_eq!(*dst, relay);
}

/// A host-only destination longer than 255 bytes cannot be encoded as
/// ATYP_DOMAIN — refuse with `NotSupported` so the dialer layer falls
/// back to a locally-resolved literal (issue #657).
#[tokio::test]
async fn front_udp_overlong_domain_refused() {
    let relay: SocketAddr = "127.0.0.1:5300".parse().unwrap();
    let (srv, _server) = spawn_associate_server(relay, REPLY_SUCCESS).await;
    let (_tx, rx) = tokio::sync::mpsc::channel(8);
    let front = Arc::new(ScriptedFront {
        inbound: tokio::sync::Mutex::new(rx),
        written: std::sync::Mutex::new(Vec::new()),
        bound: relay,
        short_write: std::sync::atomic::AtomicBool::new(false),
        closed: std::sync::atomic::AtomicBool::new(false),
    });
    let dialer = Arc::new(ChainedDialer {
        server_addr: srv,
        front: std::sync::Mutex::new(Some(front)),
        udp_dialed: std::sync::Mutex::new(Vec::new()),
        udp_ok: true,
        dial_udp_err: None,
        refuse_name_targets: false,
        seen_internal: std::sync::Mutex::new(Vec::new()),
    });
    let adapter = Socks5Adapter::new(
        "s",
        "127.0.0.1",
        srv.port(),
        None,
        false,
        false,
        Arc::clone(&dialer) as Arc<dyn crate::dialer::TcpDialer>,
    )
    .with_udp(true);
    let meta = Metadata {
        network: meow_common::Network::Udp,
        host: "x".repeat(256).into(),
        dst_port: 8388,
        ..Default::default()
    };
    match adapter.dial_udp(&meta).await {
        Err(MeowError::NotSupported(_)) => {}
        Err(e) => panic!("overlong domain must refuse NotSupported, got {e}"),
        Ok(_) => panic!("overlong domain must refuse NotSupported"),
    }
}

fn meta_with_host(host: &str, port: u16) -> Metadata {
    Metadata {
        host: host.into(),
        dst_port: port,
        ..Default::default()
    }
}

fn meta_with_ipv4(ip: Ipv4Addr, port: u16) -> Metadata {
    Metadata {
        dst_ip: Some(IpAddr::V4(ip)),
        dst_port: port,
        ..Default::default()
    }
}

// ─── socks5_no_auth_connects ──────────────────────────────────────────────
// upstream: adapter/outbound/socks5.go::DialContext

#[tokio::test]
async fn socks5_no_auth_connects() {
    let (addr, _) = MockServer::new_no_auth().spawn().await;
    let adapter = make_adapter("127.0.0.1", addr.port(), None);
    let meta = meta_with_host("example.com", 443);
    adapter
        .dial_tcp(&meta)
        .await
        .expect("socks5 no-auth connect");
}

// ─── socks5_user_pass_auth_succeeds ───────────────────────────────────────

#[tokio::test]
async fn socks5_user_pass_auth_succeeds() {
    let (addr, _) = MockServer::new_user_pass("bob", "hunter2").spawn().await;
    let adapter = make_adapter(
        "127.0.0.1",
        addr.port(),
        Some(("bob".into(), "hunter2".into())),
    );
    let meta = meta_with_host("example.com", 443);
    adapter
        .dial_tcp(&meta)
        .await
        .expect("socks5 user-pass auth");
}

// ─── socks5_no_acceptable_method_returns_error ────────────────────────────
// NOT retry. NOT fallback to no-auth.

#[tokio::test]
async fn socks5_no_acceptable_method_returns_error() {
    let (addr, _) = MockServer::new_no_acceptable().spawn().await;
    let adapter = make_adapter("127.0.0.1", addr.port(), None);
    let meta = meta_with_host("example.com", 443);
    let err = adapter.dial_tcp(&meta).await.err().expect("expected Err");
    assert!(
        matches!(err, MeowError::NoAcceptableMethod),
        "0xFF must map to NoAcceptableMethod; got {err:?}"
    );
}

// ─── socks5_server_chooses_no_auth_despite_creds_configured ──────────────
// Server may prefer no-auth even when client offers user/pass.
// NOT Err(NoAcceptableMethod). NOT sending auth sub-negotiation.
// upstream: socks5.go::handshake

#[tokio::test]
async fn socks5_server_chooses_no_auth_despite_creds_configured() {
    let (addr, _) = MockServer::new_force_no_auth().spawn().await;
    let adapter = make_adapter(
        "127.0.0.1",
        addr.port(),
        Some(("bob".into(), "hunter2".into())),
    );
    let meta = meta_with_host("example.com", 443);
    // Must succeed — server chose no-auth, skip sub-negotiation.
    adapter
        .dial_tcp(&meta)
        .await
        .expect("server chose no-auth despite creds configured");
}

// ─── socks5_auth_failure_returns_proxy_auth_failed ────────────────────────

#[tokio::test]
async fn socks5_auth_failure_returns_proxy_auth_failed() {
    // Server expects different credentials → auth status != 0x00.
    let (addr, _) = MockServer::new_user_pass("correct", "correct")
        .spawn()
        .await;
    let adapter = make_adapter(
        "127.0.0.1",
        addr.port(),
        Some(("wrong".into(), "wrong".into())),
    );
    let meta = meta_with_host("example.com", 443);
    let err = adapter.dial_tcp(&meta).await.err().expect("expected Err");
    assert!(
        matches!(err, MeowError::ProxyAuthFailed),
        "auth failure must map to ProxyAuthFailed; got {err:?}"
    );
}

// ─── socks5_connect_failure_returns_socks5_connect_failed ────────────────
// rep=0x02 = CONN_NOT_ALLOWED

#[tokio::test]
async fn socks5_connect_failure_returns_socks5_connect_failed() {
    let (addr, _) = MockServer::new_no_auth()
        .with_connect_fail(0x02)
        .spawn()
        .await;
    let adapter = make_adapter("127.0.0.1", addr.port(), None);
    let meta = meta_with_host("example.com", 443);
    let err = adapter.dial_tcp(&meta).await.err().expect("expected Err");
    assert!(
        matches!(err, MeowError::Socks5ConnectFailed(0x02)),
        "rep=0x02 must map to Socks5ConnectFailed(0x02); got {err:?}"
    );
}

// ─── socks5_domain_name_preferred_over_ip ────────────────────────────────
// metadata has both host and dst_ip; assert wire frame uses atyp 0x03 (domain).
// NOT atyp 0x01 (IPv4) when domain is available.

#[tokio::test]
async fn socks5_domain_name_preferred_over_ip() {
    let (addr, handle) = MockServer::new_no_auth().spawn().await;
    let adapter = make_adapter("127.0.0.1", addr.port(), None);

    // Metadata has BOTH host and dst_ip.
    let meta = Metadata {
        host: "example.com".into(),
        dst_ip: Some(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4))),
        dst_port: 80,
        ..Default::default()
    };
    adapter.dial_tcp(&meta).await.expect("dial_tcp");

    let captured = handle.await.unwrap();
    // captured_req layout:
    //   [0] = 0x05 (VERSION from greeting)  [1] = n_methods  [2..] = methods
    //   then CONNECT: [0]=VER [1]=CMD [2]=RSV [3]=ATYP ...
    // The CONNECT header starts at offset 2 + n_methods.
    let n_methods = captured[1] as usize;
    let connect_start = 2 + n_methods;
    let atyp = captured[connect_start + 3];
    assert_eq!(
        atyp, ATYP_DOMAIN,
        "atyp must be 0x03 (domain) when metadata.host is set; got {atyp:#04x}"
    );
}

// ─── socks5_ipv4_used_when_no_hostname ────────────────────────────────────
// metadata has dst_ip only; assert atyp 0x01 frame.

#[tokio::test]
async fn socks5_ipv4_used_when_no_hostname() {
    let (addr, handle) = MockServer::new_no_auth().spawn().await;
    let adapter = make_adapter("127.0.0.1", addr.port(), None);
    let meta = meta_with_ipv4(Ipv4Addr::new(10, 0, 0, 1), 8080);
    adapter.dial_tcp(&meta).await.expect("dial_tcp");

    let captured = handle.await.unwrap();
    let n_methods = captured[1] as usize;
    let connect_start = 2 + n_methods;
    let atyp = captured[connect_start + 3];
    assert_eq!(
        atyp, ATYP_IPV4,
        "atyp must be 0x01 (IPv4) when only dst_ip is set; got {atyp:#04x}"
    );
}

// ─── socks5_hostname_too_long_returns_error ───────────────────────────────
// Pre-VLESS hardening (M1.B-4): hostname > 255 bytes → Proxy error.
// NOT silently truncated. NOT protocol frame sent.
// ADR-0002 Class A divergence from upstream socks5.go.

#[tokio::test]
async fn socks5_hostname_too_long_returns_error() {
    let (addr, _) = MockServer::new_no_auth().spawn().await;
    let adapter = make_adapter("127.0.0.1", addr.port(), None);
    let long_host = "a".repeat(256); // 256 bytes > 255 limit
    let meta = meta_with_host(&long_host, 80);
    let err = adapter.dial_tcp(&meta).await.err().expect("expected Err");
    assert!(
        matches!(err, MeowError::Proxy(ref msg) if msg.contains("hostname too long")),
        "hostname > 255 bytes must return Proxy error; got {err:?}"
    );
}

// ─── socks5_auth_username_too_long_returns_error ──────────────────────────
// RFC 1929 §2: username length field is 1 byte (max 255).
// NOT silently truncated.

#[tokio::test]
async fn socks5_auth_username_too_long_returns_error() {
    let (addr, _) = MockServer::new_user_pass("ignored", "ignored")
        .spawn()
        .await;
    let long_user = "u".repeat(256);
    let adapter = make_adapter("127.0.0.1", addr.port(), Some((long_user, "pass".into())));
    let meta = meta_with_host("example.com", 443);
    let err = adapter.dial_tcp(&meta).await.err().expect("expected Err");
    assert!(
        matches!(err, MeowError::Proxy(ref msg) if msg.contains("username too long")),
        "username > 255 bytes must return Proxy error; got {err:?}"
    );
}

// ─── socks5_auth_password_too_long_returns_error ──────────────────────────
// RFC 1929 §2: password length field is 1 byte (max 255).
// NOT silently truncated.

#[tokio::test]
async fn socks5_auth_password_too_long_returns_error() {
    let (addr, _) = MockServer::new_user_pass("ignored", "ignored")
        .spawn()
        .await;
    let long_pass = "p".repeat(256);
    let adapter = make_adapter("127.0.0.1", addr.port(), Some(("user".into(), long_pass)));
    let meta = meta_with_host("example.com", 443);
    let err = adapter.dial_tcp(&meta).await.err().expect("expected Err");
    assert!(
        matches!(err, MeowError::Proxy(ref msg) if msg.contains("password too long")),
        "password > 255 bytes must return Proxy error; got {err:?}"
    );
}

// ─── UDP ASSOCIATE: disabled by default ────────────────────────────────────

#[tokio::test]
async fn socks5_udp_disabled_by_default() {
    let adapter = make_adapter("127.0.0.1", 1080, None);
    assert!(
        !adapter.support_udp(),
        "udp must be off unless with_udp(true)"
    );
    let meta = meta_with_host("example.com", 53);
    let err = adapter
        .dial_udp(&meta)
        .await
        .err()
        .expect("dial_udp should return Err when udp disabled");
    assert!(
        matches!(err, MeowError::NotSupported(_)),
        "dial_udp must return NotSupported when disabled; got {err:?}"
    );
}

#[test]
fn with_udp_toggles_support() {
    let off = make_adapter("127.0.0.1", 1080, None);
    assert!(!off.support_udp());
    let on = make_adapter("127.0.0.1", 1080, None).with_udp(true);
    assert!(on.support_udp());
}

// ─── UDP datagram header codec (RFC 1928 §7) ───────────────────────────────

#[test]
fn udp_header_roundtrip_v4_and_v6() {
    for target in [
        "1.2.3.4:443".parse::<SocketAddr>().unwrap(),
        "[2001:db8::1]:443".parse::<SocketAddr>().unwrap(),
    ] {
        let mut pkt: SmallVec<[u8; 1500]> = SmallVec::new();
        encode_udp_header(&mut pkt, &target);
        pkt.extend_from_slice(b"payload");
        // Datagram is RSV(2)=0 FRAG(1)=0 then atyp/addr/port/data.
        assert_eq!(&pkt[0..3], &[0, 0, 0]);

        let mut buf = pkt.to_vec();
        let n = buf.len();
        let (data_len, src) = decode_udp_datagram(&mut buf, n).unwrap();
        assert_eq!(src, target);
        assert_eq!(&buf[..data_len], b"payload");
    }
}

#[test]
fn udp_decode_rejects_fragments_and_short() {
    // FRAG != 0 → unsupported.
    let mut frag = vec![0, 0, 1, ATYP_IPV4, 1, 2, 3, 4, 0, 80];
    let n = frag.len();
    assert!(decode_udp_datagram(&mut frag, n).is_err());
    // Too short for even the fixed header.
    let mut short = vec![0, 0];
    assert!(decode_udp_datagram(&mut short, 2).is_err());
}

// ─── UDP ASSOCIATE end-to-end against a mock relay ─────────────────────────

#[tokio::test]
async fn socks5_udp_associate_round_trip() {
    use tokio::net::UdpSocket;
    use tokio::time::{timeout, Duration};

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ctrl_addr = listener.local_addr().unwrap();

    // Mock SOCKS5 server: no-auth negotiation, UDP ASSOCIATE, then a UDP
    // relay that echoes each datagram verbatim (header + data) back to the
    // client — standing in for a remote that echoes.
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        // Method negotiation.
        let mut hdr = [0u8; 2];
        s.read_exact(&mut hdr).await.unwrap();
        let mut methods = vec![0u8; hdr[1] as usize];
        s.read_exact(&mut methods).await.unwrap();
        s.write_all(&[VERSION, METHOD_NO_AUTH]).await.unwrap();
        // UDP ASSOCIATE request: VER CMD RSV ATYP(v4) addr(4) port(2).
        let mut req = [0u8; 10];
        s.read_exact(&mut req).await.unwrap();
        assert_eq!(req[1], CMD_UDP_ASSOCIATE);

        let relay = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let rport = relay.local_addr().unwrap().port();
        s.write_all(&[
            VERSION,
            REPLY_SUCCESS,
            RESERVED,
            ATYP_IPV4,
            127,
            0,
            0,
            1,
            (rport >> 8) as u8,
            (rport & 0xFF) as u8,
        ])
        .await
        .unwrap();

        // Echo loop. Hold `s` (control conn) alive by keeping it in scope.
        let mut buf = [0u8; 2048];
        loop {
            let (n, peer) = relay.recv_from(&mut buf).await.unwrap();
            relay.send_to(&buf[..n], peer).await.unwrap();
        }
    });

    let adapter = make_adapter("127.0.0.1", ctrl_addr.port(), None).with_udp(true);
    assert!(adapter.support_udp());
    let conn = adapter
        .dial_udp(&Metadata::default())
        .await
        .expect("dial_udp");

    let target: SocketAddr = "1.2.3.4:443".parse().unwrap();
    conn.write_packet(b"quic-hello", &target)
        .await
        .expect("write_packet");

    let mut buf = [0u8; 256];
    let (n, src) = timeout(Duration::from_secs(2), conn.read_packet(&mut buf))
        .await
        .expect("read timed out")
        .expect("read_packet");
    assert_eq!(&buf[..n], b"quic-hello");
    assert_eq!(src, target, "decoded source must be the datagram target");
}

// ─── socks5_connect_over_relay ────────────────────────────────────────────
// Pass mock ProxyConn stream; assert handshake runs over it.
// NOT fresh TCP connect.

#[tokio::test]
async fn socks5_connect_over_relay() {
    let (addr, _) = MockServer::new_no_auth().spawn().await;
    let adapter = make_adapter("127.0.0.1", addr.port(), None);

    // Establish the "outer" connection.
    let tcp = TcpStream::connect(addr).await.unwrap();
    let outer: Box<dyn ProxyConn> = Box::new(tcp);

    let meta = meta_with_host("example.com", 443);
    let mut conn = adapter
        .connect_over(outer, &meta)
        .await
        .expect("connect_over");

    // Tunnel is live — echo works.
    conn.write_all(b"hi").await.unwrap();
    let mut buf = [0u8; 2];
    conn.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"hi");
}

/// A zero-length `ATYP_DOMAIN` BND cannot name a relay — it is a
/// malformed reply, not a degenerate name-bound association (issue
/// #657 review).
#[tokio::test]
async fn relay_addr_rejects_empty_domain() {
    // atyp=DOMAIN, len=0, port=0x1F90 — the port bytes are present but
    // must never be read into a valid RelayAddr.
    let mut wire: &[u8] = &[0x00, 0x1f, 0x90];
    let Err(e) = read_relay_addr(&mut wire, ATYP_DOMAIN).await else {
        panic!("empty domain BND must error");
    };
    assert!(e.to_string().contains("empty domain"), "{e}");
}
