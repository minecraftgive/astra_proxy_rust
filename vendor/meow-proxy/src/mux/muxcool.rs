//! Xray Mux.Cool client (mux config: protocol muxcool, VLESS-only).
//!
//! Mux.Cool multiplexes logical streams over one physical VLESS connection:
//! the session connection's VLESS request carries command 0x03
//! (CommandMux) and no address, then every stream exchanges
//! meta + payload frames that carry the real per-stream destination.
//!
//! # Wire format
//!
//! upstream: xray common/mux/frame.go (FrameMetadata / frame format comment),
//!           xray common/mux/writer.go (WriteMultiBuffer / Close);
//!           sagernet/sing-vmess mux.go::HandleMuxConnection (server side).
//!
//!   meta_len   u16 BE    length of the meta block
//!   meta:
//!     session_id u16 BE  stream id - client-allocated, echoed by the server
//!     status     u8      1=New 2=Keep 3=End 4=KeepAlive
//!     option     u8      bit 1 = Data, bit 2 = Error
//!     (New frames, and Keep frames carrying a UDP destination:)
//!       network u8       1=TCP 2=UDP
//!       port    u16 BE
//!       atype   u8       0x01 IPv4 | 0x02 domain | 0x03 IPv6
//!       address ...      VMess address layout - port BEFORE address
//!   payload_len u16 BE   present iff option&Data
//!   payload             payload_len bytes
//!
//! End frames carry no payload length.  The server never opens streams: it
//! answers on the session id from the client's New frame.  Extra meta bytes
//! are tolerated (Xray appends optional fullcone/GlobalID metadata we do not
//! send; both Xray and sing-vmess servers discard unknown meta bytes).
//!
//! Compatible servers: Xray-core (VLESS inbound) and sing-box/sing-vmess
//! (vmess.HandleMuxConnection behind VLESS CommandMux), plus mihomo's
//! sing-vmess VLESS listener.

use super::client::MuxSession;
use bytes::{BufMut, Bytes, BytesMut};
use meow_common::{MeowError, ProxyConn, ProxyPacketConn, Result};
use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Weak};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::sync::{mpsc, Mutex, Notify};
use tokio_util::sync::PollSender;

// --- Wire constants (xray common/mux/frame.go) ------------------------------

const STATUS_NEW: u8 = 0x01;
const STATUS_KEEP: u8 = 0x02;
const STATUS_END: u8 = 0x03;
const STATUS_KEEPALIVE: u8 = 0x04;

const OPTION_DATA: u8 = 0x01;
const OPTION_ERROR: u8 = 0x02;

const NETWORK_TCP: u8 = 0x01;
const NETWORK_UDP: u8 = 0x02;

const ATYPE_IPV4: u8 = 0x01;
const ATYPE_DOMAIN: u8 = 0x02;
const ATYPE_IPV6: u8 = 0x03;

/// Session id used on keepalive frames (not bound to any stream).
const KEEPALIVE_SESSION_ID: u16 = 0;

/// Payload cap per data frame - xray splits stream writes at 8 KiB
/// (common/mux/writer.go::WriteMultiBuffer), keeping frame latency bounded.
const MAX_PAYLOAD: usize = 8 * 1024;

/// Xray's PacketReader (common/mux/reader.go) rejects UDP payloads larger
/// than common/buf.Size — the same 8 KiB `MAX_PAYLOAD` derives from — and
/// aborts the entire mux session. Reject them before queuing a frame.
const MAX_UDP_PAYLOAD: usize = MAX_PAYLOAD;

/// Per-stream inbound queue depth (frames, not bytes).  A stalled consumer
/// applies session-wide flow control: the driver parks the overflowing
/// frame (at most one) and pauses the connection read until the consumer
/// drains its queue - TCP window semantics, without the reader task ever
/// blocking on a stream queue (which would deadlock writers waiting on
/// outbound capacity).
const STREAM_QUEUE: usize = 32;

/// Client keepalive cadence - keeps NAT bindings alive on idle sessions;
/// servers drop status-4 frames silently on both sides.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// Meta length cap — xray FrameMetadata::Unmarshal rejects meta_len > 512;
/// capping also bounds the per-frame allocation on garbage input.
const MAX_META: usize = 512;

// --- Frame codec (pure, no I/O - byte-tested below) -------------------------

/// Append [port u16 BE][atype u8][address] (VMess layout: port first).
fn put_address(out: &mut BytesMut, host: &str, port: u16) -> io::Result<()> {
    out.put_u16(port);
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => {
            out.put_u8(ATYPE_IPV4);
            out.put_slice(&ip.octets());
        }
        Ok(IpAddr::V6(ip)) => {
            out.put_u8(ATYPE_IPV6);
            out.put_slice(&ip.octets());
        }
        Err(_) => {
            if host.len() > u8::MAX as usize {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("mux.cool: domain '{host}' exceeds 255 bytes"),
                ));
            }
            out.put_u8(ATYPE_DOMAIN);
            out.put_u8(host.len() as u8);
            out.put_slice(host.as_bytes());
        }
    }
    Ok(())
}

/// Encode a New frame (option = no payload): opens the stream and declares
/// its destination.  The first data chunk follows in a Keep frame.
pub(crate) fn encode_new_frame(sid: u16, udp: bool, host: &str, port: u16) -> io::Result<Bytes> {
    let mut out = BytesMut::with_capacity(64);
    out.put_u16(0); // meta_len placeholder, patched below
    out.put_u16(sid);
    out.put_u8(STATUS_NEW);
    out.put_u8(0); // no payload
    out.put_u8(if udp { NETWORK_UDP } else { NETWORK_TCP });
    put_address(&mut out, host, port)?;
    let meta_len = out.len() - 2;
    out[0..2].copy_from_slice(&(meta_len as u16).to_be_bytes());
    Ok(out.freeze())
}

