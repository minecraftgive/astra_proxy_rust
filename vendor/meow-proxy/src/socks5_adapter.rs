//! SOCKS5 outbound proxy adapter (M1.B-4).
//!
//! Implements `ProxyAdapter` for `type: socks5` config entries.  Supports:
//! - Auth method negotiation: no-auth (0x00) and username/password (0x02).
//! - `CMD CONNECT` (0x01) — TCP tunnel.
//! - `atyp` 0x03 (domain) preferred when `metadata.host` is set;
//!   0x01 (IPv4) or 0x04 (IPv6) otherwise.
//! - Optional TLS-wrapping of the TCP control connection to the proxy server.
//! - `CMD UDP ASSOCIATE` (0x03) when `udp: true` — relays UDP (incl. QUIC /
//!   HTTP/3) via a side UDP transport, wrapping each datagram in the RFC 1928
//!   §7 header. The TCP control connection is held open for the association's
//!   lifetime. The UDP relay path is plain (not TLS-wrapped), per the SOCKS5
//!   spec. Under `dialer-proxy` the relay datagrams ride the front proxy's
//!   own `dial_udp` association to the advertised relay endpoint (mihomo
//!   `proxyDialer.ListenPacket`); a UDP-less front fails closed.
//!
//! upstream: `adapter/outbound/socks5.go`

use std::net::{IpAddr, SocketAddr};

use async_trait::async_trait;
use meow_common::{
    AdapterType, MeowError, Metadata, ProxyAdapter, ProxyConn, ProxyHealth, ProxyPacketConn, Result,
};
use smallvec::SmallVec;
use smol_str::SmolStr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UdpSocket;
use tracing::debug;

use crate::dialer::UdpTarget;
use crate::stream_conn::StreamConn;

// ─── SOCKS5 constants ─────────────────────────────────────────────────────────

const VERSION: u8 = 0x05;
const CMD_CONNECT: u8 = 0x01;
const CMD_UDP_ASSOCIATE: u8 = 0x03;
const RESERVED: u8 = 0x00;
const ATYP_IPV4: u8 = 0x01;
const ATYP_DOMAIN: u8 = 0x03;
const ATYP_IPV6: u8 = 0x04;
const METHOD_NO_AUTH: u8 = 0x00;
const METHOD_USER_PASS: u8 = 0x02;
const METHOD_NO_ACCEPTABLE: u8 = 0xFF;
const AUTH_VERSION: u8 = 0x01;
const AUTH_SUCCESS: u8 = 0x00;
const REPLY_SUCCESS: u8 = 0x00;
/// ATYP_DOMAIN carries a one-byte length — a longer name cannot ride the
/// wire and the requester falls back to a literal resolution.
const MAX_DOMAIN_LEN: usize = u8::MAX as usize;

// ─── Adapter ─────────────────────────────────────────────────────────────────

/// SOCKS5 outbound proxy adapter.
///
/// upstream: `adapter/outbound/socks5.go` — `Socks5`
pub struct Socks5Adapter {
    name: SmolStr,
    server: SmolStr,
    port: u16,
    /// `"server:port"` — returned by `addr()` for relay metadata building.
    addr_str: SmolStr,
    /// `Some((username, password))` — both present or neither (ADR-0002 Class A).
    auth: Option<(String, String)>,
    /// Built once at construction (rustls ClientConfig + root store are
    /// expensive); `TlsLayer::connect` is safe to call concurrently.
    tls_layer: Option<meow_transport::tls::TlsLayer>,
    /// Whether `udp: true` was configured — gates UDP ASSOCIATE.
    udp: bool,
    health: ProxyHealth,
    dialer: Arc<dyn crate::dialer::TcpDialer>,
}

impl Socks5Adapter {
    /// Create a `Socks5Adapter`. UDP ASSOCIATE is disabled by default; call
    /// [`Self::with_udp`] to enable it (set from `udp: true` in config).
    pub fn new(
        name: &str,
        server: &str,
        port: u16,
        auth: Option<(String, String)>,
        tls: bool,
        skip_cert_verify: bool,
        dialer: Arc<dyn crate::dialer::TcpDialer>,
    ) -> Self {
        // Hoisted out of the dial path: TlsLayer::new clones the webpki root
        // store and builds verifier + crypto provider — per-adapter, not
        // per-connection (same pattern as TrojanAdapter::new).
        let tls_layer = tls.then(|| {
            use meow_transport::tls::{TlsConfig, TlsLayer};
            let tls_cfg = TlsConfig {
                skip_cert_verify,
                ..TlsConfig::new(server)
            };
            TlsLayer::new(&tls_cfg)
                .expect("Socks5Adapter: failed to build TlsLayer — check TLS config")
        });

        Self {
            name: SmolStr::from(name),
            addr_str: SmolStr::from(format!("{server}:{port}")),
            server: SmolStr::from(server),
            port,
            auth,
            tls_layer,
            udp: false,
            health: ProxyHealth::new(),
            dialer,
        }
    }

