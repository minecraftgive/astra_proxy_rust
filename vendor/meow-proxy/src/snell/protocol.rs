//! Snell protocol constants and the high-level `Snell` stream wrapper.
//!
//! Bridges the version-specific AEAD codec to the snell request/response
//! semantics:
//!
//! * The client writes a 5-byte `[ver | cmd | client-id-len=0 | host-len |
//!   host... | port:u16 BE]` connect request after the salt is in flight.
//! * The server replies with a status byte (Tunnel/Pong/Error). Error
//!   responses carry `[code, msg-len, msg...]`.
//! * Either side may send a zero-payload frame to signal half-close; in
//!   reuse mode the client emits a zero chunk after each session so the
//!   connection can be returned to the pool and reused for the next request.
//! * v6 always uses the reuse-capable request and defers it into the
//!   session's first record (see [`Snell::defer_request`]).

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};

use super::v3::{is_zero_chunk as is_v3_zero_chunk, V3Conn};
use super::v4::{is_zero_chunk as is_v4_zero_chunk, V4Conn, MAX_PAYLOAD_LENGTH};
use super::v6::{V6Codec, V6Conn, MAX_RECORD_PAYLOAD as V6_MAX_RECORD_PAYLOAD};

/// First byte of every Snell request — `0x01` since v1.
pub const HEADER_VERSION: u8 = 1;

pub const COMMAND_CONNECT: u8 = 1;
/// Reuse-capable TCP connect; used when the client maintains a pool.
pub const COMMAND_CONNECT_V2: u8 = 5;
pub const COMMAND_UDP: u8 = 6;
/// First byte of each UDP-over-TCP request frame.
pub const COMMAND_UDP_FORWARD: u8 = 1;

pub const RESPONSE_TUNNEL: u8 = 0;
pub const RESPONSE_PONG: u8 = 1;
pub const RESPONSE_ERROR: u8 = 2;

/// True iff an error is a version-specific Snell zero-chunk half-close
/// (v6 reuses v4's).
pub fn is_zero_chunk(err: &io::Error) -> bool {
    is_v3_zero_chunk(err) || is_v4_zero_chunk(err)
}

/// Application-layer error returned by the snell peer.
#[derive(Debug, Clone)]
pub struct AppError {
    pub code: u8,
    pub message: String,
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "snell server error code={} msg={}",
            self.code, self.message
        )
    }
}

impl std::error::Error for AppError {}

/// Encode a TCP CONNECT request. The host is sent as text, IP literals
/// included.
pub fn connect_request(host: &str, port: u16, reuse: bool) -> io::Result<Vec<u8>> {
    if host.len() > 255 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "snell: host name too long",
        ));
    }
    let mut buf = Vec::with_capacity(5 + host.len() + 2);
    buf.push(HEADER_VERSION);
    buf.push(if reuse {
        COMMAND_CONNECT_V2
    } else {
        COMMAND_CONNECT
    });
    buf.push(0); // empty client ID
    buf.push(host.len() as u8);
    buf.extend_from_slice(host.as_bytes());
    buf.extend_from_slice(&port.to_be_bytes());
    Ok(buf)
}

/// Write a TCP CONNECT request through the caller's encrypted stream.
pub async fn write_header<W: AsyncWrite + Unpin>(
    stream: &mut W,
    host: &str,
    port: u16,
    reuse: bool,
) -> io::Result<()> {
    stream.write_all(&connect_request(host, port, reuse)?).await
}

/// UDP-ASSOCIATE request (empty client ID).
pub const UDP_REQUEST: [u8; 3] = [HEADER_VERSION, COMMAND_UDP, 0x00];

/// Write a UDP-ASSOCIATE request header.
pub async fn write_udp_header<W: AsyncWrite + Unpin>(stream: &mut W) -> io::Result<()> {
    stream.write_all(&UDP_REQUEST).await
}

/// Emit a zero-chunk (`payload_len == 0 && padding_len == 0`) — the
/// half-close signal recognized by the peer. The Snell codecs turn a
/// zero-byte `poll_write` into a zero-chunk frame, so this is a thin wrapper.
pub async fn write_zero_chunk<W: AsyncWrite + Unpin>(stream: &mut W) -> io::Result<()> {
    // A codec may first finish an in-flight payload write and report that
    // payload's consumed length. Keep polling until the empty input itself
    // completes, which is reported as zero bytes consumed.
    loop {
        if std::future::poll_fn(|cx| Pin::new(&mut *stream).poll_write(cx, &[])).await? == 0 {
            break;
        }
    }
    stream.flush().await
}

