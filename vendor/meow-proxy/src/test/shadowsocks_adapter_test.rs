use super::*;
use meow_common::atomic::Uint;
use std::sync::Mutex;

#[test]
fn sip003u_mode_extraction() {
    // Mode has no PartialEq; compare via its tcp/udp semantics.
    let sem = |m: Mode| (m.enable_tcp(), m.enable_udp());

    // Default: no opts → TcpOnly, nothing forwarded.
    let (m, o) = extract_sip003_plugin_mode(None);
    assert_eq!(sem(m), (true, false));
    assert_eq!(o, None);

    // No mode token → TcpOnly, opts preserved verbatim.
    let (m, o) = extract_sip003_plugin_mode(Some("host=a.com;path=/x"));
    assert_eq!(sem(m), (true, false));
    assert_eq!(o.as_deref(), Some("host=a.com;path=/x"));

    // SIP003u transport mode is consumed and translated; other opts kept.
    let (m, o) = extract_sip003_plugin_mode(Some("mode=tcp_and_udp;host=a.com"));
    assert_eq!(sem(m), (true, true));
    assert_eq!(o.as_deref(), Some("host=a.com"));

    let (m, o) = extract_sip003_plugin_mode(Some("mode=udp_only"));
    assert_eq!(sem(m), (false, true));
    assert_eq!(o, None, "a lone transport-mode token leaves empty opts");

    // A plugin-specific `mode` value (v2ray quic, obfs tls) is NOT consumed.
    let (m, o) = extract_sip003_plugin_mode(Some("mode=quic;host=a.com"));
    assert_eq!(
        sem(m),
        (true, false),
        "non-transport mode must not change Mode"
    );
    assert_eq!(o.as_deref(), Some("mode=quic;host=a.com"));
}

/// A dialer that reports itself as proxied but cannot carry UDP —
/// stands in for a `ProxyDialer` whose front hop lacks UDP support.
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

/// Loopback `ProxyPacketConn` playing the *server* side of the SS UDP
/// protocol: `write_packet` decrypts the client datagram (real
/// `crypto_io`, real key) and queues a server-side encrypted echo
/// addressed from the client's target. Drives the whole `Chained` codec
/// — encryption, session control, bound-remote contract — without a
/// socket.
struct FakeSsUdpServer {
    bound: SocketAddr,
    context: SharedContext,
    method: CipherKind,
    key: Vec<u8>,
    /// Server-side association ID (SIP0222 §3.2.3) — nonzero, or the
    /// client's `accept_reply` filter would drop the echo.
    server_session_id: u64,
    reply_packet_id: Mutex<u64>,
    /// Outer addrs `write_packet` was handed — must always be the SS
    /// server endpoint the association is bound to.
    outer: Mutex<Vec<SocketAddr>>,
    /// Decrypted inner targets + payloads the "server" received.
    inner: Mutex<Vec<(Address, Vec<u8>)>>,
    reply_tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    reply_rx: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>>,
}

impl FakeSsUdpServer {
    fn new(bound: SocketAddr, core: &SsCore) -> Self {
        let (reply_tx, reply_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            bound,
            context: Arc::clone(&core.context),
            method: core.server_config.method(),
            key: core.server_config.key().to_vec(),
            server_session_id: 0x5345_5256_4552_5349, // "SERVERSE"-ish
            reply_packet_id: Mutex::new(0),
            outer: Mutex::new(Vec::new()),
            inner: Mutex::new(Vec::new()),
            reply_tx,
            reply_rx: tokio::sync::Mutex::new(reply_rx),
        }
    }
}

#[async_trait]
impl ProxyPacketConn for FakeSsUdpServer {
    async fn read_packet(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr)> {
        let pkt = self
            .reply_rx
            .lock()
            .await
            .recv()
            .await
            .ok_or_else(|| MeowError::Proxy("fake ss server closed".into()))?;
        let n = pkt.len().min(buf.len());
        buf[..n].copy_from_slice(&pkt[..n]);
        Ok((n, self.bound))
    }

    async fn write_packet(&self, buf: &[u8], addr: &SocketAddr) -> Result<usize> {
        use shadowsocks::relay::udprelay::crypto_io::{
            decrypt_client_payload, encrypt_server_payload,
        };
        self.outer.lock().unwrap().push(*addr);
        let mut pkt = buf.to_vec();
        let (n, target, control) =
            decrypt_client_payload(&self.context, self.method, &self.key, &mut pkt, None)
                .map_err(|e| MeowError::Proxy(format!("fake server decrypt: {e}")))?;
        self.inner
            .lock()
            .unwrap()
            .push((target.clone(), pkt[..n].to_vec()));
        // Reply like ssserver: payload echoed, addressed from the
        // target. AEAD-2022 replies carry the echoed client session ID
        // (kept by `unwrap_or_default`) plus the server's own session
        // ID + a fresh packet ID; non-2022 methods ignore `control`.
        let mut ctrl = control.unwrap_or_default();
        ctrl.server_session_id = self.server_session_id;
        ctrl.packet_id = {
            let mut n = self.reply_packet_id.lock().unwrap();
            *n += 1;
            *n
        };
        let mut reply = bytes::BytesMut::new();
        encrypt_server_payload(
            &self.context,
            self.method,
            &self.key,
            &target,
            &ctrl,
            &pkt[..n],
            &mut reply,
        );
        let _ = self.reply_tx.send(reply.to_vec());
        Ok(buf.len())
    }

    fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.bound)
    }

    fn close(&self) -> Result<()> {
        Ok(())
    }
}

/// A front conn that replays a scripted `(bytes, reported_src)` queue —
/// exercises the chained read path's drop-and-continue branches (junk
/// ciphertext, foreign wire source) without sockets.
struct ScriptedFront {
    remote: SocketAddr,
    script: Mutex<std::collections::VecDeque<(Vec<u8>, SocketAddr)>>,
    written: Mutex<Vec<Vec<u8>>>,
    short_write: std::sync::atomic::AtomicBool,
}

impl ScriptedFront {
    fn new(remote: SocketAddr) -> Self {
        Self {
            remote,
            script: Mutex::new(std::collections::VecDeque::new()),
            written: Mutex::new(Vec::new()),
            short_write: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl ProxyPacketConn for ScriptedFront {
    async fn read_packet(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr)> {
        let Some((pkt, src)) = self.script.lock().unwrap().pop_front() else {
            // Script exhausted: park forever (a real conn would block on
            // the wire); tests close via `close`/drop.
            return std::future::pending().await;
        };
        let n = pkt.len().min(buf.len());
        buf[..n].copy_from_slice(&pkt[..n]);
        Ok((n, src))
    }

    async fn write_packet(&self, buf: &[u8], addr: &SocketAddr) -> Result<usize> {
        assert_eq!(
            *addr, self.remote,
            "chained writes must go to the SS server endpoint"
        );
        self.written.lock().unwrap().push(buf.to_vec());
        if self.short_write.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(buf.len() / 2);
        }
        Ok(buf.len())
    }

    fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.remote)
    }

    fn close(&self) -> Result<()> {
        Ok(())
    }
}

/// Dialer variant that returns a [`ScriptedFront`] conn.
struct ScriptedFrontDialer {
    front: Mutex<Option<Arc<ScriptedFront>>>,
}

#[async_trait]
impl crate::dialer::TcpDialer for ScriptedFrontDialer {
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

    fn supports_udp(&self) -> bool {
        true
    }

    async fn dial_udp_conn(
        &self,
        _remote: UdpTarget,
        _internal: bool,
    ) -> std::io::Result<Arc<dyn ProxyPacketConn>> {
        self.front
            .lock()
            .unwrap()
            .clone()
            .map(|f| f as Arc<dyn ProxyPacketConn>)
            .ok_or_else(|| std::io::Error::other("no scripted front"))
    }
}

/// A `dialer-proxy` dialer whose UDP endpoint is [`FakeSsUdpServer`] —
/// installed after adapter construction because the fake needs the
/// adapter's crypto context.
#[derive(Default)]
struct FakeChainedDialer {
    server: Mutex<Option<Arc<FakeSsUdpServer>>>,
    dialed: Mutex<Vec<UdpTarget>>,
    /// Refuse `UdpTarget::Name` dials `Unsupported` (a front that cannot
    /// carry a domain) — exercises the caller's local-resolution
    /// fallback (issue #657).
    refuse_name: bool,
}

#[async_trait]
impl crate::dialer::TcpDialer for FakeChainedDialer {
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

    fn supports_udp(&self) -> bool {
        true
    }

    async fn dial_udp_conn(
        &self,
        remote: UdpTarget,
        _internal: bool,
    ) -> std::io::Result<Arc<dyn ProxyPacketConn>> {
        if self.refuse_name && matches!(remote, UdpTarget::Name { .. }) {
            self.dialed.lock().unwrap().push(remote);
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "front cannot carry domain targets",
            ));
        }
        self.dialed.lock().unwrap().push(remote);
        self.server
            .lock()
            .unwrap()
            .clone()
            .map(|s| s as Arc<dyn ProxyPacketConn>)
            .ok_or_else(|| std::io::Error::other("fake front has no server"))
    }
}

fn ss_adapter(udp: bool, dialer: Arc<dyn crate::dialer::TcpDialer>) -> ShadowsocksAdapter {
    ss_adapter_with(udp, "password", "aes-256-gcm", dialer)
}

fn ss_adapter_with(
    udp: bool,
    password: &str,
    cipher: &str,
    dialer: Arc<dyn crate::dialer::TcpDialer>,
) -> ShadowsocksAdapter {
    ShadowsocksAdapter::new(
        "ss-test",
        "127.0.0.1",
        8388,
        password,
        cipher,
        udp,
        None,
        None,
        None,
        dialer,
    )
    .expect("adapter builds")
}