    /// Enable SOCKS5 UDP ASSOCIATE (HTTP/3 / QUIC relay). Off by default.
    #[must_use]
    pub fn with_udp(mut self, udp: bool) -> Self {
        self.udp = udp;
        self
    }

    /// Dial TCP to the proxy server, optionally wrapping in TLS.
    async fn dial_stream(&self, internal: bool) -> Result<Box<dyn meow_transport::Stream>> {
        let tcp = self
            .dialer
            .dial(&self.server, self.port, internal)
            .await
            .map_err(MeowError::Io)?;
        self.wrap_tls(tcp).await
    }

    /// Apply the configured TLS layer to `stream` if `tls: true`.
    /// `stream` must already terminate at this SOCKS5 server — `dial_tcp`
    /// obtains it from `dialer.dial`, `connect_over` receives it from the
    /// relay chain.
    async fn wrap_tls(
        &self,
        stream: Box<dyn meow_transport::Stream>,
    ) -> Result<Box<dyn meow_transport::Stream>> {
        if let Some(tls_layer) = &self.tls_layer {
            use meow_transport::Transport;
            tls_layer
                .connect(Box::new(stream))
                .await
                .map_err(|e| match e {
                    meow_transport::TransportError::Io(e) => MeowError::Io(e),
                    other => MeowError::Proxy(other.to_string()),
                })
        } else {
            Ok(stream)
        }
    }

    /// Run the full SOCKS5 handshake (auth negotiation + CONNECT) over `stream`.
    ///
    /// `target_host` — destination hostname (used as `atyp 0x03` when non-empty).
    /// `target_ip`   — destination IP (used as `atyp 0x01`/`0x04` when host is empty).
    /// `target_port` — destination port.
    async fn run_handshake<S>(
        &self,
        stream: &mut S,
        target_host: &str,
        target_ip: Option<IpAddr>,
        target_port: u16,
    ) -> Result<()>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        // SOCKS5 ATYP 0x03 encodes the hostname length as a single byte (0–255).
        // Casting `len() as u8` when len > 255 silently truncates and produces a
        // malformed frame; we reject early instead.
        // ADR-0002 Class A divergence: upstream socks5.go does not guard these.
        if !target_host.is_empty() && target_host.len() > 255 {
            return Err(MeowError::Proxy(format!(
                "socks5: hostname too long ({} bytes, max 255 per protocol)",
                target_host.len()
            )));
        }

        self.negotiate_and_auth(stream).await?;

        // ── Step 3: CONNECT request ───────────────────────────────────────────
        //
        // Prefer domain name (atyp 0x03) when metadata.host is set;
        // fall back to IPv4/IPv6 literal otherwise.
        // upstream: socks5.go — uses hostname when available, NOT IP-only dial.
        let mut req_buf = [0u8; 262];
        req_buf[0] = VERSION;
        req_buf[1] = CMD_CONNECT;
        req_buf[2] = RESERVED;
        let mut pos = 3;

        if target_host.is_empty() {
            match target_ip {
                Some(IpAddr::V4(v4)) => {
                    req_buf[pos] = ATYP_IPV4;
                    pos += 1;
                    req_buf[pos..pos + 4].copy_from_slice(&v4.octets());
                    pos += 4;
                }
                Some(IpAddr::V6(v6)) => {
                    req_buf[pos] = ATYP_IPV6;
                    pos += 1;
                    req_buf[pos..pos + 16].copy_from_slice(&v6.octets());
                    pos += 16;
                }
                None => {
                    return Err(MeowError::Proxy(
                        "socks5: no destination address in metadata".into(),
                    ));
                }
            }
        } else {
            let host_bytes = target_host.as_bytes();
            req_buf[pos] = ATYP_DOMAIN;
            pos += 1;
            req_buf[pos] = host_bytes.len() as u8;
            pos += 1;
            req_buf[pos..pos + host_bytes.len()].copy_from_slice(host_bytes);
            pos += host_bytes.len();
        }

        req_buf[pos] = (target_port >> 8) as u8;
        req_buf[pos + 1] = (target_port & 0xFF) as u8;
        pos += 2;
        stream
            .write_all(&req_buf[..pos])
            .await
            .map_err(MeowError::Io)?;

        // ── Step 4: CONNECT response ──────────────────────────────────────────
        // [0x05, rep, 0x00, atyp, bnd_addr..., bnd_port_hi, bnd_port_lo]
        let mut resp_hdr = [0u8; 4];
        stream
            .read_exact(&mut resp_hdr)
            .await
            .map_err(MeowError::Io)?;

        if resp_hdr[1] != REPLY_SUCCESS {
            return Err(MeowError::Socks5ConnectFailed(resp_hdr[1]));
        }

        // Drain the bound address (we don't use it for TCP relay).
        drain_socks5_addr(stream, resp_hdr[3]).await?;

