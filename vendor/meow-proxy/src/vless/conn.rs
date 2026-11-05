//! `VlessConn` — TCP passthrough connection wrapping a transport-layer stream.
//!
//! After construction (`new().await`), the VLESS request header has been sent and
//! the VLESS response header has been read and validated.  Subsequent `AsyncRead`
//! and `AsyncWrite` calls pass through to the underlying stream directly.
//!
//! `VlessPacketConn` is the UDP-over-TCP variant.  Each datagram is framed with
//! a 2-byte big-endian length prefix (standard VLESS UDP encoding).

use std::io;
use std::pin::Pin;
use std::task::{ready, Context, Poll};

use bytes::{BufMut, Bytes, BytesMut};
use meow_common::{MeowError, ProxyConn, ProxyPacketConn, Result};
use meow_transport::Stream;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};

#[cfg(feature = "mux")]
use super::header::encode_mux_request;
use super::header::{encode_request, Cmd, VlessAddr};

// ─── VlessConn ────────────────────────────────────────────────────────────────

/// A VLESS TCP connection — request header sent, response header consumed lazily.
///
/// The response header is read on the first `poll_read` call, not during
/// construction. This matches upstream xray-core behavior where the server
/// only sends the response after connecting to the destination.
pub struct VlessConn {
    pub(crate) inner: Box<dyn Stream>,
    /// `true` until the 2-byte response header has been consumed.
    pub(crate) response_pending: bool,
    /// Partial response-header read progress.  Cancel-safe: a Pending
    /// between the two header bytes must not lose the byte already
    /// consumed (a local buffer would shift the whole stream by one byte).
    pub(crate) response_buf: [u8; 2],
    pub(crate) response_pos: usize,
    /// Scratch buffer for the response addon bytes (usually empty).
    pub(crate) response_addon: Vec<u8>,
    /// Pre-encoded request header, written ahead of the first payload.
    /// `Some` only for deferred-header connections (Vision): the request
    /// must ride inside the first Vision-padded record, matching
    /// mihomo/xray, so it is emitted on the first write instead of during
    /// construction.
    pub(crate) header_pending: Option<Bytes>,
    /// Set once the deferred header has reached the inner stream and cleared
    /// after the next flush.  A read-first protocol must flush the request
    /// before waiting for the server response.
    pub(crate) header_needs_flush: bool,
}

impl VlessConn {
    /// Establish a VLESS connection:
    /// 1. Write the request header.
    /// 2. Return immediately — response header is read lazily on first read.
    pub async fn new(
        mut stream: Box<dyn Stream>,
        uuid_bytes: &[u8; 16],
        flow: Option<&str>,
        cmd: Cmd,
        dst_port: u16,
        addr: &VlessAddr,
    ) -> Result<Self> {
        // Write the VLESS request header.
        let mut buf = BytesMut::new();
        encode_request(&mut buf, uuid_bytes, flow, cmd, dst_port, addr);
        tracing::debug!("VLESS: writing {} byte request header", buf.len());
        stream.write_all(&buf).await.map_err(MeowError::Io)?;
        stream.flush().await.map_err(MeowError::Io)?;
        tracing::debug!("VLESS: request header sent, response will be read lazily");

        Ok(Self {
            inner: stream,
            response_pending: true,
            response_buf: [0; 2],
            response_pos: 0,
            response_addon: Vec::new(),
            header_pending: None,
            header_needs_flush: false,
        })
    }

    /// Like [`Self::new`], but defers the request header to the first
    /// write so a wrapping Vision layer can pad it together with the first
    /// payload (xray expects the request inside the first Vision record).
    #[cfg(any(feature = "mux", feature = "vless-vision"))]
    pub async fn new_deferred(
        stream: Box<dyn Stream>,
        uuid_bytes: &[u8; 16],
        flow: Option<&str>,
        cmd: Cmd,
        dst_port: u16,
        addr: &VlessAddr,
    ) -> Result<Self> {
        let mut buf = BytesMut::new();
        encode_request(&mut buf, uuid_bytes, flow, cmd, dst_port, addr);
        Ok(Self {
            inner: stream,
            response_pending: true,
            response_buf: [0; 2],
            response_pos: 0,
            response_addon: Vec::new(),
            header_pending: Some(buf.freeze()),
            header_needs_flush: false,
        })
    }