/// Variant whose server is named by domain — exercises the
/// `UdpTarget::Name` dial (issue #657).
fn ss_adapter_named(
    udp: bool,
    server: &str,
    dialer: Arc<dyn crate::dialer::TcpDialer>,
) -> ShadowsocksAdapter {
    ShadowsocksAdapter::new(
        "ss-test",
        server,
        8388,
        "password",
        "aes-256-gcm",
        udp,
        None,
        None,
        None,
        dialer,
    )
    .expect("adapter builds")
}

/// Fail-closed: under a `dialer-proxy` chain the plain SS UDP relay must
/// never reach the raw-socket path — a front that cannot carry UDP
/// refuses the association instead of leaking the real source path.
///
/// `dial_udp` is the enforcement point on purpose: routing probes do
/// consult `support_udp` for target eligibility (tunnel.rs
/// `RouteTargetProbe`), so the advertisement alone can steer a UDP
/// flow to a later rule — but the dispatch path (`meow-tunnel/src/
/// udp.rs`) calls `dial_udp` directly, so gating only the
/// advertisement would leave the leak open for any rule that
/// references the outbound by name.
#[tokio::test]
async fn chained_udp_fails_closed_when_front_cannot_carry() {
    let adapter = ss_adapter(true, Arc::new(FakeProxyDialer));

    // `ProxyPacketConn` is not `Debug`, so match rather than `expect_err`.
    // Assert the variant, not the text: `NotSupported` is the class that
    // keeps the refusal exempt from group dead-marking — a reworded
    // `Proxy` would silently break the exemption.
    match adapter.dial_udp(&Metadata::default()).await {
        Err(MeowError::NotSupported(msg)) => assert!(
            msg.contains("dialer-proxy"),
            "refusal should name dialer-proxy, got: {msg}"
        ),
        Err(e) => panic!("expected NotSupported refusal, got: {e:?}"),
        Ok(_) => panic!("chained UDP must fail when the front cannot carry it"),
    }

    assert!(
        !adapter.support_udp(),
        "advertised capability must agree: no UDP through a UDP-less front"
    );
}

/// The `dialer-proxy` UDP path: `dial_udp` must dial the SS server's UDP
/// endpoint through the injected dialer (`dial_udp_conn`, never a raw
/// socket) and run the SS crypto layer over the returned conn.
#[tokio::test]
async fn chained_udp_round_trips_ss_crypto_over_front_conn() {
    // AEAD-2022 exercises the `SsUdpSession` control path end-to-end:
    // the fake server echoes the client session ID and stamps its own
    // server session ID / packet counter, which `accept_reply` must
    // accept. (Password for 2022 is base64 of the 32-byte key.)
    for (password, cipher) in [
        ("password", "aes-256-gcm"),
        (
            "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=",
            "2022-blake3-aes-256-gcm",
        ),
    ] {
        let dialer = Arc::new(FakeChainedDialer::default());
        let adapter = ss_adapter_with(true, password, cipher, Arc::clone(&dialer) as _);
        assert!(
            adapter.support_udp(),
            "a UDP-capable front makes chained UDP advertised ({cipher})"
        );
        let server = Arc::new(FakeSsUdpServer::new(
            "127.0.0.1:8388".parse().unwrap(),
            &adapter.core,
        ));
        *dialer.server.lock().unwrap() = Some(Arc::clone(&server));

        let conn = adapter
            .dial_udp(&Metadata::default())
            .await
            .expect("chained dial_udp");

        // The dialer was handed the resolved SS server UDP endpoint.
        assert_eq!(
            dialer.dialed.lock().unwrap().as_slice(),
            &[UdpTarget::Addr("127.0.0.1:8388".parse().unwrap())],
        );

        let target: SocketAddr = "8.8.8.8:53".parse().unwrap();
        // Two datagrams: the 2022 reply filter must accept increasing
        // server packet IDs within one server session.
        for payload in [b"hello chained udp".as_slice(), b"second".as_slice()] {
            conn.write_packet(payload, &target).await.expect("write");
            let mut buf = [0u8; 2048];
            let (n, src) = conn.read_packet(&mut buf).await.expect("read");
            assert_eq!(&buf[..n], payload, "{cipher}");
            assert_eq!(src, target, "reply source is the inner SS target");
        }

        // Wire shape: the front conn saw only the SS-server endpoint —
        // the real destination rode encrypted inside the SS payload.
        assert_eq!(
            server.outer.lock().unwrap().as_slice(),
            &[
                "127.0.0.1:8388".parse().unwrap(),
                "127.0.0.1:8388".parse().unwrap()
            ],
        );
        let inner = server.inner.lock().unwrap();
        assert_eq!(inner.len(), 2);
        assert_eq!(inner[0].0, Address::SocketAddress(target));
        assert_eq!(inner[0].1, b"hello chained udp");
    }
}