        Ok(())
    }

    /// Method negotiation + optional RFC 1929 username/password sub-negotiation.
    /// Shared by CONNECT ([`Self::run_handshake`]) and UDP ASSOCIATE
    /// ([`Self::run_udp_associate`]).
    async fn negotiate_and_auth<S>(&self, stream: &mut S) -> Result<()>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        // RFC 1929 §2 encodes username/password lengths as single bytes.
        if let Some((user, pass)) = &self.auth {
            if user.len() > 255 {
                return Err(MeowError::Proxy(format!(
                    "socks5: username too long ({} bytes, max 255 per RFC 1929)",
                    user.len()
                )));
            }
            if pass.len() > 255 {
                return Err(MeowError::Proxy(format!(
                    "socks5: password too long ({} bytes, max 255 per RFC 1929)",
                    pass.len()
                )));
            }
        }

        // ── Step 1: Method negotiation ────────────────────────────────────────
        let methods: &[u8] = if self.auth.is_some() {
            &[METHOD_NO_AUTH, METHOD_USER_PASS]
        } else {
            &[METHOD_NO_AUTH]
        };

        let greeting_len = 2 + methods.len();
        let mut greeting = [0u8; 4];
        greeting[0] = VERSION;
        greeting[1] = methods.len() as u8;
        greeting[2..greeting_len].copy_from_slice(methods);
        stream
            .write_all(&greeting[..greeting_len])
            .await
            .map_err(MeowError::Io)?;

        let mut server_choice = [0u8; 2];
        stream
            .read_exact(&mut server_choice)
            .await
            .map_err(MeowError::Io)?;

        if server_choice[0] != VERSION {
            return Err(MeowError::Proxy(format!(
                "socks5: unexpected version byte {:#04x} in method selection",
                server_choice[0]
            )));
        }

        let chosen = server_choice[1];
        if chosen == METHOD_NO_ACCEPTABLE {
            return Err(MeowError::NoAcceptableMethod);
        }

        // ── Step 2: Username/password sub-negotiation (if server chose 0x02) ──
        //
        // upstream: socks5.go::handshake — if the server picks no-auth even when
        // credentials were offered, proceed WITHOUT sub-negotiation.
        if chosen == METHOD_USER_PASS {
            let (user, pass) = self
                .auth
                .as_ref()
                .expect("auth set when METHOD_USER_PASS offered");

            let auth_len = 3 + user.len() + pass.len();
            let mut auth_buf = [0u8; 515];
            auth_buf[0] = AUTH_VERSION;
            auth_buf[1] = user.len() as u8;
            auth_buf[2..2 + user.len()].copy_from_slice(user.as_bytes());
            auth_buf[2 + user.len()] = pass.len() as u8;
            auth_buf[3 + user.len()..auth_len].copy_from_slice(pass.as_bytes());
            stream
                .write_all(&auth_buf[..auth_len])
                .await
                .map_err(MeowError::Io)?;

            let mut auth_resp = [0u8; 2];
            stream
                .read_exact(&mut auth_resp)
                .await
                .map_err(MeowError::Io)?;

            if auth_resp[1] != AUTH_SUCCESS {
                return Err(MeowError::ProxyAuthFailed);
            }
        }

        Ok(())
    }

    /// Run a UDP ASSOCIATE handshake over the (already TLS-wrapped, if
    /// configured) control stream and return the relay endpoint the proxy
    /// expects UDP datagrams on.
    ///
    /// The DST.ADDR/DST.PORT in the request is the wildcard `0.0.0.0:0`: per
    /// RFC 1928 it advertises the address the client will send from, which we
    /// don't know ahead of the first packet — `0.0.0.0:0` tells the server not
    /// to restrict the association to a specific source.
    ///
    /// A `BND.ADDR` of `0.0.0.0` / `::` (some servers return the wildcard
    /// meaning "same host as this control connection") comes back as
    /// [`RelayAddr::Server`]; a domain BND stays a name. The caller picks
    /// the resolution side — local for the raw path, the front's resolver
    /// view via [`UdpTarget::Name`] under `dialer-proxy`.
    async fn run_udp_associate<S>(&self, stream: &mut S) -> Result<RelayAddr>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        self.negotiate_and_auth(stream).await?;

        // Request: VER CMD RSV ATYP(v4) 0.0.0.0 0
        let req = [
            VERSION,
            CMD_UDP_ASSOCIATE,
            RESERVED,
            ATYP_IPV4,
            0,
            0,
            0,
            0, // DST.ADDR 0.0.0.0
            0,
            0, // DST.PORT 0
        ];
        stream.write_all(&req).await.map_err(MeowError::Io)?;

        // Response header: VER REP RSV ATYP
        let mut resp_hdr = [0u8; 4];
        stream
            .read_exact(&mut resp_hdr)
            .await
            .map_err(MeowError::Io)?;
        if resp_hdr[1] != REPLY_SUCCESS {
            return Err(MeowError::Socks5ConnectFailed(resp_hdr[1]));
        }

        let relay = read_relay_addr(stream, resp_hdr[3]).await?;
        // A zero BND port is not a usable relay endpoint (mihomo rejects it
        // as a malformed server address) — refuse rather than blackhole.
        let port = match &relay {
            RelayAddr::Addr(a) => a.port(),
            RelayAddr::Server { port, .. } | RelayAddr::Domain { port, .. } => *port,
        };
        if port == 0 {
            return Err(MeowError::Proxy(format!(
                "socks5: server advertised unusable relay {relay:?}"
            )));
        }
        Ok(relay)
    }

    /// Resolve a [`RelayAddr`] to a concrete `SocketAddr` locally — the raw
    /// (unchained) path, and the chained fallback when the front cannot
    /// carry a domain target.
    ///
    /// A wildcard BND prefers a candidate in the wildcard's own family:
    /// `0.0.0.0` means "same host, on this v4 socket" — a dual-stack
    /// hostname must not substitute a v6 address the relay never bound
    /// (mihomo resolves with the node's IPv4-prefer for the same reason).
    async fn resolve_relay(&self, relay: &RelayAddr) -> Result<SocketAddr> {
        match relay {
            RelayAddr::Addr(addr) => Ok(*addr),
            RelayAddr::Server { port, v6 } => {
                let candidates = meow_common::resolve_host_all(&self.server, *port)
                    .await
                    .map_err(MeowError::Io)?;
                candidates
                    .iter()
                    .find(|c| c.is_ipv6() == *v6)
                    .or_else(|| candidates.first())
                    .copied()
                    .ok_or_else(|| {
                        MeowError::Proxy(format!(
                            "socks5: cannot resolve relay host {}",
                            self.server
                        ))
                    })
            }
            RelayAddr::Domain { host, port } => meow_common::resolve_host_all(host, *port)
                .await
                .map_err(MeowError::Io)?
                .into_iter()
                .next()
                .ok_or_else(|| MeowError::Proxy(format!("socks5: cannot resolve bound {host}"))),
        }
    }
}