/// Encode a Keep+Data frame.  dest adds a per-datagram destination (UDP);
/// TCP data frames carry none (meta_len = 4).
fn encode_data_frame(sid: u16, data: &[u8], dest: Option<(&str, u16)>) -> io::Result<Bytes> {
    let mut out = BytesMut::with_capacity(16 + data.len());
    out.put_u16(0); // meta_len placeholder
    out.put_u16(sid);
    out.put_u8(STATUS_KEEP);
    out.put_u8(OPTION_DATA);
    if let Some((host, port)) = dest {
        out.put_u8(NETWORK_UDP);
        put_address(&mut out, host, port)?;
    }
    let meta_len = out.len() - 2;
    out[0..2].copy_from_slice(&(meta_len as u16).to_be_bytes());
    debug_assert!(
        data.len() <= u16::MAX as usize,
        "muxcool: data frame payload exceeds u16"
    );
    out.put_u16(data.len() as u16);
    out.put_slice(data);
    Ok(out.freeze())
}

/// Encode a Keep+Data frame carrying a per-datagram UDP destination from
/// raw IP octets - the hot UDP write path, no string round-trip through
/// put_address.
fn encode_udp_data_frame(sid: u16, data: &[u8], ip: IpAddr, port: u16) -> Bytes {
    let mut out = BytesMut::with_capacity(32 + data.len());
    out.put_u16(0); // meta_len placeholder, patched below
    out.put_u16(sid);
    out.put_u8(STATUS_KEEP);
    out.put_u8(OPTION_DATA);
    out.put_u8(NETWORK_UDP);
    out.put_u16(port);
    match ip {
        IpAddr::V4(v4) => {
            out.put_u8(ATYPE_IPV4);
            out.put_slice(&v4.octets());
        }
        IpAddr::V6(v6) => {
            out.put_u8(ATYPE_IPV6);
            out.put_slice(&v6.octets());
        }
    }
    let meta_len = out.len() - 2;
    out[0..2].copy_from_slice(&(meta_len as u16).to_be_bytes());
    debug_assert!(
        data.len() <= u16::MAX as usize,
        "muxcool: udp data frame payload exceeds u16"
    );
    out.put_u16(data.len() as u16);
    out.put_slice(data);
    out.freeze()
}

/// Encode an End frame (no payload length field follows).
pub(crate) fn encode_end_frame(sid: u16, has_error: bool) -> Bytes {
    let mut out = BytesMut::with_capacity(6);
    out.put_u16(4);
    out.put_u16(sid);
    out.put_u8(STATUS_END);
    out.put_u8(if has_error { OPTION_ERROR } else { 0 });
    out.freeze()
}

/// Encode a KeepAlive frame with the unbound session id 0.
pub(crate) fn encode_keepalive_frame() -> Bytes {
    let mut out = BytesMut::with_capacity(6);
    out.put_u16(4);
    out.put_u16(KEEPALIVE_SESSION_ID);
    out.put_u8(STATUS_KEEPALIVE);
    out.put_u8(0);
    out.freeze()
}

/// Destination host parsed from a frame's address block.  IP destinations —
/// the overwhelmingly common case on the UDP hot path — stay as `IpAddr`, so
/// `dest_to_socket` builds the `SocketAddr` directly instead of round
/// tripping through `to_string()`/reparse; domains (rare here) keep their
/// raw bytes, same as before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Dest {
    Ip(IpAddr),
    Domain(Vec<u8>),
}

impl Dest {
    /// Render as a host string for the `&str`-based frame encoders
    /// (`encode_new_frame`/`encode_data_frame`).  Test-only: the hot UDP
    /// write path uses `encode_udp_data_frame` directly on raw octets and
    /// never needs this.
    #[cfg(test)]
    fn host_string(&self) -> String {
        match self {
            Dest::Ip(ip) => ip.to_string(),
            Dest::Domain(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        }
    }
}

/// Decoded meta block of a frame.
#[derive(Debug)]
pub(crate) struct FrameMeta {
    pub session_id: u16,
    pub status: u8,
    pub option: u8,
    /// Per-frame destination (New frames and UDP Keep frames).
    pub dest: Option<(Dest, u16)>,
}

/// Parse a meta block (the bytes between meta_len and the payload).
///
/// Mirrors xray FrameMetadata::UnmarshalFromBuffer: the destination is read
/// for New frames and for Keep frames whose extra byte announces UDP; other
/// trailing meta bytes are ignored.
pub(crate) fn decode_meta(meta: &[u8]) -> io::Result<FrameMeta> {
    if meta.len() < 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "mux.cool: meta shorter than 4 bytes",
        ));
    }
    let session_id = u16::from_be_bytes([meta[0], meta[1]]);
    let status = meta[2];
    let option = meta[3];
    // End frames never carry a payload: a Data bit here means the peer is
    // off-spec and a payload length would follow where we expect the next
    // meta_len - reject instead of desyncing the frame stream.
    if status == STATUS_END && option & OPTION_DATA != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "mux.cool: End frame with data option",
        ));
    }
    let rest = &meta[4..];
    let has_dest = status == STATUS_NEW
        || (status == STATUS_KEEP && !rest.is_empty() && rest[0] == NETWORK_UDP);
    let dest = if has_dest {
        // New frames MUST carry a destination (xray: insufficient buffer) —
        // guard before indexing rest[0] so a malformed frame errors instead
        // of panicking the reader task.
        if rest.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "mux.cool: New frame without destination",
            ));
        }
        let network = rest[0];
        if network != NETWORK_TCP && network != NETWORK_UDP {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("mux.cool: unknown network type {network}"),
            ));
        }
        Some(parse_address(&rest[1..])?)
    } else {
        None
    };
    Ok(FrameMeta {
        session_id,
        status,
        option,
        dest,
    })
}