    /// Establish a VLESS Mux.Cool session connection: the request header
    /// carries `Cmd::Mux` (0x03) and no address — the per-stream targets
    /// ride Mux.Cool frames.  The response header is consumed lazily on the
    /// first read, exactly like `Self::new`: the server still answers
    /// `[version][addon_length]` before the first mux frame.
    ///
    /// Used by the Mux.Cool session dialer
    /// (`VlessAdapter::with_mux` with `protocol: muxcool`).
    #[cfg(feature = "mux")]
    pub async fn new_mux(
        mut stream: Box<dyn Stream>,
        uuid_bytes: &[u8; 16],
        flow: Option<&str>,
    ) -> Result<Self> {
        let mut buf = BytesMut::new();
        encode_mux_request(&mut buf, uuid_bytes, flow);
        stream.write_all(&buf).await.map_err(MeowError::Io)?;
        stream.flush().await.map_err(MeowError::Io)?;
        Ok(Self {
            inner: stream,
            response_pending: true,
            response_buf: [0; 2],
            response_pos: 0,
            response_addon: Vec::new(),
            header_pending: None,
            header_needs_flush: false,
        })
    }

    /// Deferred variant of `Self::new_mux` for Vision: the Mux request
    /// header rides inside the first Vision record together with the first
    /// Mux.Cool frame (xray expects the request inside the first record).
    #[cfg(feature = "mux")]
    pub async fn new_mux_deferred(
        stream: Box<dyn Stream>,
        uuid_bytes: &[u8; 16],
        flow: Option<&str>,
    ) -> Result<Self> {
        let mut buf = BytesMut::new();
        encode_mux_request(&mut buf, uuid_bytes, flow);
        Ok(Self {
            inner: stream,
            response_pending: true,
            response_buf: [0; 2],
            response_pos: 0,
            response_addon: Vec::new(),
            header_pending: Some(buf.freeze()),
            header_needs_flush: false,
        })
    }

    /// Write a deferred request header before the caller's first payload.
    /// Header-only progress may continue inside one poll; payload progress is
    /// reported immediately so `AsyncWrite` never returns `Pending` or
    /// `Ok(0)` after consuming caller bytes.
    fn poll_write_deferred(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        loop {
            let Some(header) = self.header_pending.take() else {
                return Pin::new(&mut *self.inner).poll_write(cx, buf);
            };
            let header_len = header.len();
            let mut combined = BytesMut::with_capacity(header_len + buf.len());
            combined.extend_from_slice(&header);
            combined.extend_from_slice(buf);
            match Pin::new(&mut *self.inner).poll_write(cx, &combined) {
                Poll::Pending => {
                    self.header_pending = Some(header);
                    return Poll::Pending;
                }
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Ready(Ok(0)) => {
                    self.header_pending = Some(header);
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "vless: failed to write deferred request header",
                    )));
                }
                Poll::Ready(Ok(written)) if written < header_len => {
                    self.header_pending = Some(header.slice(written..));
                }
                Poll::Ready(Ok(written)) => {
                    self.header_needs_flush = true;
                    let payload_written = written - header_len;
                    if payload_written > 0 || buf.is_empty() {
                        return Poll::Ready(Ok(payload_written));
                    }
                }
            }
        }
    }

    /// Ensure a deferred request is sent and flushed before reading or
    /// shutting down.  This is the server-first path used by SMTP, IMAP,
    /// MySQL and similar protocols that do not write application data first.
    fn poll_flush_deferred_header(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while self.header_pending.is_some() {
            let written = ready!(self.poll_write_deferred(cx, &[]))?;
            debug_assert_eq!(written, 0);
        }
        if self.header_needs_flush {
            ready!(Pin::new(&mut *self.inner).poll_flush(cx))?;
            self.header_needs_flush = false;
        }
        Poll::Ready(Ok(()))
    }

    // Called only from the Vision wrapper (vless-vision feature).
    #[cfg_attr(not(feature = "vless-vision"), allow(dead_code))]
    pub(crate) fn supports_raw_passthrough(&mut self) -> bool {
        #[cfg(feature = "vless-encryption")]
        if (&mut *self.inner)
            .as_any_mut()
            .is::<super::encryption::EncryptionStream>()
        {
            return true;
        }
        meow_transport::supports_raw_passthrough(&mut *self.inner)
    }

    #[cfg_attr(not(feature = "vless-vision"), allow(dead_code))]
    pub(crate) fn enable_raw_read_passthrough(&mut self) -> bool {
        #[cfg(feature = "vless-encryption")]
        if let Some(stream) = (&mut *self.inner)
            .as_any_mut()
            .downcast_mut::<super::encryption::EncryptionStream>()
        {
            stream.enable_raw_read();
            return true;
        }
        meow_transport::enable_raw_read_passthrough(&mut *self.inner)
    }

    #[cfg_attr(not(feature = "vless-vision"), allow(dead_code))]
    pub(crate) fn enable_raw_write_passthrough(&mut self) -> bool {
        #[cfg(feature = "vless-encryption")]
        if let Some(stream) = (&mut *self.inner)
            .as_any_mut()
            .downcast_mut::<super::encryption::EncryptionStream>()
        {
            stream.enable_raw_write();
            return true;
        }
        meow_transport::enable_raw_write_passthrough(&mut *self.inner)
    }
}