/// Read and discard the bound address + port from a SOCKS5 response.
///
/// `atyp` is the address-type byte already read from the response header.
async fn drain_socks5_addr<S: tokio::io::AsyncRead + Unpin>(
    stream: &mut S,
    atyp: u8,
) -> Result<()> {
    let addr_len: usize = match atyp {
        ATYP_IPV4 => 4,
        ATYP_DOMAIN => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await.map_err(MeowError::Io)?;
            len[0] as usize
        }
        ATYP_IPV6 => 16,
        other => {
            return Err(MeowError::Proxy(format!(
                "socks5: unknown atyp {other:#04x} in response"
            )));
        }
    };
    // addr bytes + 2-byte port
    let mut discard = vec![0u8; addr_len + 2];
    stream
        .read_exact(&mut discard)
        .await
        .map_err(MeowError::Io)?;
    Ok(())
}

/// Where the SOCKS5 server said its UDP relay lives — the `BND` of a
/// UDP-ASSOCIATE reply, kept in its wire form so a `dialer-proxy` front can
/// resolve a name with its own view instead of ours (issue #657).
#[derive(Debug)]
enum RelayAddr {
    /// Literal IP endpoint.
    Addr(SocketAddr),
    /// Domain endpoint — the server named its relay by domain.
    Domain { host: smol_str::SmolStr, port: u16 },
    /// Wildcard BND (`0.0.0.0`/`::`): "same host as this control
    /// connection". `v6` remembers the wildcard's address family so a
    /// local resolution can prefer it; when the endpoint is delegated to a
    /// front as `UdpTarget::Name` the hint is lost and the front's resolver
    /// picks the family. Closing it would mean threading the preference
    /// through `Metadata` — a hot, ADR-0011-tracked type — for the only
    /// consumer that could honor it (a front resolving the name locally),
    /// while wire-carrying fronts have no protocol field for it at all;
    /// the local-resolution fallback already re-applies `v6` via
    /// `resolve_relay`.
    /// Documented as a known semantic gap (issue #666).
    Server { port: u16, v6: bool },
}

impl RelayAddr {
    /// The [`UdpTarget`] this endpoint dials under `dialer-proxy`:
    /// literals pass through, while wildcard/domain forms hand the name to
    /// the front so *its* resolver picks the backend the control leg used.
    /// A "name" that is actually an IP literal collapses to [`UdpTarget::Addr`]
    /// — the stricter source filter stays available and no resolution is
    /// needed anywhere.
    fn to_udp_target(&self, server: &str) -> UdpTarget {
        match self {
            RelayAddr::Addr(addr) => UdpTarget::Addr(*addr),
            RelayAddr::Server { port, .. } => UdpTarget::named(server, *port),
            RelayAddr::Domain { host, port } => UdpTarget::named(host, *port),
        }
    }
}