// ─── Snell stream wrapper ────────────────────────────────────────────────────

enum SnellInner<S> {
    V3(V3Conn<S>),
    V4(V4Conn<S>),
    V6(V6Conn<S>),
}

impl<S: AsyncRead + AsyncWrite + Unpin> SnellInner<S> {
    fn poll_read_inner(
        &mut self,
        cx: &mut Context<'_>,
        out: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self {
            SnellInner::V3(conn) => Pin::new(conn).poll_read(cx, out),
            SnellInner::V4(conn) => Pin::new(conn).poll_read(cx, out),
            SnellInner::V6(conn) => Pin::new(conn).poll_read(cx, out),
        }
    }

    fn poll_write_inner(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match self {
            SnellInner::V3(conn) => Pin::new(conn).poll_write(cx, buf),
            SnellInner::V4(conn) => Pin::new(conn).poll_write(cx, buf),
            SnellInner::V6(conn) => Pin::new(conn).poll_write(cx, buf),
        }
    }

    fn poll_flush_inner(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self {
            SnellInner::V3(conn) => Pin::new(conn).poll_flush(cx),
            SnellInner::V4(conn) => Pin::new(conn).poll_flush(cx),
            SnellInner::V6(conn) => Pin::new(conn).poll_flush(cx),
        }
    }

    fn poll_shutdown_inner(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self {
            SnellInner::V3(conn) => Pin::new(conn).poll_shutdown(cx),
            SnellInner::V4(conn) => Pin::new(conn).poll_shutdown(cx),
            SnellInner::V6(conn) => Pin::new(conn).poll_shutdown(cx),
        }
    }
}

/// Cross-poll state for [`Snell::poll_write_packet_frame`]. One instance per
/// datagram write; the caller keeps it alive across `Poll::Pending` returns
/// because the stream lock (and thus `&mut Snell`) is released between polls.
#[derive(Default)]
pub struct PacketFrameProgress {
    /// This progress has begun a frame write: on v4 the frame was staged
    /// into the writer's pending buffer; on v3 it marks that this progress
    /// owns the codec's staged/pending output (v3 credits `written` only
    /// after the pending bytes drain, so `written` alone cannot tell a
    /// mid-drain resume from a fresh write).
    staged: bool,
    /// v3: bytes of `frame` already accepted by the AEAD stream.
    written: usize,
}

/// AEAD-wrapped stream with snell request/response semantics.
///
/// On the first `poll_read`, the wrapper consumes the server's status byte
/// before yielding any relay bytes (`read_reply`). Subsequent reads pass
/// through directly. The wrapper exposes
/// [`Snell::poll_write_packet_frame`]/[`Snell::write_packet_frame`] so the
/// UDP relay can emit one datagram per frame and fail fast on a torn write
/// (`frame_write_torn` below).
pub struct Snell<S> {
    inner: SnellInner<S>,
    /// Set to `true` after the reply byte has been consumed once. Reset to
    /// `false` by [`Snell::reset_reply_state`] when a pooled connection is
    /// re-used for a fresh request.
    reply_consumed: bool,
    /// The current session ended with the peer's zero-chunk. Unlike a raw TCP
    /// EOF, this makes a v4/v5 connection eligible for protocol-level reuse
    /// once our own zero-chunk has also been sent.
    peer_half_closed: bool,
    /// Armed when a packet-frame write starts; cleared only when it runs to
    /// completion. A future dropped mid-write (or an errored one) leaves the
    /// AEAD stream mid-frame — a later write would append after the torn
    /// prefix (v3) or clobber undrained pending bytes (v4), desyncing every
    /// following datagram permanently. Any such attempt errors instead
    /// (issue #625 item 16; the conn-level poison for item 3 lives in
    /// `udp::SnellPacketConn`).
    frame_write_torn: bool,
}

impl<S> Snell<S> {
    pub fn from_v4(inner: V4Conn<S>) -> Self {
        Self {
            inner: SnellInner::V4(inner),
            reply_consumed: false,
            peer_half_closed: false,
            frame_write_torn: false,
        }
    }