/// Parse [port u16 BE][atype][address] into a destination + port.  IP
/// addresses are kept as `IpAddr` (no `to_string()` allocation on this hot
/// UDP-datagram-read path); only the rare domain case still allocates a
/// `Vec<u8>`.
fn parse_address(b: &[u8]) -> io::Result<(Dest, u16)> {
    if b.len() < 3 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "mux.cool: truncated address",
        ));
    }
    let port = u16::from_be_bytes([b[0], b[1]]);
    let dest = match b[2] {
        ATYPE_IPV4 => {
            if b.len() < 7 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "mux.cool: truncated IPv4 address",
                ));
            }
            Dest::Ip(IpAddr::V4(std::net::Ipv4Addr::new(b[3], b[4], b[5], b[6])))
        }
        ATYPE_IPV6 => {
            if b.len() < 19 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "mux.cool: truncated IPv6 address",
                ));
            }
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&b[3..19]);
            Dest::Ip(IpAddr::V6(std::net::Ipv6Addr::from(octets)))
        }
        ATYPE_DOMAIN => {
            // Guard b[3] before reading it: a 3-byte block (port + atype
            // only) must error, not panic the reader task.
            if b.len() < 4 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "mux.cool: truncated domain address",
                ));
            }
            let len = b[3] as usize;
            if b.len() < 4 + len {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "mux.cool: truncated domain address",
                ));
            }
            Dest::Domain(b[4..4 + len].to_vec())
        }
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("mux.cool: unknown address type {other}"),
            ));
        }
    };
    Ok((dest, port))
}

// --- Events on a stream's inbound channel -----------------------------------

pub(crate) enum Event {
    Data {
        bytes: Bytes,
        dest: Option<(Dest, u16)>,
    },
    /// Server sent End for this stream: deliver a clean EOF.
    Eof,
    Error(io::Error),
}

// --- Session ----------------------------------------------------------------

/// Outbound frame queue depth per session (frames, not bytes); a slow
/// server applies backpressure to stream writes via the bounded channel.
const OUTBOUND_QUEUE: usize = 64;

/// One Mux.Cool session multiplexing streams over one VLESS connection.
///
/// A single driver task owns both connection halves and polls both directions.
/// Outbound frames arrive over a bounded channel (backpressure via
/// PollSender), while persistent read/write state makes either select branch
/// cancellation-safe.
///
/// Inbound delivery never blocks the driver: a frame that hits a full
/// stream queue parks (at most one) and pauses the connection read -
/// session-wide flow control with TCP window semantics.  Blocking here
/// would deadlock a consumer that stalls its reads until its writes
/// (queued behind the driver's outbound channel) complete; consumers wake
/// the driver (space Notify) whenever they drain a queue.
///
/// Dropping the last external Arc (pool eviction of a zero-stream session)
/// tears the session down: Drop fires the Notify, the driver exits.
pub(crate) struct MuxCoolSession {
    driver_tx: mpsc::Sender<Bytes>,
    streams: Arc<Mutex<HashMap<u16, mpsc::Sender<Event>>>>,
    next_sid: AtomicU32,
    /// Stops the pool selecting this session after its u16 id space is used,
    /// without aborting existing streams or their keepalive traffic.
    retired: AtomicBool,
    closed: Arc<AtomicBool>,
    done: Arc<Notify>,
    /// Fired by stream consumers whenever they drain their inbound queue
    /// (or drop the stream): wakes a driver that parked a frame on a full
    /// queue.  notify_one keeps a permit for the arm-before-check pattern
    /// in the driver loop, so no wake is lost.
    space: Arc<Notify>,
}