/// Read a SOCKS5 address (`atyp` already consumed) into its wire form —
/// domain `atyp` is *kept as a name*; resolution is the caller's choice
/// (local for the raw path, delegated to the front under `dialer-proxy`).
async fn read_relay_addr<S: tokio::io::AsyncRead + Unpin>(
    stream: &mut S,
    atyp: u8,
) -> Result<RelayAddr> {
    match atyp {
        ATYP_IPV4 => {
            let mut b = [0u8; 6];
            stream.read_exact(&mut b).await.map_err(MeowError::Io)?;
            let ip = IpAddr::from([b[0], b[1], b[2], b[3]]);
            let port = u16::from_be_bytes([b[4], b[5]]);
            if ip.is_unspecified() {
                return Ok(RelayAddr::Server { port, v6: false });
            }
            Ok(RelayAddr::Addr(SocketAddr::new(ip, port)))
        }
        ATYP_IPV6 => {
            let mut b = [0u8; 18];
            stream.read_exact(&mut b).await.map_err(MeowError::Io)?;
            let mut o = [0u8; 16];
            o.copy_from_slice(&b[..16]);
            let ip = IpAddr::from(o);
            let port = u16::from_be_bytes([b[16], b[17]]);
            if ip.is_unspecified() {
                return Ok(RelayAddr::Server { port, v6: true });
            }
            Ok(RelayAddr::Addr(SocketAddr::new(ip, port)))
        }
        ATYP_DOMAIN => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await.map_err(MeowError::Io)?;
            // A zero-length name cannot name a relay — treat it as a
            // malformed reply rather than binding a degenerate association.
            if len[0] == 0 {
                return Err(MeowError::Proxy(
                    "socks5: empty domain in UDP associate reply".into(),
                ));
            }
            let mut dbuf = vec![0u8; len[0] as usize + 2];
            stream.read_exact(&mut dbuf).await.map_err(MeowError::Io)?;
            let host = std::str::from_utf8(&dbuf[..len[0] as usize])
                .map_err(|_| MeowError::Proxy("socks5: non-utf8 bound domain".into()))?;
            let port = u16::from_be_bytes([dbuf[len[0] as usize], dbuf[len[0] as usize + 1]]);
            Ok(RelayAddr::Domain {
                host: smol_str::SmolStr::new(host),
                port,
            })
        }
        other => Err(MeowError::Proxy(format!(
            "socks5: unknown atyp {other:#04x} in response"
        ))),
    }
}

/// Encode a SOCKS5 UDP request header for `addr` into `out` (RFC 1928 §7):
/// `RSV(2)=0 FRAG(1)=0 ATYP DST.ADDR DST.PORT`. The DATA is appended by the
/// caller.
fn encode_udp_header(out: &mut SmallVec<[u8; 1500]>, addr: &SocketAddr) {
    out.extend_from_slice(&[0, 0, 0]); // RSV(2) + FRAG(1)
    match addr.ip() {
        IpAddr::V4(v4) => {
            out.push(ATYP_IPV4);
            out.extend_from_slice(&v4.octets());
        }
        IpAddr::V6(v6) => {
            out.push(ATYP_IPV6);
            out.extend_from_slice(&v6.octets());
        }
    }
    out.extend_from_slice(&addr.port().to_be_bytes());
}

/// Same header for a domain destination — the association's bound name
/// when this adapter fronts a chained UDP request (issue #657). The
/// server resolves `host` with its own view; `host.len()` was validated
/// against `MAX_DOMAIN_LEN` at dial time.
fn encode_udp_header_domain(out: &mut SmallVec<[u8; 1500]>, host: &str, port: u16) {
    out.extend_from_slice(&[0, 0, 0]); // RSV(2) + FRAG(1)
    out.push(ATYP_DOMAIN);
    out.push(u8::try_from(host.len()).expect("domain length validated at dial"));
    out.extend_from_slice(host.as_bytes());
    out.extend_from_slice(&port.to_be_bytes());
}

/// Parse a received SOCKS5 UDP datagram in place: validate the header, return
/// the source `SocketAddr`, and shift the DATA portion to the front of `buf`.
/// Returns `(data_len, src)`.
fn decode_udp_datagram(buf: &mut [u8], n: usize) -> Result<(usize, SocketAddr)> {
    if n < 4 {
        return Err(MeowError::Proxy("socks5: short UDP datagram".into()));
    }
    // RSV(2) ignored. FRAG must be 0 — we don't reassemble fragments.
    if buf[2] != 0 {
        return Err(MeowError::Proxy(
            "socks5: fragmented UDP datagram not supported".into(),
        ));
    }
    let (src, header_len) = match buf[3] {
        ATYP_IPV4 => {
            if n < 10 {
                return Err(MeowError::Proxy("socks5: truncated v4 UDP header".into()));
            }
            let ip = IpAddr::from([buf[4], buf[5], buf[6], buf[7]]);
            (
                SocketAddr::new(ip, u16::from_be_bytes([buf[8], buf[9]])),
                10,
            )
        }
        ATYP_IPV6 => {
            if n < 22 {
                return Err(MeowError::Proxy("socks5: truncated v6 UDP header".into()));
            }
            let mut o = [0u8; 16];
            o.copy_from_slice(&buf[4..20]);
            (
                SocketAddr::new(IpAddr::from(o), u16::from_be_bytes([buf[20], buf[21]])),
                22,
            )
        }
        // Domain-form source in a reply is degenerate (servers echo the IP).
        other => {
            return Err(MeowError::Proxy(format!(
                "socks5: unsupported UDP reply atyp {other:#04x}"
            )));
        }
    };
    let data_len = n - header_len;
    buf.copy_within(header_len..n, 0);
    Ok((data_len, src))
}

