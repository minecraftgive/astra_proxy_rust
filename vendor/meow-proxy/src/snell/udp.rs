//! Snell UDP-over-TCP framing.
//!
//! Port of opensnell `components/snell/udp.go`. Each datagram is sent as a
//! single snell AEAD frame whose body is
//! `[CommandUDPForward=0x01][addr][payload]`. The address encoding mirrors
//! SOCKS5 except IPv6 is signaled by `0x06` (not 0x04 of SOCKS5).
//!
//! Server → client frames use a slightly different address layout:
//! `[0x04|0x06][ip-bytes][port:u16 BE][payload]`. The `read_packet` parser
//! handles both ipv4 (`0x04`) and ipv6 (`0x06`); domain-name replies are not
//! emitted by official servers.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use async_trait::async_trait;
use meow_common::{MeowError, ProxyPacketConn, Result};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Mutex;

use super::protocol::{Snell, COMMAND_UDP_FORWARD};

/// Build the `[CommandUDPForward][addr-encoding][payload]` frame payload for a
/// snell UDP request.
fn build_request_frame(addr: &SocketAddr, payload: &[u8]) -> Vec<u8> {
    // Header is encoded as if the client always sent an IP target; that
    // matches what opensnell's PacketConn does after the DNS resolve
    // shortcut in the SOCKS5 path.
    let mut buf = Vec::with_capacity(1 + 1 + 16 + 2 + payload.len());
    buf.push(COMMAND_UDP_FORWARD);
    // host-length 0 means "address follows as raw IP" with a one-byte family
    // marker.
    buf.push(0);
    match addr.ip() {
        IpAddr::V4(v4) => {
            buf.push(0x04);
            buf.extend_from_slice(&v4.octets());
        }
        IpAddr::V6(v6) => {
            buf.push(0x06);
            buf.extend_from_slice(&v6.octets());
        }
    }
    buf.extend_from_slice(&addr.port().to_be_bytes());
    buf.extend_from_slice(payload);
    buf
}

/// Domain form of [`build_request_frame`] — `host-length > 0` carries the
/// hostname bytes so the server resolves the target itself. Used when this
/// adapter fronts a chained `UdpTarget::Name` (issue #657). `host` must be
/// ≤255 bytes (the length prefix is a u8).
fn build_request_frame_domain(host: &str, port: u16, payload: &[u8]) -> Vec<u8> {
    let host_bytes = host.as_bytes();
    debug_assert!(
        !host_bytes.is_empty() && host_bytes.len() <= u8::MAX as usize,
        "domain length checked by caller"
    );
    let mut buf = Vec::with_capacity(1 + 1 + host_bytes.len() + 2 + payload.len());
    buf.push(COMMAND_UDP_FORWARD);
    buf.push(host_bytes.len() as u8);
    buf.extend_from_slice(host_bytes);
    buf.extend_from_slice(&port.to_be_bytes());
    buf.extend_from_slice(payload);
    buf
}

/// Parse a server-to-client snell UDP response frame, writing the payload
/// into `out` and returning (bytes copied, source address).
fn parse_response_frame(frame: &[u8], out: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
    if frame.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "snell udp: empty response frame",
        ));
    }

    let (addr, payload_start) = match frame[0] {
        0x04 => {
            const HEAD_LEN: usize = 1 + 4 + 2;
            if frame.len() < HEAD_LEN {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "snell udp: short IPv4 response frame",
                ));
            }
            let ip = [frame[1], frame[2], frame[3], frame[4]];
            let port = [frame[5], frame[6]];
            (
                SocketAddr::new(IpAddr::V4(Ipv4Addr::from(ip)), u16::from_be_bytes(port)),
                HEAD_LEN,
            )
        }
        0x06 => {
            const HEAD_LEN: usize = 1 + 16 + 2;
            if frame.len() < HEAD_LEN {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "snell udp: short IPv6 response frame",
                ));
            }
            let mut ip = [0u8; 16];
            ip.copy_from_slice(&frame[1..17]);
            let port = [frame[17], frame[18]];
            // Dual-stack servers (v6) report IPv4 sources as `::ffff:a.b.c.d`;
            // unmap them so replies match the IPv4 target the caller sent to.
            (
                SocketAddr::new(Ipv6Addr::from(ip).to_canonical(), u16::from_be_bytes(port)),
                HEAD_LEN,
            )
        }
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("snell udp: unknown address family 0x{other:x}"),
            ));
        }
    };

    let payload = &frame[payload_start..];
    let copied = payload.len().min(out.len());
    out[..copied].copy_from_slice(&payload[..copied]);
    Ok((copied, addr))
}