/// Chained read path: junk ciphertext and foreign wire sources drop
/// per-datagram without killing the association — the filter then
/// delivers the first valid reply.
#[tokio::test]
async fn chained_udp_drops_junk_and_foreign_sources() {
    let remote: SocketAddr = "127.0.0.1:8388".parse().unwrap();
    let front = Arc::new(ScriptedFront::new(remote));
    let adapter = ss_adapter(
        true,
        Arc::new(ScriptedFrontDialer {
            front: Mutex::new(Some(Arc::clone(&front))),
        }),
    );
    let conn = adapter
        .dial_udp(&Metadata::default())
        .await
        .expect("chained dial_udp");

    // Mint a server reply with the same crypto/context the server would
    // use, so the client arm's decrypt + session filter must accept it.
    let server = FakeSsUdpServer::new(remote, &adapter.core);
    let target: SocketAddr = "8.8.8.8:53".parse().unwrap();
    let mut reply = bytes::BytesMut::new();
    shadowsocks::relay::udprelay::crypto_io::encrypt_server_payload(
        &server.context,
        server.method,
        &server.key,
        &Address::SocketAddress(target),
        &Default::default(),
        b"real-reply",
        &mut reply,
    );

    let foreign: SocketAddr = "9.9.9.9:1234".parse().unwrap();
    {
        let mut q = front.script.lock().unwrap();
        // 1. junk ciphertext from the right source → decrypt fails → drop
        q.push_back((vec![0xde, 0xad, 0xbe, 0xef], remote));
        // 2. valid ciphertext from a foreign source → source filter drops
        q.push_back((reply.to_vec(), foreign));
        // 3. the real reply from the bound remote → delivered
        q.push_back((reply.to_vec(), remote));
    }

    let mut buf = [0u8; 2048];
    let (n, src) = conn.read_packet(&mut buf).await.expect("read");
    assert_eq!(&buf[..n], b"real-reply");
    assert_eq!(src, target);
}

/// Chained write path: ciphertext beyond u16::MAX is refused before the
/// write (stream fronts frame with a u16 length — a wrap would desync),
/// and a front that returns a short write is an error, never a silent
/// truncation of ciphertext.
#[tokio::test]
async fn chained_udp_rejects_oversized_and_short_writes() {
    let remote: SocketAddr = "127.0.0.1:8388".parse().unwrap();
    let front = Arc::new(ScriptedFront::new(remote));
    let adapter = ss_adapter(
        true,
        Arc::new(ScriptedFrontDialer {
            front: Mutex::new(Some(Arc::clone(&front))),
        }),
    );
    let conn = adapter
        .dial_udp(&Metadata::default())
        .await
        .expect("chained dial_udp");

    let target: SocketAddr = "8.8.8.8:53".parse().unwrap();

    // Oversized: a u16::MAX payload encrypts to > u16::MAX under any
    // cipher's overhead — but the write must never reach the front.
    let big = vec![0u8; u16::MAX as usize];
    let writes_before = front.written.lock().unwrap().len();
    match conn.write_packet(&big, &target).await {
        Err(e) if front.written.lock().unwrap().len() == writes_before => {
            assert!(
                format!("{e:?}").contains("u16"),
                "expected u16 refusal: {e:?}"
            );
        }
        Err(e) => panic!("oversized datagram touched the front conn: {e:?}"),
        Ok(n) => panic!("oversized datagram reported success ({n})"),
    }

    // Short write: the front reports fewer bytes written.
    front
        .short_write
        .store(true, std::sync::atomic::Ordering::Relaxed);
    match conn.write_packet(b"payload", &target).await {
        Err(e) => {
            assert!(
                format!("{e:?}").contains("truncat"),
                "expected short-write error: {e:?}"
            );
        }
        Ok(n) => panic!("short write reported success ({n})"),
    }
}

/// A domain-named SS server dials the front as `UdpTarget::Name` — the
/// front resolves the server with the same view that carried the TCP
/// leg, closing the split-horizon/GeoDNS divergence (issue #657).
#[tokio::test]
async fn chained_udp_domain_server_dials_name_target() {
    let dialer = Arc::new(FakeChainedDialer::default());
    let adapter = ss_adapter_named(true, "localhost", Arc::clone(&dialer) as _);
    let server = Arc::new(FakeSsUdpServer::new(
        "127.0.0.1:8388".parse().unwrap(),
        &adapter.core,
    ));
    *dialer.server.lock().unwrap() = Some(server);

    let conn = adapter
        .dial_udp(&Metadata::default())
        .await
        .expect("chained dial_udp");

    assert_eq!(
        dialer.dialed.lock().unwrap().as_slice(),
        &[UdpTarget::Name {
            host: "localhost".into(),
            port: 8388
        }],
        "domain server must reach the front as a name, not a local resolution"
    );

    // Writes take the advisory placeholder dst; replies filtered by
    // port only (the front's resolution legitimately differs from ours).
    let target: SocketAddr = "8.8.8.8:53".parse().unwrap();
    conn.write_packet(b"ping", &target).await.expect("write");
    let mut buf = [0u8; 2048];
    let (n, src) = conn.read_packet(&mut buf).await.expect("read");
    assert_eq!(&buf[..n], b"ping");
    assert_eq!(src, target);
}