// Accept domain replies only when they match the association's remote name.
fn decode_udp_datagram_bound(buf: &mut [u8], n: usize, bound: Option<&(SmolStr, u16)>) -> Result<(usize, SocketAddr)> {
    if n < 5 || buf[3] != ATYP_DOMAIN { return decode_udp_datagram(buf, n); }
    if buf[..3] != [0, 0, 0] { return Err(MeowError::Proxy("socks5: invalid UDP flags".into())); }
    let len = buf[4] as usize;
    let header_len = 7 + len;
    if len == 0 || n < header_len { return Err(MeowError::Proxy("socks5: truncated domain UDP header".into())); }
    let name = std::str::from_utf8(&buf[5..5 + len]).map_err(|_| MeowError::Proxy("socks5: invalid domain".into()))?;
    let port = u16::from_be_bytes([buf[5 + len], buf[6 + len]]);
    if !bound.is_some_and(|(host, expected)| host.eq_ignore_ascii_case(name) && *expected == port) {
        return Err(MeowError::Proxy("socks5: unexpected domain UDP reply".into()));
    }
    buf.copy_within(header_len..n, 0);
    Ok((n - header_len, SocketAddr::from(([0, 0, 0, 0], port))))
}

// ─── ProxyAdapter ─────────────────────────────────────────────────────────────

#[async_trait]
impl ProxyAdapter for Socks5Adapter {
    fn name(&self) -> &str {
        &self.name
    }

    fn adapter_type(&self) -> AdapterType {
        AdapterType::Socks5
    }

    fn addr(&self) -> &str {
        &self.addr_str
    }

    fn support_udp(&self) -> bool {
        // The UDP-ASSOCIATE datagrams ride the front proxy's own `dial_udp`
        // under `dialer-proxy` (mihomo `proxyDialer.ListenPacket`), so a
        // proxy dialer is fine *when the front advertises UDP* — a
        // point-in-time snapshot for group fronts; `dial_udp` re-checks at
        // dial time and fails closed on a front that cannot carry UDP.
        //
        // Two consumers read this advertisement: the rule probe treats a
        // UDP flow matched to a `!support_udp` target as ineligible and
        // skips to the next rule (tunnel.rs `RouteTargetProbe`), and
        // LoadBalance filters members by it.  It is NOT the enforcement
        // point — `dial_udp` refuses on its own, so a stale optimistic
        // snapshot cannot open a raw socket.
        self.udp && (!self.dialer.is_proxy() || self.dialer.supports_udp())
    }

    async fn dial_tcp(&self, metadata: &Metadata) -> Result<Box<dyn ProxyConn>> {
        debug!(
            "socks5: CONNECT {}:{} via {}:{}",
            metadata.host, metadata.dst_port, self.server, self.port
        );

        let mut stream = self.dial_stream(metadata.is_internal()).await?;
        self.run_handshake(
            &mut stream,
            &metadata.host,
            metadata.dst_ip,
            metadata.dst_port,
        )
        .await?;
        Ok(Box::new(StreamConn(stream)))
    }