impl MuxCoolSession {
    /// Start a session over conn - a VLESS connection whose request carried
    /// CommandMux.  The VLESS response header is consumed lazily by the
    /// driver's first read (VlessConn semantics).
    pub(crate) async fn client(conn: Box<dyn ProxyConn>) -> io::Result<Arc<Self>> {
        let (driver_tx, out_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let streams = Arc::new(Mutex::new(HashMap::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let done = Arc::new(Notify::new());
        let space = Arc::new(Notify::new());
        let session = Arc::new(Self {
            driver_tx,
            streams: Arc::clone(&streams),
            next_sid: AtomicU32::new(1),
            retired: AtomicBool::new(false),
            closed: Arc::clone(&closed),
            done: Arc::clone(&done),
            space: Arc::clone(&space),
        });
        tokio::spawn(driver_loop(
            conn,
            out_rx,
            Arc::clone(&streams),
            Arc::clone(&closed),
            Arc::clone(&done),
            Arc::clone(&space),
        ));
        tokio::spawn(keepalive_loop(
            Arc::downgrade(&session),
            Arc::clone(&closed),
        ));
        Ok(session)
    }

    /// True once the session is unusable (driver died).
    pub(crate) fn aborted(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// True when the pool must replace the session.  Retirement is distinct
    /// from abort: existing streams remain usable after id exhaustion.
    pub(crate) fn unavailable(&self) -> bool {
        self.retired.load(Ordering::SeqCst) || self.aborted()
    }

    /// Queue one outbound frame to the driver task (async contexts).
    ///
    /// Only a dead session is rejected.  Retirement (sid exhaustion) must
    /// NOT block this path: it only stops the pool opening *new* streams
    /// (alloc_sid fails), while already-open streams must keep sending
    /// data, End and keepalive frames for the rest of their lifetime.
    async fn send_frame(&self, frame: Bytes) -> io::Result<()> {
        if self.aborted() {
            return Err(io::Error::other("mux.cool session closed"));
        }
        self.driver_tx
            .send(frame)
            .await
            .map_err(|_| io::Error::other("mux.cool session closed"))
    }

    /// Open one stream and write its New frame carrying host:port.
    /// Returns the raw parts; callers wrap them (Stream for TCP, PacketConn
    /// for UDP).
    async fn open_stream_parts(
        self: &Arc<Self>,
        host: &str,
        port: u16,
        udp: bool,
    ) -> io::Result<StreamParts> {
        if self.aborted() {
            return Err(io::Error::other("mux.cool session closed"));
        }
        let sid = self.alloc_sid()?;
        let frame = encode_new_frame(sid, udp, host, port)?;
        let (tx, rx) = mpsc::channel(STREAM_QUEUE);
        self.streams.lock().await.insert(sid, tx);
        // The guard removes the map entry if this future is cancelled
        // while waiting for outbound capacity — otherwise the id stays
        // consumed and the entry lingers for the session's lifetime.
        let mut guard = StreamMapGuard {
            streams: Arc::clone(&self.streams),
            sid,
            armed: true,
        };
        if let Err(e) = self.send_frame(frame).await {
            drop(guard);
            return Err(e);
        }
        guard.disarm();
        Ok(StreamParts {
            sid,
            session: Arc::clone(self),
            rx,
            guard: EndGuard::new(sid, Arc::clone(self)),
        })
    }

    /// Open a TCP stream (see open_stream_parts).
    pub(crate) async fn open_stream(
        self: &Arc<Self>,
        host: &str,
        port: u16,
        udp: bool,
    ) -> io::Result<Stream> {
        let parts = self.open_stream_parts(host, port, udp).await?;
        Ok(Stream::from_parts(parts))
    }

    /// Client-assigned ids start at 1 and increase by 1
    /// (xray common/mux/session.go::Allocate).  Once exhausted the session
    /// keeps failing: ids never wrap around, because a recycled id would
    /// collide with a live stream's frames.  The pool dials a fresh session
    /// for later opens; existing streams keep working.
    fn alloc_sid(&self) -> io::Result<u16> {
        // compare_exchange in a loop prevents the u32 from wrapping past
        // u16::MAX + 1 back into the valid range after 2^32 opens; a plain
        // fetch_add would wrap and could recycle a live id.  The loop
        // retries on CAS contention (concurrent open_stream calls on the
        // same session) rather than falsely declaring the session retired.
        loop {
            let val = self.next_sid.load(Ordering::SeqCst);
            if val > u16::MAX as u32 {
                self.retired.store(true, Ordering::SeqCst);
                return Err(io::Error::other("mux.cool session id space exhausted"));
            }
            match self
                .next_sid
                .compare_exchange(val, val + 1, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return Ok(val as u16),
                Err(_) => continue,
            }
        }
    }
}

impl MuxCoolSession {
    /// Tear the session down now even while streams still hold it: the
    /// driver exits and drops the VLESS connection, so every stream fails.
    /// Used by pool resets (issue #695). Idempotent.
    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        // notify_one: see `Drop` below.
        self.done.notify_one();
    }
}

impl Drop for MuxCoolSession {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
        // notify_one (not notify_waiters): the driver's `done.notified()`
        // future is re-created fresh every loop iteration, so a drop
        // landing in the gap between iterations - before the next
        // `notified()` registers a waiter - would be missed by
        // notify_waiters, which stores no permit. notify_one stores a
        // permit for the next waiter to consume, matching the `space`
        // Notify's arm-before-check pattern used elsewhere in this driver.
        self.done.notify_one();
    }
}

/// Removes the stream-map entry for `sid` when dropped while armed — the
/// cancellation-safe counterpart of `open_stream_parts`'s insert-then-send.
struct StreamMapGuard {
    streams: Arc<Mutex<HashMap<u16, mpsc::Sender<Event>>>>,
    sid: u16,
    armed: bool,
}

impl StreamMapGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StreamMapGuard {
    fn drop(&mut self) {
        if self.armed {
            let streams = Arc::clone(&self.streams);
            let sid = self.sid;
            // Detached: drop cannot await the map lock.
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    streams.lock().await.remove(&sid);
                });
            }
        }
    }
}

/// Owns one stream's shared parts.  Dropping the EndGuard inside sends the
/// stream's End frame (best effort) unless shutdown already claimed it.
pub(crate) struct StreamParts {
    pub(crate) sid: u16,
    pub(crate) session: Arc<MuxCoolSession>,
    pub(crate) rx: mpsc::Receiver<Event>,
    pub(crate) guard: EndGuard,
}

/// Fires the session's space Notify when dropped: wakes a driver parked on
/// this stream's full queue once the stream (and its receiver) is gone, so
/// the parked delivery fails fast and the driver resumes reading.  Kept as
/// a field declared after the receiver so the channel closes before the
/// notify fires.
struct SpaceWake(Arc<Notify>);

impl Drop for SpaceWake {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

/// Sends the stream's End frame exactly once: either when shutdown claims
/// it, or when the last holder (Stream / PacketConn) drops.
pub(crate) struct EndGuard {
    sid: u16,
    session: Arc<MuxCoolSession>,
    ended: bool,
}

impl EndGuard {
    fn new(sid: u16, session: Arc<MuxCoolSession>) -> Self {
        Self {
            sid,
            session,
            ended: false,
        }
    }
}

impl Drop for EndGuard {
    fn drop(&mut self) {
        let send_end = !std::mem::replace(&mut self.ended, true) && !self.session.aborted();
        if !send_end {
            // The shutdown path already spawned its own map removal (and
            // claims the End); an aborted session's driver clears the map
            // on exit.  Nothing left for this drop to do.
            return;
        }
        let session = Arc::clone(&self.session);
        let sid = self.sid;
        // Detached: the End frame queues behind any in-flight data
        // frames in the driver's channel; drop cannot await the send.
        // Skip when no runtime exists (teardown): the session is
        // going away anyway.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                // Drop the stream-map entry first: Mux.Cool servers (notably
                // sing-box) never echo End frames, so the driver cannot
                // clean this up itself, and the cleanup must not wait on
                // outbound channel capacity.  Removing the entry closes the
                // inbound channel (poll_recv → None → Eof) and keeps the
                // map bounded under connection churn.
                session.streams.lock().await.remove(&sid);
                let _ = session.send_frame(encode_end_frame(sid, false)).await;
            });
        }
    }
}