// ─── AsyncRead / AsyncWrite pass-through ──────────────────────────────────────

impl AsyncRead for VlessConn {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // Lazily consume the response header on first read.  Progress lives
        // in self (response_buf/response_pos): a Pending between the two
        // header bytes must keep the byte already consumed — a local buffer
        // would lose it and shift the whole stream by one byte.
        let this = &mut *self;
        ready!(this.poll_flush_deferred_header(cx))?;
        if this.response_pending {
            while this.response_pos < 2 {
                let pos = this.response_pos;
                let mut tmp = ReadBuf::new(&mut this.response_buf[pos..]);
                match Pin::new(&mut *this.inner).poll_read(cx, &mut tmp) {
                    Poll::Ready(Ok(())) => {
                        let n = tmp.filled().len();
                        if n == 0 {
                            return Poll::Ready(Err(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                "vless: server closed before response header",
                            )));
                        }
                        this.response_pos += n;
                    }
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Pending => return Poll::Pending,
                }
            }
            let version = this.response_buf[0];
            let addon_length = this.response_buf[1] as usize;
            if version != 0x00 {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("vless: version mismatch: expected 0x00, got {version:#04x}"),
                )));
            }
            // Discard addon bytes if any — also cancel-safe: progress
            // counts against the persistent response_pos, and the scratch
            // buffer survives Pending.
            if addon_length > 0 {
                if this.response_addon.len() != addon_length {
                    this.response_addon = vec![0u8; addon_length];
                }
                while this.response_pos - 2 < addon_length {
                    let pos = this.response_pos - 2;
                    let mut tmp = ReadBuf::new(&mut this.response_addon[pos..]);
                    match Pin::new(&mut *this.inner).poll_read(cx, &mut tmp) {
                        Poll::Ready(Ok(())) => {
                            let n = tmp.filled().len();
                            if n == 0 {
                                return Poll::Ready(Err(io::Error::new(
                                    io::ErrorKind::UnexpectedEof,
                                    "vless: EOF during addon discard",
                                )));
                            }
                            this.response_pos += n;
                        }
                        Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                        Poll::Pending => return Poll::Pending,
                    }
                }
            }
            this.response_pending = false;
            this.response_addon = Vec::new();
            tracing::debug!("VLESS: response header consumed lazily");
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for VlessConn {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.poll_write_deferred(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(this.poll_flush_deferred_header(cx))?;
        Pin::new(&mut *this.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(this.poll_flush_deferred_header(cx))?;
        Pin::new(&mut *this.inner).poll_shutdown(cx)
    }
}

impl Unpin for VlessConn {}

impl ProxyConn for VlessConn {}

// ─── VlessPacketConn (UDP-over-TCP) ──────────────────────────────────────────

/// A VLESS UDP connection over TCP.
///
/// Each datagram is framed as `u16_be(length) + data`.  The VLESS request
/// header (cmd=0x02) is sent on construction; the response header is
/// consumed **lazily** on the first read — xray/sing-vmess only write it
/// together with the first reply datagram, so eager consumption during
/// construction would deadlock (server waits for the first datagram before
/// answering, client waits for the answer before sending it).
///
/// The stream is split into independent read/write halves, each behind its
/// own `Mutex`, so a `read_packet` parked waiting for a server datagram never
/// blocks `write_packet` on the same connection (issue #278). The tunnel
/// drives the two directions from separate tasks; with a single stream-wide
/// lock any flow needing interleaved sends while a recv is pending (QUIC,
/// WireGuard, games) stalled after the first packet.
///
/// The read side cannot desync: `VlessUdpReader` owns its progress. The
/// write side can — a `write_packet` future dropped mid-`write_all` has a
/// frame prefix on the wire and the peer parses the next frame's header as
/// payload — so an incomplete write poisons the conn and every later op
/// fails fast (issue #514).
pub struct VlessPacketConn {
    reader: tokio::sync::Mutex<VlessUdpReader>,
    writer: tokio::sync::Mutex<tokio::io::WriteHalf<Box<dyn Stream>>>,
    poisoned: std::sync::atomic::AtomicBool,
}

/// Cancel-safe UDP reader.  The response header, the per-datagram length
/// prefix and the payload all persist their read progress across dropped
/// `read_packet` futures — a lost half-read would desynchronise the
/// UDP-over-TCP framing forever (callers may still cancel a read by
/// dropping the future, e.g. task abort).
struct VlessUdpReader {
    stream: tokio::io::ReadHalf<Box<dyn Stream>>,
    /// VLESS response header `[version][addon_length]`; `hdr_pos < 2`
    /// means the response phase is still active.
    hdr: [u8; 2],
    hdr_pos: usize,
    /// True once the response header (and any addon bytes) were consumed.
    header_done: bool,
    /// Per-datagram length prefix, big-endian.
    len: [u8; 2],
    len_pos: usize,
    /// Datagram payload (or the response addon bytes mid-discard).
    payload: Vec<u8>,
    payload_pos: usize,
}

impl VlessUdpReader {
    fn new(stream: tokio::io::ReadHalf<Box<dyn Stream>>) -> Self {
        Self {
            stream,
            hdr: [0; 2],
            hdr_pos: 0,
            header_done: false,
            len: [0; 2],
            len_pos: 0,
            payload: Vec::new(),
            payload_pos: 0,
        }
    }

    /// Reset the per-datagram state for the next packet (the response
    /// header is read exactly once and stays consumed).
    fn reset_datagram(&mut self) {
        self.len_pos = 0;
        self.payload.clear();
        self.payload_pos = 0;
    }

    async fn read_packet(&mut self, buf: &mut [u8]) -> Result<(usize, std::net::SocketAddr)> {
        use tokio::io::AsyncReadExt;
        // Phase 1: response header [version][addon_length], consumed
        // lazily on the first read (it rides with the first reply
        // datagram).  Progress persists so a cancelled read resumes
        // instead of shifting the stream by the lost bytes.
        if !self.header_done {
            while self.hdr_pos < self.hdr.len() {
                let n = self
                    .stream
                    .read(&mut self.hdr[self.hdr_pos..])
                    .await
                    .map_err(MeowError::Io)?;
                if n == 0 {
                    return Err(MeowError::Proxy(
                        "vless: server closed after header — check UUID and server config".into(),
                    ));
                }
                self.hdr_pos += n;
            }
            let version = self.hdr[0];
            let addon_length = self.hdr[1] as usize;
            if version != 0x00 {
                // upstream: closes silently. NOT silent — we surface the reason.
                tracing::warn!(
                    version = %format!("{:#04x}", version),
                    "vless: response version mismatch (expected 0x00) — \
                     check UUID and whether a TLS layer is required"
                );
                return Err(MeowError::Proxy(format!(
                    "vless: version mismatch: expected 0x00, got {version:#04x}"
                )));
            }
            if addon_length > 0 {
                // Discard the addon bytes (UDP sends none; keep the same
                // tolerance as the TCP path) with persistent progress.
                // Only reset when the size changes: a cancelled
                // `read_packet` future re-enters here with `header_done`
                // still false, and resetting `payload_pos` to 0 would
                // re-read `addon_length` bytes, eating the head of the
                // first datagram and desyncing the UDP framing.
                if self.payload.len() != addon_length {
                    self.payload.resize(addon_length, 0);
                    self.payload_pos = 0;
                }
                while self.payload_pos < self.payload.len() {
                    let n = self
                        .stream
                        .read(&mut self.payload[self.payload_pos..])
                        .await
                        .map_err(MeowError::Io)?;
                    if n == 0 {
                        return Err(MeowError::Proxy(
                            "vless: server closed mid response-header addon".into(),
                        ));
                    }
                    self.payload_pos += n;
                }
                self.payload.clear();
                self.payload_pos = 0;
            }
            self.header_done = true;
        }
        // Phase 2: per-datagram `u16_be(len)`.
        while self.len_pos < self.len.len() {
            let n = self
                .stream
                .read(&mut self.len[self.len_pos..])
                .await
                .map_err(MeowError::Io)?;
            if n == 0 {
                return Err(MeowError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "vless: EOF while reading UDP packet length",
                )));
            }
            self.len_pos += n;
        }
        // Phase 3: payload (sized once per datagram; partial progress
        // survives cancellation).
        if self.payload.is_empty() && self.payload_pos == 0 {
            self.payload
                .resize(u16::from_be_bytes(self.len) as usize, 0);
        }
        while self.payload_pos < self.payload.len() {
            let n = self
                .stream
                .read(&mut self.payload[self.payload_pos..])
                .await
                .map_err(MeowError::Io)?;
            if n == 0 {
                return Err(MeowError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "vless: EOF while reading UDP packet payload",
                )));
            }
            self.payload_pos += n;
        }
        let packet_len = self.payload.len();
        if packet_len > buf.len() {
            // The oversized payload was fully drained into the internal
            // buffer, so the stream framing stays aligned for the next
            // read — surface the error only after the drain.
            self.reset_datagram();
            return Err(MeowError::Proxy(format!(
                "vless: UDP packet ({packet_len} bytes) exceeds read buffer ({} bytes)",
                buf.len()
            )));
        }
        buf[..packet_len].copy_from_slice(&self.payload);
        self.reset_datagram();
        // Return a placeholder source address (connection-oriented
        // UDP-over-TCP has no per-datagram source addr).
        let placeholder: std::net::SocketAddr = "0.0.0.0:0".parse().unwrap();
        Ok((packet_len, placeholder))
    }
}