    async fn dial_udp(&self, metadata: &Metadata) -> Result<Box<dyn ProxyPacketConn>> {
        if !self.udp {
            return Err(MeowError::NotSupported(
                "socks5: UDP ASSOCIATE not enabled (set `udp: true`)".into(),
            ));
        }

        // Fail-closed (Class A, ADR-0002): a front that cannot carry UDP
        // refuses here — *before* the control connection is spent — with
        // `NotSupported`, the capability-refusal class exempt from group
        // dead-marking.  The raw-socket path below stays unreachable under
        // `dialer-proxy`: no silent real-source egress.  Enforced here
        // because the tunnel's UDP dispatch calls `dial_udp` without
        // consulting `support_udp`.
        if self.dialer.is_proxy() && !self.dialer.supports_udp() {
            return Err(MeowError::NotSupported(format!(
                "socks5: `dialer-proxy` front cannot carry UDP (unsupported \
                 or unresolvable); refusing to leak the real source path ({})",
                self.addr_str
            )));
        }

        // This adapter as a *front*: a host-only UDP destination (set by
        // the dialer layer for a chained `UdpTarget::Name`, issue #657)
        // binds the association to the name — every datagram's SOCKS5 DST
        // field carries the domain, and the server resolves it with its
        // own view. The write arg becomes advisory.
        let write_target = match metadata.domain_udp_target() {
            Some((host, port)) if host.len() <= MAX_DOMAIN_LEN => Some((host.clone(), port)),
            Some(_) => {
                return Err(MeowError::NotSupported(
                    "socks5: domain UDP target exceeds ATYP_DOMAIN length".into(),
                ))
            }
            None => None,
        };

        // The UDP association is bound to the lifetime of this TCP control
        // connection (RFC 1928 §7): the server tears the association down when
        // the control conn closes. We keep it open via `ControlGuard`.
        let mut control = self.dial_stream(metadata.is_internal()).await?;
        let relay = self.run_udp_associate(&mut control).await?;
        debug!(
            "socks5: UDP ASSOCIATE via {}:{} → relay {:?}",
            self.server, self.port, relay
        );

        // mihomo `proxyDialer.ListenPacket`: under `dialer-proxy` the relay
        // datagrams ride the front proxy's own `dial_udp` association bound
        // to the advertised relay endpoint — the SOCKS5 UDP header still
        // wraps the per-packet destinations, the front only sees `relay`.
        // Wildcard/domain BND forms dial as `UdpTarget::Name` so the front
        // resolves them itself; a front that cannot carry a domain refuses
        // with `Unsupported` and we fall back to a local resolution —
        // the pre-#657 behavior, still fail-closed.
        if self.dialer.is_proxy() {
            let target = relay.to_udp_target(&self.server);
            let internal = metadata.is_internal();
            // The dialer maps front `NotSupported`/`UdpNotSupported` to
            // `ErrorKind::Unsupported` — a capability refusal, not a
            // transport failure: keep the class so the node is not
            // dead-marked over a UDP-less front.
            let map_err = |target: &UdpTarget, e: std::io::Error| {
                if e.kind() == std::io::ErrorKind::Unsupported {
                    MeowError::NotSupported(format!("socks5 udp via dialer-proxy {target}: {e}"))
                } else {
                    MeowError::Io(e)
                }
            };
            let (conn, bound) = match self.dialer.dial_udp_conn(target.clone(), internal).await {
                Ok(conn) => (conn, target),
                Err(e)
                    if e.kind() == std::io::ErrorKind::Unsupported
                        && matches!(target, UdpTarget::Name { .. }) =>
                {
                    // The front cannot carry a domain target — resolve
                    // locally and redial the literal (pre-#657 behavior,
                    // still fail-closed through the same chain).
                    let addr = self.resolve_relay(&relay).await?;
                    match self
                        .dialer
                        .dial_udp_conn(UdpTarget::Addr(addr), internal)
                        .await
                    {
                        Ok(conn) => (conn, UdpTarget::Addr(addr)),
                        Err(e) => return Err(map_err(&UdpTarget::Addr(addr), e)),
                    }
                }
                Err(e) => return Err(map_err(&target, e)),
            };
            debug!("socks5: UDP chained to relay {bound} via {}", self.addr_str);
            return Ok(Box::new(Socks5UdpConn {
                transport: Socks5UdpTransport::Chained(conn),
                relay: bound,
                write_target,
                _control: ControlGuard::spawn(control),
            }));
        }

        let relay_addr = self.resolve_relay(&relay).await?;
        let bind: SocketAddr = if relay_addr.is_ipv4() {
            "0.0.0.0:0".parse().expect("static")
        } else {
            "[::]:0".parse().expect("static")
        };
        let socket = meow_common::bind_udp(bind).await.map_err(MeowError::Io)?;
        socket.connect(relay_addr).await.map_err(MeowError::Io)?;

        Ok(Box::new(Socks5UdpConn {
            transport: Socks5UdpTransport::Raw(socket),
            relay: UdpTarget::Addr(relay_addr),
            write_target,
            _control: ControlGuard::spawn(control),
        }))
    }

    /// Run the SOCKS5 handshake over an already-established stream.
    ///
    /// The stream already terminates at this SOCKS5 server — it carries
    /// whichever outer transport the relay's preceding hop established, so
    /// the adapter's own TLS layer (when `tls: true`) still applies before
    /// the SOCKS5 negotiation.
    ///
    /// upstream: `adapter/outbound/socks5.go` — `DialContextWithDialer`
    async fn connect_over(
        &self,
        stream: Box<dyn ProxyConn>,
        metadata: &Metadata,
    ) -> Result<Box<dyn ProxyConn>> {
        debug!(
            "socks5: CONNECT (relay) {}:{} over existing stream",
            metadata.host, metadata.dst_port
        );
        let mut stream = self.wrap_tls(Box::new(stream)).await?;
        self.run_handshake(
            &mut stream,
            &metadata.host,
            metadata.dst_ip,
            metadata.dst_port,
        )
        .await?;
        Ok(Box::new(StreamConn(stream)))
    }

    fn health(&self) -> &ProxyHealth {
        &self.health
    }
}

// ─── UDP ASSOCIATE relay conn ──────────────────────────────────────────────────

/// Keeps the SOCKS5 UDP-ASSOCIATE control connection open for the lifetime of
/// the relay. RFC 1928 §7 ties the association to the control conn, so the
/// task holds it and drains anything the server sends; dropping the guard
/// aborts the task, closing the conn and ending the association.
struct ControlGuard(tokio::task::AbortHandle);

impl ControlGuard {
    fn spawn(control: Box<dyn meow_transport::Stream>) -> Self {
        let handle = tokio::spawn(async move {
            let mut control = control;
            let mut sink = [0u8; 16];
            // Block until the proxy closes the control conn (or we're aborted).
            loop {
                match control.read(&mut sink).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {} // ignore unexpected control-channel bytes
                }
            }
        });
        ControlGuard(handle.abort_handle())
    }
}