// --- Driver task -------------------------------------------------------------

/// Frame read phase.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReadPhase {
    MetaLen,
    Meta,
    DataLen,
    Data,
}

/// Frame read state machine.  It lives in the driver task (not inside a
/// recreated future), so a select! cancellation — an outbound frame
/// arriving mid-read — loses nothing: partial bytes persist in the buffer
/// and the read resumes exactly where it left off.
struct ReadState {
    phase: ReadPhase,
    /// Scratch buffer for the current phase.  Resizing retains the
    /// capacity across frames; only the delivered payload splits off its
    /// own Bytes, so large allocations happen once per payload.
    buf: BytesMut,
    pos: usize,
    sid: u16,
    option: u8,
    dest: Option<(Dest, u16)>,
}

impl ReadState {
    fn fresh() -> Self {
        let mut buf = BytesMut::with_capacity(2);
        buf.resize(2, 0);
        Self {
            phase: ReadPhase::MetaLen,
            buf,
            pos: 0,
            sid: 0,
            option: 0,
            dest: None,
        }
    }

    /// Start reading a fresh frame header.
    fn reset_header(&mut self) {
        self.phase = ReadPhase::MetaLen;
        self.buf.resize(2, 0);
        self.pos = 0;
    }
}

/// Read until the buffer is full.  Every call site sizes the buffer before
/// entering its phase, so the slice exposed to the connection is exactly
/// buf[pos..] - there is no separate length argument to get wrong.
async fn read_into(conn: &mut (impl AsyncRead + Unpin), state: &mut ReadState) -> io::Result<()> {
    while state.pos < state.buf.len() {
        let n = conn.read(&mut state.buf[state.pos..]).await?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "mux.cool session closed",
            ));
        }
        state.pos += n;
    }
    Ok(())
}

/// One inbound frame parked because its stream's queue is full.  At most
/// one exists at a time: parking pauses the connection read (session-wide
/// flow control, TCP window semantics), so no further frames pile up.
/// The driver retries without blocking - it never blocks on a stream queue,
/// so a consumer stalled mid-write cannot deadlock writers sharing the
/// session.
struct Parked {
    tx: mpsc::Sender<Event>,
    event: Event,
}

/// Try to hand one event to a stream without blocking: on a full queue the
/// frame parks, on a closed channel (stream gone) it is moot.
fn deliver_now(tx: Option<mpsc::Sender<Event>>, event: Event) -> Option<Parked> {
    let tx = tx?;
    match tx.try_send(event) {
        Err(mpsc::error::TrySendError::Full(event)) => Some(Parked { tx, event }),
        Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => None,
    }
}

/// Retry a parked delivery: succeeds the moment the consumer drained its
/// queue, or the channel closed because the consumer dropped the stream.
fn retry_parked(parked: &mut Option<Parked>) {
    if let Some(mut p) = parked.take() {
        match p.tx.try_send(p.event) {
            Err(mpsc::error::TrySendError::Full(event)) => {
                p.event = event;
                *parked = Some(p);
            }
            Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }
}

/// Advance the read state machine by one phase: the phase changes only on
/// completion, so cancellation mid-phase is safe.  Returns Some(frame) when
/// the frame could not be delivered (full stream queue) and must park.
async fn read_step(
    conn: &mut (impl AsyncRead + Unpin),
    state: &mut ReadState,
    streams: &Arc<Mutex<HashMap<u16, mpsc::Sender<Event>>>>,
) -> io::Result<Option<Parked>> {
    match state.phase {
        ReadPhase::MetaLen => {
            read_into(conn, state).await?;
            let meta_len = u16::from_be_bytes([state.buf[0], state.buf[1]]);
            if meta_len as usize > MAX_META {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("mux.cool: meta length {meta_len} exceeds {MAX_META}"),
                ));
            }
            state.buf.resize(meta_len as usize, 0);
            state.pos = 0;
            state.phase = ReadPhase::Meta;
            Ok(None)
        }
        ReadPhase::Meta => {
            read_into(conn, state).await?;
            let frame = decode_meta(&state.buf)?;
            state.sid = frame.session_id;
            state.option = frame.option;
            state.dest = frame.dest;
            match frame.status {
                STATUS_NEW => Err(io::Error::new(
                    // The server never opens streams on a client session.
                    io::ErrorKind::InvalidData,
                    "mux.cool: server-initiated stream",
                )),
                STATUS_KEEPALIVE => {
                    state.reset_header();
                    Ok(None)
                }
                STATUS_END => {
                    // Remove the entry before delivering the terminal
                    // event: even when the event parks (queue full) no
                    // stale sender survives in the map, and the parked
                    // retry owns its own tx clone.
                    let event = if state.option & OPTION_ERROR != 0 {
                        Event::Error(io::Error::other(
                            "mux.cool: remote stream closed with error",
                        ))
                    } else {
                        Event::Eof
                    };
                    let tx = {
                        let mut map = streams.lock().await;
                        map.remove(&state.sid)
                    };
                    state.reset_header();
                    Ok(deliver_now(tx, event))
                }
                STATUS_KEEP if state.option & OPTION_DATA != 0 => {
                    state.buf.resize(2, 0);
                    state.pos = 0;
                    state.phase = ReadPhase::DataLen;
                    Ok(None)
                }
                STATUS_KEEP => {
                    let park = if state.option & OPTION_ERROR != 0 {
                        let tx = {
                            let map = streams.lock().await;
                            map.get(&state.sid).cloned()
                        };
                        deliver_now(
                            tx,
                            Event::Error(io::Error::other("mux.cool: remote stream error")),
                        )
                    } else {
                        None
                    };
                    state.reset_header();
                    Ok(park)
                }
                other => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("mux.cool: unknown frame status {other}"),
                )),
            }
        }
        ReadPhase::DataLen => {
            read_into(conn, state).await?;
            let data_len = u16::from_be_bytes([state.buf[0], state.buf[1]]) as usize;
            state.buf.resize(data_len, 0);
            state.pos = 0;
            state.phase = ReadPhase::Data;
            Ok(None)
        }
        ReadPhase::Data => {
            read_into(conn, state).await?;
            // Acquire the map entry before consuming persistent frame state.
            // If select! cancels while this lock is pending, the payload and
            // phase remain intact and the next read_step retries delivery.
            let tx = {
                let map = streams.lock().await;
                map.get(&state.sid).cloned()
            };
            // Read the payload before delivering an error: an Error|Data
            // frame still carries data_len + payload, and skipping it would
            // desync the frame stream.
            let event = if state.option & OPTION_ERROR != 0 {
                Some(Event::Error(io::Error::other(
                    "mux.cool: remote stream error",
                )))
            } else {
                let bytes = std::mem::take(&mut state.buf).freeze();
                if bytes.is_empty() {
                    None
                } else {
                    Some(Event::Data {
                        bytes,
                        dest: std::mem::take(&mut state.dest),
                    })
                }
            };
            state.reset_header();
            match event {
                Some(event) => Ok(deliver_now(tx, event)),
                None => Ok(None),
            }
        }
    }
}