/// A front that cannot carry a domain target answers `Unsupported`;
/// the adapter resolves the name locally and redials the literal —
/// the pre-#657 behavior, still fail-closed through the chain.
#[tokio::test]
async fn chained_udp_name_unsupported_falls_back_to_literal() {
    let dialer = Arc::new(FakeChainedDialer {
        refuse_name: true,
        ..Default::default()
    });
    let adapter = ss_adapter_named(true, "localhost", Arc::clone(&dialer) as _);
    let server = Arc::new(FakeSsUdpServer::new(
        "127.0.0.1:8388".parse().unwrap(),
        &adapter.core,
    ));
    *dialer.server.lock().unwrap() = Some(server);

    let _conn = adapter
        .dial_udp(&Metadata::default())
        .await
        .expect("name refusal falls back to a local literal dial");

    let dialed = dialer.dialed.lock().unwrap().clone();
    assert_eq!(dialed.len(), 2, "name attempt, then literal retry");
    assert_eq!(
        dialed[0],
        UdpTarget::Name {
            host: "localhost".into(),
            port: 8388
        }
    );
    let UdpTarget::Addr(addr) = dialed[1] else {
        panic!("fallback must dial a literal, got {:?}", dialed[1]);
    };
    assert!(addr.ip().is_loopback(), "localhost resolves loopback");
    assert_eq!(addr.port(), 8388);
}

/// This adapter as a *front*: host-only metadata (the dialer layer's
/// encoding of `UdpTarget::Name`) stamps the *inner* SS payload target
/// with the domain so the SS server resolves it — while the front
/// conn's wire destination stays the SS server endpoint (issue #657).
#[tokio::test]
async fn front_udp_domain_target_encrypts_domain_inner() {
    let remote: SocketAddr = "127.0.0.1:8388".parse().unwrap();
    let front = Arc::new(ScriptedFront::new(remote));
    let adapter = ss_adapter(
        true,
        Arc::new(ScriptedFrontDialer {
            front: Mutex::new(Some(Arc::clone(&front))),
        }),
    );
    // The dialer-layer encoding of `UdpTarget::Name`: host set, no dst_ip.
    let meta = Metadata {
        network: meow_common::Network::Udp,
        host: "back.internal".into(),
        dst_port: 8388,
        ..Default::default()
    };
    let conn = adapter.dial_udp(&meta).await.expect("front dial_udp");
    // The caller holds no literal — its write arg is the placeholder.
    conn.write_packet(b"payload", &"0.0.0.0:8388".parse().unwrap())
        .await
        .expect("write");

    // One ciphertext frame went to the SS server endpoint on the wire...
    let written = front.written.lock().unwrap().clone();
    assert_eq!(written.len(), 1);
    // ...and decrypts to a *domain* target addressed to back.internal.
    use shadowsocks::relay::udprelay::crypto_io::decrypt_client_payload;
    let mut pkt = written[0].clone();
    let (n, target, _ctrl) = decrypt_client_payload(
        &adapter.core.context,
        adapter.core.server_config.method(),
        adapter.core.server_config.key(),
        &mut pkt,
        None,
    )
    .expect("frame decrypts");
    assert_eq!(&pkt[..n], b"payload");
    assert_eq!(
        target,
        Address::DomainNameAddress("back.internal".to_string(), 8388),
        "inner SS target must carry the domain, not the advisory arg"
    );
}

/// Every built-in TCP-only plugin must refuse `dial_udp` and name
/// itself in the refusal — the tunnel's UDP dispatch never consults
/// `support_udp`, so this is the enforcement seam.
///
/// `mux=false` on the gost fixture keeps it valid under a
/// `ss`-without-`mux` build (the upstream mux default is a parse
/// error there).
#[tokio::test]
async fn builtin_tcp_plugins_refuse_udp() {
    for (plugin, opts, tag) in [
        ("v2ray-plugin", "host=cdn.example.com", "v2ray-plugin"),
        (
            "gost-plugin",
            "mode=websocket;mux=false;host=cdn.example.com",
            "gost-plugin",
        ),
        (
            "shadow-tls",
            "host=cover.example.com;password=p;version=2",
            "shadow-tls",
        ),
        (
            "restls",
            "host=cover.example.com;password=p;version-hint=tls13",
            "restls",
        ),
        ("jls", "host=cover.example.com;username=u;password=p", "jls"),
    ] {
        let adapter = ShadowsocksAdapter::new(
            "ss-test",
            "127.0.0.1",
            8388,
            "password",
            "aes-256-gcm",
            true,
            Some(plugin),
            Some(opts),
            None,
            Arc::new(crate::dialer::DirectDialer),
        )
        .unwrap_or_else(|e| panic!("{plugin} adapter builds: {e}"));
        assert!(
            !adapter.support_udp(),
            "{plugin} must not advertise UDP it will refuse"
        );
        match adapter.dial_udp(&Metadata::default()).await {
            Err(MeowError::NotSupported(m)) => assert!(
                m.contains(tag),
                "{plugin} refusal should name itself, got: {m}"
            ),
            Err(e) => panic!("{plugin} must refuse UDP with NotSupported, got: {e}"),
            Ok(_) => panic!("{plugin} must refuse UDP, got Ok"),
        }
    }
}