impl Drop for ControlGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A SOCKS5 UDP-ASSOCIATE relay socket. Each datagram is wrapped/unwrapped in
/// the RFC 1928 §7 UDP request header; the transport is `connect()`ed to the
/// proxy's relay endpoint — a raw socket directly, or the front proxy's own
/// `dial_udp` association under `dialer-proxy`.
enum Socks5UdpTransport {
    Raw(UdpSocket),
    Chained(Arc<dyn ProxyPacketConn>),
}

struct Socks5UdpConn {
    transport: Socks5UdpTransport,
    /// The advertised relay endpoint: chained datagrams are written to it
    /// and chained reads are filtered against it (connected-socket parity).
    /// A [`UdpTarget::Name`] is bound by the front — reads then match on
    /// port only, since the wire source legitimately differs from any
    /// local resolution of the name (issue #657).
    relay: UdpTarget,
    /// Set when this adapter fronts a domain-carrying association request:
    /// every datagram's SOCKS5 DST field stamps the name instead of the
    /// caller's addr arg.
    write_target: Option<(SmolStr, u16)>,
    _control: ControlGuard,
}

#[async_trait]
impl ProxyPacketConn for Socks5UdpConn {
    async fn read_packet(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr)> {
        // A malformed relay datagram is per-datagram: it is already
        // consumed from the transport, so skip it instead of letting one
        // junk packet kill the whole association (mihomo propagates the
        // decode error and tears the session down; dropping is strictly
        // more resilient and matches this crate's SS arm).
        loop {
            let n = match &self.transport {
                Socks5UdpTransport::Raw(socket) => socket.recv(buf).await.map_err(MeowError::Io)?,
                Socks5UdpTransport::Chained(conn) => {
                    let (rn, outer_src) = conn.read_packet(buf).await?;
                    // Per-packet fronts report the real wire source and can
                    // deliver datagrams from arbitrary remotes — restore the
                    // connected-socket filter the Raw arm has.  Bound conns
                    // report `relay` or an unspecified addr (no source info);
                    // unspecified skips the check rather than breaking them.
                    // A `Name` relay was resolved by the front: only the port
                    // is comparable, since the wire source legitimately
                    // differs from any local resolution.
                    if !self.relay.src_matches(outer_src)
                        && !outer_src.ip().to_canonical().is_unspecified()
                    {
                        debug!(
                            "socks5 udp: dropped chained datagram from {outer_src} (expected {})",
                            self.relay
                        );
                        continue;
                    }
                    rn
                }
            };
            match decode_udp_datagram_bound(buf, n, self.write_target.as_ref()) {
                Ok(ok) => return Ok(ok),
                Err(e) => debug!("socks5 udp: dropping malformed relay datagram: {e}"),
            }
        }
    }

    async fn write_packet(&self, data: &[u8], addr: &SocketAddr) -> Result<usize> {
        let mut pkt: SmallVec<[u8; 1500]> = SmallVec::new();
        if let Some((host, port)) = &self.write_target {
            encode_udp_header_domain(&mut pkt, host, *port);
        } else {
            encode_udp_header(&mut pkt, addr);
        }
        pkt.extend_from_slice(data);
        match &self.transport {
            Socks5UdpTransport::Raw(socket) => {
                socket.send(&pkt).await.map_err(MeowError::Io)?;
            }
            Socks5UdpTransport::Chained(conn) => {
                // The wire destination is always the relay endpoint — the
                // caller's `addr` already went inside the SOCKS5 header.
                // For a `Name` relay the write dst is an advisory
                // placeholder; the name-bound conn stamps its own target.
                // Reject frames a stream-framed front could not carry
                // (u16 length prefix) and verify datagram atomicity.
                if pkt.len() > u16::MAX as usize {
                    return Err(MeowError::Proxy(format!(
                        "socks5 udp: {}B datagram exceeds the u16 frame limit",
                        pkt.len()
                    )));
                }
                let n = conn.write_packet(&pkt, &self.relay.write_dst()).await?;
                if n != pkt.len() {
                    return Err(MeowError::Proxy(format!(
                        "socks5 udp: short write on chained conn ({n} < {})",
                        pkt.len()
                    )));
                }
            }
        }
        Ok(data.len())
    }

    fn local_addr(&self) -> Result<SocketAddr> {
        match &self.transport {
            Socks5UdpTransport::Raw(socket) => socket.local_addr().map_err(MeowError::Io),
            Socks5UdpTransport::Chained(conn) => conn.local_addr(),
        }
    }

    fn close(&self) -> Result<()> {
        // Closing the control conn ends the association server-side
        // (RFC 1928 §7) — mihomo's `packetConn.Close()` does the same.
        self._control.0.abort();
        match &self.transport {
            Socks5UdpTransport::Chained(conn) => conn.close(),
            Socks5UdpTransport::Raw(_) => Ok(()),
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "test/socks5_adapter_test.rs"]
mod tests;