/// Persistent outbound batch.  `pos` and the retained buffer make write and
/// flush cancellation-safe while allowing the read side to keep progressing.
struct WriteState {
    buf: BytesMut,
    pos: usize,
}

impl WriteState {
    fn new() -> Self {
        Self {
            buf: BytesMut::new(),
            pos: 0,
        }
    }
}

/// Advance one outbound operation.  Newly queued frames are coalesced into a
/// single batch and flushed together; partial writes persist in `state`.
async fn write_step(
    writer: &mut (impl AsyncWrite + Unpin),
    out_rx: &mut mpsc::Receiver<Bytes>,
    state: &mut WriteState,
) -> io::Result<()> {
    if state.buf.is_empty() {
        let Some(frame) = out_rx.recv().await else {
            return Err(io::Error::other("mux.cool outbound channel closed"));
        };
        state.buf.extend_from_slice(&frame);
        while let Ok(frame) = out_rx.try_recv() {
            state.buf.extend_from_slice(&frame);
        }
        state.pos = 0;
    }

    if state.pos < state.buf.len() {
        let written = writer.write(&state.buf[state.pos..]).await?;
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "mux.cool session write returned zero",
            ));
        }
        state.pos += written;
        return Ok(());
    }

    writer.flush().await?;
    state.buf.clear();
    state.pos = 0;
    Ok(())
}

#[cfg(test)]
async fn read_u16(reader: &mut (impl AsyncRead + Unpin)) -> io::Result<u16> {
    let mut b = [0u8; 2];
    reader.read_exact(&mut b).await?;
    Ok(u16::from_be_bytes(b))
}

/// Owns both halves and polls both directions from one task.  Tokio's split
/// only interleaves polls; persistent state in `read_step` and `write_step`
/// prevents the stored-future duplication that corrupted earlier builds.
async fn driver_loop(
    conn: Box<dyn ProxyConn>,
    mut out_rx: mpsc::Receiver<Bytes>,
    streams: Arc<Mutex<HashMap<u16, mpsc::Sender<Event>>>>,
    closed: Arc<AtomicBool>,
    done: Arc<Notify>,
    space: Arc<Notify>,
) {
    let (mut reader, mut writer) = tokio::io::split(conn);
    let mut state = ReadState::fresh();
    let mut write_state = WriteState::new();
    // A parked inbound frame pauses the read side (flow control) while
    // outbound frames keep flowing: the driver never blocks on a stream
    // queue, so a consumer waiting for outbound capacity can always make
    // progress and eventually drain its own inbound queue.
    let mut parked: Option<Parked> = None;

    let result: io::Result<()> = async {
        loop {
            // Armed before the retry so a drain in the gap between them is
            // not lost: notify_one stores a permit when no waiter is
            // registered yet, and this future consumes it.
            let space_fut = space.notified();
            retry_parked(&mut parked);
            tokio::select! {
                _ = done.notified() => {
                    return Err(io::Error::other("mux.cool session closed"));
                }
                result = write_step(&mut writer, &mut out_rx, &mut write_state) => result?,
                // A consumer drained a stream queue (or dropped a stream):
                // wake so the retry above re-runs promptly.
                _ = space_fut, if parked.is_some() => {}
                step = read_step(&mut reader, &mut state, &streams), if parked.is_none() => {
                    if let Some(p) = step? {
                        parked = Some(p);
                    }
                }
            }
        }
    }
    .await;

    // Mark the abort before delivery: consumers observing a closed channel
    // must see aborted() == true so they surface an error, not a clean EOF.
    closed.store(true, Ordering::SeqCst);
    if let Err(e) = &result {
        tracing::debug!("mux.cool driver exit: {e}");
        // Push the failure to every live stream, then clear the map —
        // dropping every sender closes the channels, so consumers parked on
        // a full queue observe the abort instead of hanging forever (the
        // session itself keeps the map alive).
        let mut map = streams.lock().await;
        for tx in map.values() {
            let _ = tx.try_send(Event::Error(io::Error::new(
                e.kind(),
                format!("mux.cool session aborted: {e}"),
            )));
        }
        map.clear();
    }
}