/// A relay-supplied stream already terminates at the real SS server —
/// a built-in transport that owns its TCP dial (shadow-tls) must fail
/// loudly on `connect_over` rather than send unwrapped SS traffic
/// through the front hop.
#[tokio::test]
async fn shadow_tls_refuses_relay_supplied_stream() {
    let adapter = ShadowsocksAdapter::new(
        "ss-test",
        "127.0.0.1",
        8388,
        "password",
        "aes-256-gcm",
        true,
        Some("shadow-tls"),
        Some("host=cover.example.com;password=p;version=2"),
        None,
        Arc::new(crate::dialer::DirectDialer),
    )
    .expect("shadow-tls adapter builds");
    let (stream, _peer) = tokio::io::duplex(64);
    let stream = crate::stream_conn::StreamConn(Box::new(stream));
    match adapter
        .connect_over(Box::new(stream), &Metadata::default())
        .await
    {
        Err(MeowError::NotSupported(m)) => {
            assert!(m.contains("shadow-tls"), "refusal names itself: {m}");
        }
        Err(e) => panic!("expected NotSupported, got: {e}"),
        Ok(_) => panic!("shadow-tls must refuse a relay-supplied stream"),
    }
}

/// SIP022 §3.2.2/§3.2.4: a client session mints a non-zero ID, counts
/// packets up, and filters replies — echo match, replay drop, and
/// server-session rotation resetting the window.
#[test]
fn ss_udp_session_allocates_ids_and_filters_replies() {
    let ctx = Context::new_shared(ServerType::Local);
    let session = SsUdpSession::new(&ctx, "2022-blake3-aes-256-gcm".parse().unwrap());
    assert_ne!(session.client_session_id, 0);

    let c0 = session.send_control().unwrap();
    let c1 = session.send_control().unwrap();
    assert_eq!(c0.client_session_id, session.client_session_id);
    assert_eq!(
        c0.packet_id, 1,
        "packet IDs are pre-incremented, sslocal-style"
    );
    assert_eq!(c1.packet_id, 2, "client packet IDs count up per session");

    let mut reply = UdpSocketControlData::default();
    reply.client_session_id = session.client_session_id;
    reply.server_session_id = 77;
    reply.packet_id = 0;
    assert!(session.accept_reply(&reply));
    assert!(
        !session.accept_reply(&reply),
        "replayed server packet dropped"
    );

    let mut foreign = reply.clone();
    foreign.client_session_id = 0xdead_beef;
    foreign.packet_id = 9;
    assert!(
        !session.accept_reply(&foreign),
        "an echo for a different client session is not ours"
    );
    // A foreign echo must not have touched session 77's window.
    reply.packet_id = 9;
    assert!(session.accept_reply(&reply));

    // Server-session rotation (a restart legitimately re-keys): a fresh
    // ID gets its own window — reusing packet IDs is legal under it.
    reply.server_session_id = 88;
    reply.packet_id = 0;
    assert!(session.accept_reply(&reply));

    // Interleaved sessions keep independent windows: flapping back to
    // 77 must NOT re-accept its already-seen packet IDs.
    reply.server_session_id = 77;
    reply.packet_id = 0;
    assert!(
        !session.accept_reply(&reply),
        "session 77's window survives the flap"
    );
    reply.packet_id = 10;
    assert!(session.accept_reply(&reply));

    // `server_session_id == 0` is the reserved "no session" sentinel:
    // it never opens a window.
    reply.server_session_id = 0;
    reply.packet_id = 42;
    assert!(
        !session.accept_reply(&reply),
        "the 0 sentinel is rejected outright"
    );
}

/// The client packet-ID space is terminal rather than wrapping:
/// `send_control` returns `None` once exhausted (32-bit targets run out
/// at 2^32 datagrams) so the caller can error the association and the
/// next datagram re-dials under a fresh session — and the space never
/// emits `Uint::MAX`, the window's always-reject sentinel.
#[test]
fn send_control_none_on_exhaustion() {
    let ctx = Context::new_shared(ServerType::Local);
    let session = SsUdpSession::new(&ctx, "2022-blake3-aes-256-gcm".parse().unwrap());
    session
        .next_packet_id
        .store(Uint::MAX - 1, std::sync::atomic::Ordering::Relaxed);
    assert!(
        session.send_control().is_none(),
        "an exhausted packet-ID space errors the association"
    );
    assert_eq!(
        session
            .next_packet_id
            .load(std::sync::atomic::Ordering::Relaxed),
        Uint::MAX - 1,
        "the counter stays terminal rather than wrapping to 0"
    );
}