    pub fn from_v3(inner: V3Conn<S>) -> Self {
        Self {
            inner: SnellInner::V3(inner),
            reply_consumed: false,
            peer_half_closed: false,
            frame_write_torn: false,
        }
    }

    pub fn new(inner: S, psk: Arc<[u8]>) -> Self
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        Self {
            inner: SnellInner::V4(V4Conn::new(inner, psk)),
            reply_consumed: false,
            peer_half_closed: false,
            frame_write_torn: false,
        }
    }

    pub fn new_v3(inner: S, psk: Arc<[u8]>) -> Self
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        Self {
            inner: SnellInner::V3(V3Conn::new(inner, psk)),
            reply_consumed: false,
            peer_half_closed: false,
            frame_write_torn: false,
        }
    }

    pub fn new_v6(inner: S, psk: Arc<[u8]>, codec: V6Codec) -> Self
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        Self {
            inner: SnellInner::V6(V6Conn::new(inner, psk, codec)),
            reply_consumed: false,
            peer_half_closed: false,
            frame_write_torn: false,
        }
    }

    /// v6: queue `request` to lead the next record, so it shares a record
    /// with the session's first payload bytes. Earlier versions write their
    /// request eagerly and reject this.
    pub fn defer_request(&mut self, request: Vec<u8>) -> io::Result<()> {
        match &mut self.inner {
            SnellInner::V6(conn) => {
                conn.defer_request(request);
                Ok(())
            }
            SnellInner::V3(_) | SnellInner::V4(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "snell: deferred requests are v6-only",
            )),
        }
    }

    /// Largest datagram frame the codec carries in one record.
    pub fn max_packet_frame_len(&self) -> usize {
        match &self.inner {
            SnellInner::V3(_) | SnellInner::V4(_) => MAX_PAYLOAD_LENGTH,
            SnellInner::V6(_) => V6_MAX_RECORD_PAYLOAD,
        }
    }

    /// After a successful pool reuse the next request's reply byte is
    /// pending again — reset the flag so the next `read` consumes it.
    pub fn reset_reply_state(&mut self) {
        self.reply_consumed = false;
        self.peer_half_closed = false;
    }

    pub fn peer_half_closed(&self) -> bool {
        self.peer_half_closed
    }

    /// Stage a single frame carrying `buf` verbatim as a UDP datagram
    /// payload. v4 uses its packet-frame path to keep one datagram in one
    /// frame; v3 mirrors mihomo and writes through the regular AEAD stream.
    ///
    /// Cancellation or failure mid-write tears the AEAD frame on the wire;
    /// the tear is sticky — subsequent packet-frame writes error instead of
    /// desyncing the stream (issue #625.16).
    ///
    /// An empty `buf` is a valid packet frame but on v4 also reads as a
    /// zero-chunk half-close to the peer — don't use it for keepalives.
    pub async fn write_packet_frame(&mut self, buf: &[u8]) -> io::Result<usize>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        if buf.len() > self.max_packet_frame_len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "snell: packet frame too large",
            ));
        }
        self.begin_fresh_frame_write()?;
        let result = self.write_packet_frame_inner(buf).await;
        if result.is_ok() {
            self.frame_write_torn = false;
        }
        result
    }

    /// Entry gate for a brand-new packet-frame write (not a resume of an
    /// in-progress one — `progress` itself is the resume token). A fresh
    /// write on a torn stream fails fast; otherwise the write is armed and
    /// `frame_write_torn` stays set until the frame completes.
    ///
    /// Undrained codec-pending bytes are treated as torn even if
    /// `frame_write_torn` is clear: they can only come from a write that
    /// bypassed this gate (e.g. the `AsyncWrite` passthrough), and a fresh
    /// frame would append after them / mis-credit their drain.
    fn begin_fresh_frame_write(&mut self) -> io::Result<()> {
        let pending_leftover = match &self.inner {
            SnellInner::V3(conn) => conn.has_pending_write(),
            SnellInner::V4(conn) => conn.has_pending_write(),
            SnellInner::V6(conn) => conn.has_pending_write(),
        };
        if self.frame_write_torn || pending_leftover {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "snell: packet conn desynced by an earlier incomplete frame write",
            ));
        }
        self.frame_write_torn = true;
        Ok(())
    }

    async fn write_packet_frame_inner(&mut self, buf: &[u8]) -> io::Result<usize>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        if buf.len() > self.max_packet_frame_len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "snell: packet frame too large",
            ));
        }
        match &mut self.inner {
            SnellInner::V3(conn) => {
                conn.write_all(buf).await?;
                conn.flush().await?;
            }
            SnellInner::V4(conn) => {
                conn.stage_packet_frame(buf)?;
                // poll_flush drains the staged frame AND flushes the
                // underlying stream — unconditionally, so a transport-level
                // flush that pended after `pending` emptied is finished too.
                std::future::poll_fn(|cx| Pin::new(&mut *conn).poll_flush(cx)).await?;
            }
            SnellInner::V6(conn) => {
                conn.stage_packet_frame(buf)?;
                std::future::poll_fn(|cx| Pin::new(&mut *conn).poll_flush(cx)).await?;
            }
        }
        Ok(buf.len())
    }

    /// Poll-based equivalent of [`Snell::write_packet_frame`] for callers
    /// that must not hold a lock across an `.await` (issue #278): the UDP
    /// packet conn locks the shared stream only for the duration of each
    /// poll, so `progress` carries the write state between polls.
    ///
    /// v4/v6 stage `frame` as a single packet record and drain it; v3
    /// writes through the regular AEAD stream. Both flush before returning
    /// `Ready(Ok(()))`.
    ///
    /// Contract: resuming an in-flight write must pass the *same* `frame`
    /// slice on every poll — `progress` tracks position, not content. A
    /// different slice on resume mis-credits `written` on v3; on v4 the
    /// staged frame is fixed at first poll and the new slice is ignored.
    pub fn poll_write_packet_frame(
        &mut self,
        cx: &mut Context<'_>,
        frame: &[u8],
        progress: &mut PacketFrameProgress,
    ) -> Poll<io::Result<()>>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        if frame.len() > self.max_packet_frame_len() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "snell: packet frame too large",
            )));
        }
        // `progress` doubles as the resume token: `staged`/`written` mark a
        // poll continuing an in-flight write. A *fresh* progress arriving
        // while `frame_write_torn` is set means the previous write was
        // abandoned mid-frame (the codec may still hold undrained bytes) —
        // refuse it. The conn-level poison flag already catches this in the
        // packet-conn path; this in-Snell gate covers any future direct
        // caller (issue #625.16).
        let resuming = progress.staged || progress.written > 0;
        if !resuming {
            if let Err(e) = self.begin_fresh_frame_write() {
                return Poll::Ready(Err(e));
            }
        }
        match &mut self.inner {
            SnellInner::V3(conn) => {
                // v3 credits `written` only after the staged cipher frame
                // drains, so a mid-drain resume can arrive with
                // `written == 0` — `staged` marks that this progress already
                // entered and owns the codec's pending buffer.
                progress.staged = true;
                while progress.written < frame.len() {
                    match Pin::new(&mut *conn).poll_write(cx, &frame[progress.written..]) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                        Poll::Ready(Ok(0)) => {
                            return Poll::Ready(Err(io::Error::new(
                                io::ErrorKind::WriteZero,
                                "snell v3: packet frame write returned 0",
                            )));
                        }
                        Poll::Ready(Ok(n)) => progress.written += n,
                    }
                }
                match Pin::new(conn).poll_flush(cx) {
                    Poll::Ready(Ok(())) => {
                        self.frame_write_torn = false;
                        *progress = PacketFrameProgress::default();
                        Poll::Ready(Ok(()))
                    }
                    other => other,
                }
            }
            SnellInner::V4(conn) => {
                if !progress.staged {
                    match conn.stage_packet_frame(frame) {
                        Ok(()) => progress.staged = true,
                        Err(e) => return Poll::Ready(Err(e)),
                    }
                }
                // poll_flush drains the staged frame AND flushes the
                // underlying stream — call it unconditionally so a resume
                // after `pending` emptied also finishes a transport flush
                // that pended on the previous poll.
                match Pin::new(&mut *conn).poll_flush(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Ready(Ok(())) => {}
                }
                self.frame_write_torn = false;
                *progress = PacketFrameProgress::default();
                Poll::Ready(Ok(()))
            }
            SnellInner::V6(conn) => {
                if !progress.staged {
                    match conn.stage_packet_frame(frame) {
                        Ok(()) => progress.staged = true,
                        Err(e) => return Poll::Ready(Err(e)),
                    }
                }
                // Same unconditional drain + flush as v4.
                match Pin::new(&mut *conn).poll_flush(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Ready(Ok(())) => {}
                }
                self.frame_write_torn = false;
                *progress = PacketFrameProgress::default();
                Poll::Ready(Ok(()))
            }
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> Snell<S> {
    /// Consume the server's status byte. Idempotent — calls after the first
    /// successful invocation are no-ops until [`Snell::reset_reply_state`].
    pub async fn read_reply(&mut self) -> io::Result<()> {
        if self.reply_consumed {
            return Ok(());
        }
        let mut byte = [0u8; 1];
        self.read_exact_underlying(&mut byte).await?;
        self.reply_consumed = true;
        match byte[0] {
            RESPONSE_TUNNEL | RESPONSE_PONG => Ok(()),
            RESPONSE_ERROR => {
                let mut buf = [0u8; 1];
                self.read_exact_underlying(&mut buf).await?;
                let code = buf[0];
                self.read_exact_underlying(&mut buf).await?;
                let len = buf[0] as usize;
                let mut msg = vec![0u8; len];
                if len > 0 {
                    self.read_exact_underlying(&mut msg).await?;
                }
                let message = String::from_utf8_lossy(&msg).into_owned();
                Err(io::Error::other(AppError { code, message }))
            }
            other => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("snell: unknown response code 0x{other:x}"),
            )),
        }
    }

    /// Whether an idle pooled connection still looks usable: one read poll
    /// (with a no-op waker) must find nothing — no data, EOF or error. A
    /// v6 request is only written with the session's first bytes, so a
    /// connection the server dropped while idle is caught here rather than
    /// after the caller has handed it data.
    pub fn idle_conn_alive(&mut self) -> bool {
        let mut cx = Context::from_waker(Waker::noop());
        let mut byte = [0u8; 1];
        let mut rb = ReadBuf::new(&mut byte);
        self.inner.poll_read_inner(&mut cx, &mut rb).is_pending()
    }

    async fn read_exact_underlying(&mut self, buf: &mut [u8]) -> io::Result<()> {
        // Read directly from the AEAD-decoded stream, bypassing the reply
        // guard (otherwise we'd recurse).
        let mut filled = 0;
        while filled < buf.len() {
            let mut rb = ReadBuf::new(&mut buf[filled..]);
            std::future::poll_fn(|cx| self.inner.poll_read_inner(cx, &mut rb)).await?;
            let n = rb.filled().len();
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "snell: unexpected EOF reading reply",
                ));
            }
            filled += n;
        }
        Ok(())
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for Snell<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        out: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // First-read reply handshake. Implemented as a hand-rolled state
        // machine: the byte is read via `poll_read` on the inner stream into
        // a tiny scratch buffer, then we recurse on the body read in the
        // same poll once the reply is consumed.
        let this = &mut *self;
        if this.peer_half_closed {
            return Poll::Ready(Ok(()));
        }
        if !this.reply_consumed {
            let mut buf = [0u8; 1];
            let mut rb = ReadBuf::new(&mut buf);
            match this.inner.poll_read_inner(cx, &mut rb) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Ready(Ok(())) => {}
            }
            if rb.filled().is_empty() {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "snell: EOF before reply byte",
                )));
            }
            this.reply_consumed = true;
            match buf[0] {
                RESPONSE_TUNNEL | RESPONSE_PONG => {}
                RESPONSE_ERROR => {
                    // We're inside poll_read — surface the error rather than
                    // trying to read the error tail synchronously. The caller
                    // will report it; for richer messages the explicit
                    // `read_reply` path is preferred (the adapter calls it for
                    // UDP, and on TCP the next byte is data, so any error is
                    // surfaced as an io::Error here).
                    return Poll::Ready(Err(io::Error::other(
                        "snell: server returned error response (use read_reply for details)",
                    )));
                }
                other => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("snell: unknown response code 0x{other:x}"),
                    )));
                }
            }
        }
        // Map the v4 zero-chunk into a clean EOF for the caller.
        match this.inner.poll_read_inner(cx, out) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) if is_zero_chunk(&e) => {
                this.peer_half_closed = true;
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for Snell<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.inner.poll_write_inner(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.inner.poll_flush_inner(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.inner.poll_shutdown_inner(cx)
    }
}

#[cfg(test)]
#[path = "../test/snell_protocol_test.rs"]
mod tests;