async fn keepalive_loop(session: Weak<MuxCoolSession>, closed: Arc<AtomicBool>) {
    loop {
        tokio::time::sleep(KEEPALIVE_INTERVAL).await;
        let Some(session) = session.upgrade() else {
            return;
        };
        if closed.load(Ordering::SeqCst) {
            return;
        }
        // The driver detects physical write failures; a send error here
        // just means the session is already going away.
        if session.send_frame(encode_keepalive_frame()).await.is_err() {
            return;
        }
    }
}

// --- TCP stream -------------------------------------------------------------

/// One multiplexed TCP stream.  Writes are framed Keep frames and queued
/// to the session driver over a bounded channel — PollSender applies real
/// backpressure when the driver falls behind; reads come from the stream's
/// demux channel.
///
/// Dropping it sends the stream's End frame via the EndGuard.
pub(crate) struct Stream {
    pub(crate) parts: StreamParts,
    poll_sender: PollSender<Bytes>,
    pending: Option<(Bytes, usize)>,
    eof: bool,
    /// End frame waiting for outbound channel capacity (shutdown is poll
    /// based; the frame survives Pending instead of being rebuilt).
    shutdown_frame: Option<Bytes>,
    /// Acts through Drop (wakes a driver parked on the full inbound queue
    /// once the receiver is gone); never read.
    #[allow(dead_code, reason = "SpaceWake acts through Drop, not reads")]
    wake: SpaceWake,
}

impl Stream {
    pub(crate) fn from_parts(parts: StreamParts) -> Self {
        let poll_sender = PollSender::new(parts.session.driver_tx.clone());
        let wake = SpaceWake(Arc::clone(&parts.session.space));
        Self {
            parts,
            poll_sender,
            pending: None,
            eof: false,
            shutdown_frame: None,
            wake,
        }
    }
}

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        // A zero-capacity buffer yields Ready(Ok(())) per the tokio
        // AsyncRead docs (a Pending here would leave `read(&mut [])`
        // parked forever).
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        if this.eof {
            return Poll::Ready(Ok(()));
        }
        if let Some((bytes, pos)) = &mut this.pending {
            let remaining = &bytes[*pos..];
            let n = remaining.len().min(buf.remaining());
            buf.put_slice(&remaining[..n]);
            *pos += n;
            if *pos >= bytes.len() {
                this.pending = None;
            }
            return Poll::Ready(Ok(()));
        }
        loop {
            match this.parts.rx.poll_recv(cx) {
                Poll::Ready(Some(event)) => {
                    // The channel slot just freed: wake a driver parked on
                    // this stream's full queue (notify_one keeps a permit
                    // when no waiter is registered yet, so the wake is
                    // never lost).
                    this.parts.session.space.notify_one();
                    match event {
                        Event::Data { bytes, .. } => {
                            if bytes.is_empty() {
                                continue;
                            }
                            let n = bytes.len().min(buf.remaining());
                            buf.put_slice(&bytes[..n]);
                            if n < bytes.len() {
                                this.pending = Some((bytes, n));
                            }
                            return Poll::Ready(Ok(()));
                        }
                        Event::Eof => {
                            this.eof = true;
                            return Poll::Ready(Ok(()));
                        }
                        Event::Error(e) => return Poll::Ready(Err(e)),
                    }
                }
                Poll::Ready(None) => {
                    this.parts.session.space.notify_one();
                    this.eof = true;
                    if this.parts.session.aborted() {
                        return Poll::Ready(Err(io::Error::other("mux.cool session closed")));
                    }
                    return Poll::Ready(Ok(()));
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if this.parts.guard.ended {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "mux.cool: stream closed",
            )));
        }
        if this.parts.session.aborted() {
            // A frame reserved while the driver is exiting would be
            // dropped without ever reaching the wire — surface an error
            // instead of reporting the bytes as written.
            return Poll::Ready(Err(io::Error::other("mux.cool session closed")));
        }
        let n = buf.len().min(MAX_PAYLOAD);
        match this.poll_sender.poll_reserve(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => Poll::Ready(Err(io::Error::other(e.to_string()))),
            Poll::Ready(Ok(())) => {
                // Encode only once capacity is reserved: a Pending reserve
                // costs no frame allocation on this hot path.
                let frame = match encode_data_frame(this.parts.sid, &buf[..n], None) {
                    Ok(frame) => frame,
                    Err(e) => return Poll::Ready(Err(e)),
                };
                if let Err(e) = this.poll_sender.send_item(frame) {
                    return Poll::Ready(Err(io::Error::other(e.to_string())));
                }
                Poll::Ready(Ok(n))
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // The driver flushes the connection after every frame, so there is
        // nothing buffered at the stream layer.
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.parts.guard.ended {
            return Poll::Ready(Ok(()));
        }
        if this.shutdown_frame.is_none() {
            this.shutdown_frame = Some(encode_end_frame(this.parts.sid, false));
        }
        let Some(frame) = this.shutdown_frame.take() else {
            return Poll::Ready(Ok(()));
        };
        match this.poll_sender.poll_reserve(cx) {
            Poll::Pending => {
                this.shutdown_frame = Some(frame);
                return Poll::Pending;
            }
            Poll::Ready(Err(e)) => {
                return Poll::Ready(Err(io::Error::other(e.to_string())));
            }
            Poll::Ready(Ok(())) => {
                // The End frame queues behind any data frames already
                // in the driver channel (FIFO order preserved).
                this.poll_sender
                    .send_item(frame)
                    .map_err(|e| io::Error::other(e.to_string()))?;
                // Claim the End only once it is enqueued: if this future
                // is cancelled while the reserve is Pending, the
                // EndGuard re-sends the End on drop.
                this.parts.guard.ended = true;
                // Mux.Cool servers (notably sing-box) close the stream
                // silently and never echo an End frame, so the read side
                // must EOF locally: the map removal below closes the
                // inbound channel (poll_recv → None → Eof), which is
                // what consumers observe.  Data frames the server sent
                // before processing our End are still delivered first —
                // the channel drains before it reports closure — so a
                // response that was already in flight is not discarded
                // (the End itself is a full close on the wire: Mux.Cool
                // has no TCP-style half-close, matching xray).
                // Remove the stream-map entry so the inbound channel
                // closes and the entry cannot accumulate for the
                // session's lifetime.  The EndGuard repeats this removal
                // for the drop-without-shutdown path.
                let session = Arc::clone(&this.parts.session);
                let sid = this.parts.sid;
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    handle.spawn(async move {
                        session.streams.lock().await.remove(&sid);
                    });
                }
            }
        }
        Poll::Ready(Ok(()))
    }
}