/// Per-connection snell UDP relay. Multiplexes datagrams over a single AEAD
/// stream.
///
/// The AEAD codec keeps its read and write cipher states in one object over
/// one TCP stream, so a `tokio::io::split`-style structural split is not
/// possible. Instead the stream sits behind a synchronous mutex that is
/// locked **per poll** (the same lock-per-poll pattern `tokio::io::split`
/// uses internally): a `read_packet` parked waiting for a server datagram
/// holds no stream lock between polls, so `write_packet` on the same conn
/// proceeds freely (issue #278). The `read_gate`/`write_gate` async mutexes serialise whole
/// datagrams within each direction so concurrent callers cannot interleave
/// partial frames; `read_gate` doubles as the owner of the reusable frame
/// buffer so reads allocate once per conn, not once per datagram.
///
/// A `write_packet` future dropped mid-frame would leave the AEAD stream
/// torn — v4's next write would clobber undrained pending bytes, v3's would
/// append after a half-written frame — silently desyncing every following
/// datagram. `poisoned` + `PoisonOnIncomplete` make the tear fail fast
/// instead (issue #625.3, same pattern as the trojan/vless packet conns).
pub struct SnellPacketConn<S> {
    stream: Arc<parking_lot::Mutex<Snell<S>>>,
    read_gate: Mutex<Vec<u8>>,
    write_gate: Mutex<()>,
    poisoned: AtomicBool,
    /// Largest frame one record carries (v6 allows more than v3/v4).
    max_frame: usize,
    /// A chained `UdpTarget::Name` binds every request frame's address to
    /// the domain so the server resolves it (issue #657). `None` keeps
    /// per-packet `SocketAddr` stamping.
    write_target: Option<(smol_str::SmolStr, u16)>,
}

impl<S> SnellPacketConn<S> {
    pub fn new(snell: Snell<S>) -> Self {
        Self::with_target(snell, None)
    }

    pub fn with_target(snell: Snell<S>, write_target: Option<(smol_str::SmolStr, u16)>) -> Self {
        Self {
            max_frame: snell.max_packet_frame_len(),
            stream: Arc::new(parking_lot::Mutex::new(snell)),
            read_gate: Mutex::new(Vec::new()),
            write_gate: Mutex::new(()),
            poisoned: AtomicBool::new(false),
            write_target,
        }
    }
}

#[async_trait]
impl<S> ProxyPacketConn for SnellPacketConn<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static,
{
    async fn read_packet(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr)> {
        // No post-lock re-check needed: the read side cannot tear framing —
        // a cancelled read resumes the same frame via the codec's internal
        // state — so the poison can only come from the write side, and a
        // conn with a torn uplink is dead regardless of what the peer sends.
        crate::check_not_desynced(&self.poisoned)?;
        let mut frame = self.read_gate.lock().await;
        // One decoded AEAD frame per `poll_read` ready; the frame buffer is
        // sized so a full frame always fits and never splits across reads.
        // Sized lazily on first use — conns that never receive pay nothing.
        if frame.len() < self.max_frame {
            frame.resize(self.max_frame, 0);
        }
        let n = std::future::poll_fn(|cx| {
            let mut stream = self.stream.lock();
            let mut rb = tokio::io::ReadBuf::new(frame.as_mut_slice());
            match std::pin::Pin::new(&mut *stream).poll_read(cx, &mut rb) {
                std::task::Poll::Ready(Ok(())) => std::task::Poll::Ready(Ok(rb.filled().len())),
                std::task::Poll::Ready(Err(e)) => std::task::Poll::Ready(Err(e)),
                std::task::Poll::Pending => std::task::Poll::Pending,
            }
        })
        .await
        .map_err(MeowError::Io)?;
        parse_response_frame(&frame[..n], buf).map_err(MeowError::Io)
    }

    async fn write_packet(&self, buf: &[u8], addr: &SocketAddr) -> Result<usize> {
        crate::check_not_desynced(&self.poisoned)?;
        let _serialized = self.write_gate.lock().await;
        // Re-check after the lock: a write parked behind one cancelled
        // mid-frame must not append after the torn frame.
        crate::check_not_desynced(&self.poisoned)?;
        let frame = match &self.write_target {
            Some((host, port)) => build_request_frame_domain(host, *port, buf),
            None => build_request_frame(addr, buf),
        };
        // Oversize is rejected before arming the guard — an unwritable
        // datagram must not brick an otherwise healthy conn (the codec's
        // own check inside `poll_write_packet_frame` stays as backstop).
        if frame.len() > self.max_frame {
            return Err(MeowError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "snell: packet frame too large",
            )));
        }
        let mut progress = super::protocol::PacketFrameProgress::default();
        // The guard poisons the conn if this future is dropped or returns
        // an error before completing the frame.
        let mut guard = crate::PoisonOnIncomplete::new(&self.poisoned);
        std::future::poll_fn(|cx| {
            let mut stream = self.stream.lock();
            stream.poll_write_packet_frame(cx, &frame, &mut progress)
        })
        .await
        .map_err(MeowError::Io)?;
        guard.complete = true;
        Ok(buf.len())
    }

    fn local_addr(&self) -> Result<SocketAddr> {
        // Datagrams ride on a TCP stream — no real local UDP socket exists.
        Ok(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0))
    }

    fn close(&self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
#[path = "../test/snell_udp_test.rs"]
mod tests;