impl VlessPacketConn {
    /// Create a UDP-over-TCP VLESS connection.
    pub async fn new(
        mut stream: Box<dyn Stream>,
        uuid_bytes: &[u8; 16],
        dst_port: u16,
        addr: &VlessAddr,
    ) -> Result<Self> {
        // Write request header with Cmd::Udp.
        let mut buf = BytesMut::new();
        encode_request(&mut buf, uuid_bytes, None, Cmd::Udp, dst_port, addr);
        stream.write_all(&buf).await.map_err(MeowError::Io)?;
        stream.flush().await.map_err(MeowError::Io)?;

        let (reader, writer) = tokio::io::split(stream);
        Ok(Self {
            reader: tokio::sync::Mutex::new(VlessUdpReader::new(reader)),
            writer: tokio::sync::Mutex::new(writer),
            poisoned: std::sync::atomic::AtomicBool::new(false),
        })
    }
}

#[async_trait::async_trait]
impl ProxyPacketConn for VlessPacketConn {
    /// Write a UDP packet as `u16_be(len) + data`.
    async fn write_packet(&self, buf: &[u8], _addr: &std::net::SocketAddr) -> Result<usize> {
        crate::check_not_desynced(&self.poisoned)?;
        // A `buf.len() > u16::MAX` cast would wrap the length field and
        // permanently desync the stream framing — reject like the trojan,
        // mux, anytls and snell datagram writers already do.
        if buf.len() > u16::MAX as usize {
            return Err(MeowError::Proxy(format!(
                "vless udp: datagram {}B exceeds u16 frame limit",
                buf.len()
            )));
        }
        let mut writer = self.writer.lock().await;
        // Re-check after the lock: a write parked behind one cancelled
        // mid-frame must not append after the torn frame.
        crate::check_not_desynced(&self.poisoned)?;
        let mut frame = BytesMut::with_capacity(2 + buf.len());
        frame.put_u16(buf.len() as u16);
        frame.put_slice(buf);
        let mut guard = crate::PoisonOnIncomplete::new(&self.poisoned);
        writer.write_all(&frame).await.map_err(MeowError::Io)?;
        // Fully buffered once `write_all` returns; a cancelled `flush`
        // cannot tear framing, so it runs unguarded.
        guard.complete = true;
        writer.flush().await.map_err(MeowError::Io)?;
        Ok(buf.len())
    }

    /// Read a UDP packet: consume `u16_be(len)` then `len` bytes.  The
    /// VLESS response header is consumed lazily on the first read (it rides
    /// with the first reply datagram).  All progress is persistent, so a
    /// cancelled future resumes from where it stopped.
    async fn read_packet(&self, buf: &mut [u8]) -> Result<(usize, std::net::SocketAddr)> {
        // No post-lock re-check needed: this reader never loses bytes, so
        // the poison can only come from the write side — and a conn with a
        // torn uplink is dead regardless of what the peer still sends.
        crate::check_not_desynced(&self.poisoned)?;
        let mut reader = self.reader.lock().await;
        reader.read_packet(buf).await
    }

    fn local_addr(&self) -> Result<std::net::SocketAddr> {
        Err(MeowError::NotSupported(
            "VlessPacketConn has no local UDP addr (UDP-over-TCP)".into(),
        ))
    }

    fn close(&self) -> Result<()> {
        // Stream closes when VlessPacketConn is dropped.
        Ok(())
    }
}

// ─── Unit tests (§F) ─────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "../test/vless_conn_test.rs"]
mod tests;