/// §3.2.4: reaching [`MAX_TRACKED_SERVER_SESSIONS`] evicts the least
/// recently used window only — never clears the table. A `clear()`
/// would let an on-path attacker replay N distinct captured
/// server-session IDs to re-open every window at once; per-entry
/// eviction bounds the damage to the single evicted session.
#[test]
fn server_reply_tracker_evicts_lru_not_all() {
    let ctx = Context::new_shared(ServerType::Local);
    let session = SsUdpSession::new(&ctx, "2022-blake3-aes-256-gcm".parse().unwrap());
    let reply = |ssid: u64, pid: u64| {
        let mut c = UdpSocketControlData::default();
        c.client_session_id = session.client_session_id;
        c.server_session_id = ssid;
        c.packet_id = pid;
        c
    };

    // Fill the table: server sessions 1..=4 each record packet ID 5.
    for s in 1..=MAX_TRACKED_SERVER_SESSIONS as u64 {
        assert!(session.accept_reply(&reply(s, 5)));
    }

    // A fifth distinct session ID evicts exactly the LRU entry
    // (session 1) — every other window must keep its state.
    assert!(session.accept_reply(&reply(5, 5)));
    for s in 2..=4u64 {
        assert!(
            !session.accept_reply(&reply(s, 5)),
            "session {s}'s window must survive the LRU eviction"
        );
    }

    // The evicted session is the only one whose recorded IDs are
    // re-accepted — the bounded blast radius of per-entry eviction.
    assert!(
        session.accept_reply(&reply(1, 5)),
        "only the evicted LRU window forgets its IDs"
    );
}

/// The same adapter with the default direct dialer keeps advertising UDP —
/// the guard must not regress the non-chained path.
#[test]
fn plain_udp_still_advertised_under_direct_dialer() {
    assert!(ss_adapter(true, Arc::new(crate::dialer::DirectDialer)).support_udp());
    assert!(!ss_adapter(false, Arc::new(crate::dialer::DirectDialer)).support_udp());
}

#[test]
fn test_is_builtin_obfs_plugin_accepts_aliases() {
    assert!(is_builtin_obfs_plugin("obfs"));
    assert!(is_builtin_obfs_plugin("simple-obfs"));
    assert!(!is_builtin_obfs_plugin("v2ray-plugin"));
    assert!(!is_builtin_obfs_plugin("gost-plugin"));
    assert!(!is_builtin_obfs_plugin("OBFS"));
    assert!(!is_builtin_obfs_plugin(""));
}

#[test]
fn test_parse_obfs_opts_http_yaml_keys() {
    // YAML map form serializes to `mode=http;host=foo`.
    let got = parse_obfs_opts(Some("mode=http;host=bing.com"), "1.2.3.4").unwrap();
    match got {
        BuiltinObfs::Http { host } => assert_eq!(host, "bing.com"),
        _ => panic!("expected Http"),
    }
}

#[test]
fn test_parse_obfs_opts_tls_yaml_keys() {
    let got = parse_obfs_opts(Some("mode=tls;host=gateway.icloud.com"), "1.2.3.4").unwrap();
    match got {
        BuiltinObfs::Tls { server } => assert_eq!(server, "gateway.icloud.com"),
        _ => panic!("expected Tls"),
    }
}

#[test]
fn test_parse_obfs_opts_sip003_alias_keys() {
    // Native SIP003 keys (`obfs`/`obfs-host`).
    let got = parse_obfs_opts(Some("obfs=http;obfs-host=cloudflare.com"), "1.2.3.4").unwrap();
    match got {
        BuiltinObfs::Http { host } => assert_eq!(host, "cloudflare.com"),
        _ => panic!("expected Http"),
    }
}

#[test]
fn test_parse_obfs_opts_rejects_ctl_host() {
    // `host` lands verbatim in the emitted `Host:` header (http) or SNI
    // (tls) — provider-supplied CTLs and oversized names must not parse
    // (issue #648). An embedded `;`/`=` is already unusable as a value
    // (the tokenizer splits there), so CTLs are the injection vector.
    for opts in [
        "mode=http;host=a\rb\nc",
        "mode=tls;obfs-host=a\0b",
        "mode=http;host=a\tb",
    ] {
        assert!(
            parse_obfs_opts(Some(opts), "1.2.3.4").is_err(),
            "{opts:?} must be rejected"
        );
    }
    let overlong = format!("mode=http;host={}", "a".repeat(254));
    assert!(parse_obfs_opts(Some(&overlong), "1.2.3.4").is_err());
    let at_limit = format!("mode=http;host={}", "a".repeat(253));
    assert!(parse_obfs_opts(Some(&at_limit), "1.2.3.4").is_ok());
}

#[test]
fn test_parse_obfs_opts_mode_is_case_insensitive() {
    let http = parse_obfs_opts(Some("mode=HTTP;host=foo"), "1.2.3.4").unwrap();
    assert!(matches!(http, BuiltinObfs::Http { .. }));
    let tls = parse_obfs_opts(Some("mode=TLS;host=foo"), "1.2.3.4").unwrap();
    assert!(matches!(tls, BuiltinObfs::Tls { .. }));
    let mixed = parse_obfs_opts(Some("mode=TlS;host=foo"), "1.2.3.4").unwrap();
    assert!(matches!(mixed, BuiltinObfs::Tls { .. }));
}