// --- UDP packet stream ------------------------------------------------------

/// One UDP flow multiplexed over a Mux.Cool session.  Datagrams carry
/// per-packet destinations in the frame meta (both directions, matching
/// sing-vmess serverMuxPacketConn / xray UDP keep frames).
pub(crate) struct PacketConn {
    sid: u16,
    session: Arc<MuxCoolSession>,
    rx: Mutex<mpsc::Receiver<Event>>,
    /// Held purely for its Drop (sends the End frame); never read.
    #[allow(dead_code, reason = "EndGuard acts through Drop, not reads")]
    guard: EndGuard,
    /// Declared after rx: fires only once the receiver is gone, so a driver
    /// parked on this flow's full queue retries against a closed channel.
    #[allow(dead_code, reason = "SpaceWake acts through Drop, not reads")]
    wake: SpaceWake,
    /// Stream's declared destination - fallback source for datagrams whose
    /// frame carries no per-packet address (e.g. domain targets).
    bound: SocketAddr,
    pool_session: Arc<MuxSession>,
}

impl PacketConn {
    pub(crate) fn new(
        parts: StreamParts,
        pool_session: Arc<MuxSession>,
        bound: SocketAddr,
    ) -> Self {
        let StreamParts {
            sid,
            session,
            rx,
            guard,
        } = parts;
        let wake = SpaceWake(Arc::clone(&session.space));
        Self {
            sid,
            session,
            rx: Mutex::new(rx),
            guard,
            wake,
            bound,
            pool_session,
        }
    }
}

impl Drop for PacketConn {
    fn drop(&mut self) {
        // The stream count belongs to the pool; the EndGuard and SpaceWake
        // act through their own Drops.
        self.pool_session.streams.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Resolve a frame destination to a SocketAddr; domains (rare on the UDP
/// path) fall back to the stream's bound destination.  IP destinations
/// build the SocketAddr directly - no string parse on this hot path.
fn dest_to_socket(dest: &Dest, port: u16, fallback: SocketAddr) -> SocketAddr {
    match dest {
        Dest::Ip(ip) => SocketAddr::new(*ip, port),
        Dest::Domain(_) => fallback,
    }
}

#[async_trait::async_trait]
impl ProxyPacketConn for PacketConn {
    async fn read_packet(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr)> {
        let mut rx = self.rx.lock().await;
        loop {
            match rx.recv().await {
                Some(event) => {
                    // The channel slot just freed: wake a driver parked on
                    // this flow's full queue.
                    self.session.space.notify_one();
                    match event {
                        Event::Data { bytes, dest } => {
                            if bytes.is_empty() {
                                continue;
                            }
                            if bytes.len() > buf.len() {
                                return Err(MeowError::Proxy(format!(
                                    "mux.cool: UDP packet ({} bytes) exceeds read buffer ({} bytes)",
                                    bytes.len(),
                                    buf.len()
                                )));
                            }
                            buf[..bytes.len()].copy_from_slice(&bytes);
                            let src = dest.map_or(self.bound, |(dest, port)| {
                                dest_to_socket(&dest, port, self.bound)
                            });
                            return Ok((bytes.len(), src));
                        }
                        Event::Eof => {
                            return Err(MeowError::Io(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                "mux.cool: UDP stream closed",
                            )));
                        }
                        Event::Error(e) => return Err(MeowError::Io(e)),
                    }
                }
                None => {
                    return Err(MeowError::Io(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "mux.cool: UDP stream closed",
                    )));
                }
            }
        }
    }

    async fn write_packet(&self, buf: &[u8], addr: &SocketAddr) -> Result<usize> {
        if buf.len() > MAX_UDP_PAYLOAD {
            return Err(MeowError::Proxy(format!(
                "mux.cool: UDP packet ({} bytes) exceeds peer limit ({MAX_UDP_PAYLOAD} bytes)",
                buf.len(),
            )));
        }
        // The destination rides the Keep frame meta, encoded straight from
        // the IP octets (no string round-trip on the hot datagram path).
        let frame = encode_udp_data_frame(self.sid, buf, addr.ip(), addr.port());
        self.session
            .send_frame(frame)
            .await
            .map_err(MeowError::Io)?;
        Ok(buf.len())
    }

    fn local_addr(&self) -> Result<SocketAddr> {
        Err(MeowError::NotSupported(
            "mux.cool PacketConn has no local UDP addr (UDP-over-mux)".into(),
        ))
    }

    fn close(&self) -> Result<()> {
        // The End frame goes out when the PacketConn drops (EndGuard).
        Ok(())
    }
}

// --- Unit tests -------------------------------------------------------------

#[cfg(test)]
#[path = "../test/mux_muxcool_test.rs"]
mod tests;