#[test]
fn test_parse_obfs_opts_extra_whitespace() {
    // Tolerate whitespace around `;` and `=`, similar to the Go reference.
    let got = parse_obfs_opts(Some("  mode = http ;  host = bing.com  "), "1.2.3.4").unwrap();
    match got {
        BuiltinObfs::Http { host } => assert_eq!(host, "bing.com"),
        _ => panic!("expected Http"),
    }
}

#[test]
fn test_parse_obfs_opts_unknown_keys_ignored() {
    // Forward-compat: unknown keys must be silently dropped.
    let got = parse_obfs_opts(
        Some("mode=http;host=foo;fastopen=1;something=else"),
        "1.2.3.4",
    )
    .unwrap();
    match got {
        BuiltinObfs::Http { host } => assert_eq!(host, "foo"),
        _ => panic!("expected Http"),
    }
}

#[test]
fn test_parse_obfs_opts_missing_host_falls_back_to_server() {
    let got = parse_obfs_opts(Some("mode=http"), "ss.example.org").unwrap();
    match got {
        BuiltinObfs::Http { host } => assert_eq!(host, "ss.example.org"),
        _ => panic!("expected Http"),
    }
}

#[test]
fn test_parse_obfs_opts_empty_host_falls_back_to_server() {
    let got = parse_obfs_opts(Some("mode=tls;host="), "ss.example.org").unwrap();
    match got {
        BuiltinObfs::Tls { server } => assert_eq!(server, "ss.example.org"),
        _ => panic!("expected Tls"),
    }
}

#[test]
fn test_parse_obfs_opts_missing_mode_errors() {
    let err = parse_obfs_opts(Some("host=bing.com"), "1.2.3.4").unwrap_err();
    match err {
        MeowError::Config(msg) => assert!(
            msg.contains("mode=http") || msg.contains("mode=tls"),
            "error message should mention valid modes: {msg}"
        ),
        _ => panic!("expected Config error"),
    }
}

#[test]
fn test_parse_obfs_opts_empty_opts_errors() {
    // Empty / missing opts is also "no mode" — must error.
    assert!(parse_obfs_opts(None, "1.2.3.4").is_err());
    assert!(parse_obfs_opts(Some(""), "1.2.3.4").is_err());
    assert!(parse_obfs_opts(Some("   "), "1.2.3.4").is_err());
}

#[test]
fn test_parse_obfs_opts_invalid_mode_errors() {
    let err = parse_obfs_opts(Some("mode=quic;host=foo"), "1.2.3.4").unwrap_err();
    match err {
        MeowError::Config(msg) => {
            assert!(msg.contains("quic"), "error should mention bad mode: {msg}");
            assert!(
                msg.contains("http") && msg.contains("tls"),
                "error should hint valid modes: {msg}"
            );
        }
        _ => panic!("expected Config error"),
    }
}

#[test]
fn test_parse_obfs_opts_yaml_overrides_sip003_when_both_present() {
    // If both `mode=` and `obfs=` are passed (unusual but legal), the last
    // one wins after iteration order. Document the behavior so future
    // changes notice if it breaks: with the current parser, `obfs` and
    // `mode` are aliases, so the latter parsed wins.
    let got = parse_obfs_opts(Some("mode=http;obfs=tls;host=foo"), "1.2.3.4").unwrap();
    assert!(matches!(got, BuiltinObfs::Tls { .. }));
    let got = parse_obfs_opts(Some("obfs=tls;mode=http;host=foo"), "1.2.3.4").unwrap();
    assert!(matches!(got, BuiltinObfs::Http { .. }));
}

/// The shadowsocks crate `assert!`s `domain.len() <= u8::MAX` while
/// serializing — a >255-byte target must error, never panic (issue #657
/// review; the TCP `parse_address` hole predates the UDP path).
#[test]
fn parse_address_rejects_oversized_domain() {
    let meta = Metadata {
        host: "a".repeat(256).into(),
        dst_port: 443,
        ..Default::default()
    };
    let Err(e) = parse_address(&meta) else {
        panic!("256-byte domain must error");
    };
    assert!(matches!(e, MeowError::NotSupported(_)), "{e:?}");

    // 255 stays encodable.
    let meta = Metadata {
        host: "a".repeat(255).into(),
        dst_port: 443,
        ..Default::default()
    };
    assert!(matches!(
        parse_address(&meta),
        Ok(Address::DomainNameAddress(..))
    ));
}

/// Front role: a chained `UdpTarget::Name` arrives as host-only UDP
/// metadata — an oversized name must refuse `NotSupported` (so the
/// caller falls back to a literal `Addr`) rather than trip the crate's
/// assert during packet encode.
#[tokio::test]
async fn front_udp_refuses_oversized_domain_target() {
    let adapter = ss_adapter_named(true, "127.0.0.1", Arc::new(FakeProxyDialer));
    let meta = Metadata {
        network: meow_common::Network::Udp,
        host: "a".repeat(256).into(),
        dst_port: 8388,
        ..Default::default()
    };
    let Err(e) = adapter.dial_udp(&meta).await else {
        panic!("oversized domain target must error");
    };
    assert!(matches!(e, MeowError::NotSupported(_)), "{e:?}");
}
