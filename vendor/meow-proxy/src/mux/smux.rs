//! smux protocol version 1, client side.
//!
//! The v1 frame layout is identical across the xtaci and sagernet forks:
//! `ver=1, cmd, len u16 LE, stream_id u32 LE, data` (8-byte header,
//! little-endian), no window-update (UPD) flow control — a v1 write just
//! splits into MaxFrameSize frames. Commands: SYN=0, FIN=1, PSH=2, NOP=3;
//! client stream IDs are odd (1, 3, 5, ...). Session defaults match
//! sing-mux's usage (DefaultConfig + KeepAliveDisabled): Version=1,
//! MaxFrameSize=32768, MaxStreamBuffer=64 KiB, MaxReceiveBuffer=4 MiB —
//! the kcptun path overrides frame size, buffers, and keepalive.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::sync::{mpsc, oneshot, Notify, OwnedSemaphorePermit, Semaphore, TryAcquireError};
use tokio_util::sync::{CancellationToken, PollSender};
use tracing::{debug, info, warn};

const CMD_SYN: u8 = 0;
const CMD_FIN: u8 = 1;
const CMD_PSH: u8 = 2;
const CMD_NOP: u8 = 3;
const SMUX_VERSION: u8 = 1;
const FRAME_HEADER_LEN: usize = 8;

/// Largest frame payload — sagernet smux MaxFrameSize default (u16 length).
const MAX_FRAME_SIZE: usize = 32768;
/// Inbox depth: a bound on the *frame-count overhead* of the per-stream
/// fast-path queue. It is deliberately NOT a retire trigger on its own —
/// a frame count says nothing about whether the consumer is stalled, and
/// retiring on it cut off healthy streams mid-burst (see
/// [`MAX_STREAM_BUFFER`] and [`STREAM_STALL_GRACE`] for the real policy).
const STREAM_QUEUE: usize = 2048;
/// Per-stream spill capacity beyond the inbox, in frames. A consumer that
/// has not drained a single one of `STREAM_QUEUE + SPILL_QUEUE` frames is
/// by construction an adversarial tiny-frame flood, never an ordinary
/// burst — ordinary frames (>= 32 B payload on average) cross the
/// grace-gated byte cap first. See the last-resort retire in the reader.
const SPILL_QUEUE: usize = 2048;
/// Sagernet's default session-wide receive budget.
pub const MAX_RECEIVE_BUFFER: usize = 4 * 1024 * 1024;
/// One stream's share of [`MAX_RECEIVE_BUFFER`]. A stream is retired only
/// once this much of its payload sits undelivered for a whole
/// [`STREAM_STALL_GRACE`] window — see [`stall_watchdog`] for why the
/// decision must not be taken at frame-arrival time. sing-mux's own
/// `MaxStreamBuffer` is 64 KiB, but that is a *blocking* threshold —
/// reusing it as a retire threshold would kill streams for sub-millisecond
/// stalls.
///
/// The divisor has to leave headroom for many concurrent streams, not just
/// a few: `MuxClient::offer` stops consulting `max-streams` once
/// `max-connections` sessions exist, so one session carries an unbounded
/// number of streams. At `/4`, four stalled streams exhausted the whole
/// budget and wedged the reader for every other stream on the session; at
/// `/32` that takes thirty-two.
const MAX_STREAM_BUFFER: usize = MAX_RECEIVE_BUFFER / 32;
/// How long a stream may hold more than its receive share ([`MAX_STREAM_BUFFER`]
/// bytes unread, or a non-empty spill) before it is retired. Retirement is
/// deliberately NOT decided at frame-arrival time: in a buffered burst the
/// reader task drains frames back-to-back without ever yielding to the
/// consumer, so `unread` crosses any byte cap while a perfectly healthy,
/// actively reading consumer is merely waiting to be scheduled. The
/// watchdog re-checks the live watermark when it fires, so a consumer that
/// gets back under its share inside the window — which every ordinary burst
/// does — survives; only a stream still holding more than its share after
/// the full window is cut off to release the budget for its peers.
const STREAM_STALL_GRACE: Duration = Duration::from_millis(500);
/// How long the reader waits for session receive budget before giving up on
/// the session. Nothing below the mux layer reclaims budget held by a
/// stalled consumer, so a wedged session cannot recover on its own. The
/// deadline is session-wide, not per frame: it is armed the first time an
/// acquire blocks and cleared only by a successful one, so a trickle of
/// permits cannot keep postponing it forever.
const BUDGET_STALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Bounded outbound queue; stream writes wait when the physical writer lags.
const OUTBOUND_QUEUE: usize = 64;
/// Bounded overflow queue for FIN frames when the writer is backpressured.
const DEFERRED_FIN_QUEUE: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Frame {
    cmd: u8,
    stream_id: u32,
    data: Bytes,
}

impl Frame {
    #[cfg(test)]
    fn encode(&self) -> Bytes {
        encode_frame(self.cmd, self.stream_id, &self.data)
    }

    fn decode_header(buf: &[u8]) -> io::Result<(u8, usize, u32)> {
        if buf.len() < FRAME_HEADER_LEN {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "smux frame header too short",
            ));
        }
        if buf[0] != SMUX_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported smux version: {}", buf[0]),
            ));
        }
        let cmd = buf[1];
        let length = u16::from_le_bytes([buf[2], buf[3]]) as usize;
        let stream_id = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
        Ok((cmd, length, stream_id))
    }
}

async fn drain_frame<R>(
    reader: &mut R,
    cancel: &CancellationToken,
    scratch: &mut [u8],
    mut remaining: usize,
) -> bool
where
    R: AsyncRead + Unpin,
{
    while remaining > 0 {
        let n = remaining.min(scratch.len());
        let result = tokio::select! {
            _ = cancel.cancelled() => return false,
            result = reader.read_exact(&mut scratch[..n]) => result,
        };
        if result.is_err() {
            return false;
        }
        remaining -= n;
    }
    true
}

fn encode_frame(cmd: u8, stream_id: u32, data: &[u8]) -> Bytes {
    // The u16 length field caps single frames; poll_write limits writes to
    // MAX_FRAME_SIZE, so this only guards future call sites.
    debug_assert!(
        data.len() <= u16::MAX as usize,
        "smux frame payload exceeds u16 length"
    );
    let mut out = BytesMut::with_capacity(FRAME_HEADER_LEN + data.len());
    out.put_u8(SMUX_VERSION);
    out.put_u8(cmd);
    out.put_u16_le(data.len() as u16);
    out.put_u32_le(stream_id);
    out.put_slice(data);
    out.freeze()
}

struct InboundChunk {
    data: Bytes,
    /// Releases session receive capacity as the stream consumes or drops data.
    permit: Option<OwnedSemaphorePermit>,
    /// Its stream's undelivered-payload counter; see [`StreamEntry::unread`].
    unread: Arc<AtomicUsize>,
}

impl InboundChunk {
    fn advance(&mut self, n: usize) {
        self.data.advance(n);
        self.unread.fetch_sub(n, Ordering::Release);
        if let Some(permit) = &mut self.permit {
            drop(
                permit
                    .split(n)
                    .expect("inbound permit must cover unread payload"),
            );
        }
    }
}

impl Drop for InboundChunk {
    fn drop(&mut self) {
        // `advance` already accounted for the bytes it handed over, so only
        // the unread remainder is left. Together the two subtract exactly
        // what the reader added when it enqueued the frame.
        if !self.data.is_empty() {
            self.unread.fetch_sub(self.data.len(), Ordering::Release);
        }
    }
}

/// Per-stream receive-side accounting and overflow parking, shared between
/// the reader (producer) and the stream's consumer.
///
/// The `unread` counter lives in its own `Arc` (not inside this struct)
/// because [`InboundChunk`] shares it: parking a chunk that held an
/// `Arc<StreamStats>` would form a cycle (`StreamStats::spill` → chunk →
/// `Arc<StreamStats>`) and leak the chunk's session-budget permits forever.
struct StreamStats {
    /// Payload bytes enqueued for this stream and not yet handed to its
    /// consumer. Retiring is gated on this passing the stream's share
    /// ([`stream_buffer`]) for a whole [`STREAM_STALL_GRACE`] window,
    /// which keeps one stream from monopolising the session budget while
    /// transient consumer pauses — bursts, scheduler hiccups — ride
    /// through untouched.
    unread: Arc<AtomicUsize>,
    /// This stream's share of the session receive budget: the
    /// [`MAX_STREAM_BUFFER`] default for multi-stream sessions, or the
    /// whole budget when the session is single-stream by construction.
    stream_buffer: usize,
    /// Overflow parking for chunks that arrive while the inbox is full.
    /// FIFO is preserved by the reader (once the spill is non-empty every
    /// later chunk parks there too) and by the consumer (which drains the
    /// inbox to exhaustion before touching the spill).
    spill: Mutex<VecDeque<InboundChunk>>,
    /// Single-watchdog gate: the reader arms exactly one watchdog per
    /// stream with a CAS on this flag; whichever watchdog owns the `true`
    /// value is responsible for clearing it on exit.
    armed: AtomicBool,
    /// Wakes the consumer when a chunk is parked in the spill. The inbox
    /// channel has its own wake-up path; this covers only the spill.
    notify: Notify,
}

impl StreamStats {
    fn pop_spill(&self) -> Option<InboundChunk> {
        self.spill.lock().pop_front()
    }

    /// The retire condition: the stream holds more than its share of the
    /// receive budget, or is carrying parked overflow, right now. Arming
    /// uses the same predicate, so the watchdog's re-check is exact.
    fn over_share(&self) -> bool {
        self.unread.load(Ordering::Acquire) > self.stream_buffer || !self.spill.lock().is_empty()
    }
}

#[derive(Clone)]
struct StreamEntry {
    tx: mpsc::Sender<InboundChunk>,
    aborted: Arc<AtomicBool>,
    stats: Arc<StreamStats>,
}

/// One outbound writer request.  Flush requests travel the SAME bounded
/// channel as data frames so a flush can never overtake frames the
/// requesting stream queued earlier (FIFO guarantee).  `Clone` keeps
/// `PollSender<OutMsg>` usable (its item type must be `Clone`).
#[derive(Clone)]
enum OutMsg {
    Frame(Bytes),
    /// Acknowledge after the physical transport's buffered data reached
    /// the wire.  `Arc` keeps the item `Clone` for `PollSender`.
    Flush(Arc<oneshot::Sender<()>>),
}

struct SessionState {
    /// Streams keyed by stream ID; dropping the sender EOFs the stream.
    streams: Mutex<HashMap<u32, StreamEntry>>,
    dead: AtomicBool,
    /// Millis-since-epoch of the last inbound frame — the smux
    /// `KeepAliveTimeout` input. Only written when a session is built
    /// with keepalive; the epoch in [`now_ms`] is process-wide.
    last_recv_ms: AtomicU64,
}

/// Milliseconds on a process-wide monotonic epoch for keepalive timestamps.
fn now_ms() -> u64 {
    static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    EPOCH
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64
}

/// smux session over one physical connection (client role).
pub struct Session {
    state: Arc<SessionState>,
    writer_tx: mpsc::Sender<OutMsg>,
    deferred_fin_tx: mpsc::Sender<Bytes>,
    cancel: CancellationToken,
    next_stream_id: AtomicU32,
    /// Per-stream receive share past which the stall watchdog may retire
    /// the stream — [`MAX_STREAM_BUFFER`] by default; single-stream
    /// sessions (gost-plugin) may grant the whole budget.
    stream_buffer: usize,
    /// Outbound frame payload cap — [`MAX_FRAME_SIZE`] (32768) for sing-mux,
    /// `frameSize` (8192) for kcptun whose peers reject larger frames.
    max_frame_size: usize,
}

impl Session {
    /// True once the session is dead (reader/writer task ended or the
    /// physical connection failed).
    pub fn is_dead(&self) -> bool {
        self.state.dead.load(Ordering::SeqCst)
    }

    /// Start an smux client session over the IO.  A reader task owns the
    /// read half; a writer task serialises outbound frames.
    pub fn client<S>(io: S) -> io::Result<Self>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        Self::client_with_stream_buffer(io, MAX_STREAM_BUFFER)
    }

    /// [`client`](Self::client) with a custom per-stream receive share.
    /// The `MAX_STREAM_BUFFER` divisor exists to protect sessions
    /// carrying many streams; a session that is single-stream by
    /// construction may pass the whole [`MAX_RECEIVE_BUFFER`].
    pub fn client_with_stream_buffer<S>(io: S, stream_buffer: usize) -> io::Result<Self>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        Ok(Self::client_inner_with_budget(
            io,
            stream_buffer,
            None,
            MAX_FRAME_SIZE,
            MAX_RECEIVE_BUFFER,
        ))
    }

    /// [`client_with_stream_buffer`](Self::client_with_stream_buffer) with
    /// smux keepalive (`KeepAliveInterval`/`KeepAliveTimeout` upstream): a
    /// NOP frame goes out every `interval`, and the session is declared
    /// dead once no inbound frame has arrived for `timeout`. A zero
    /// `interval` or `timeout` disables the task rather than panicking on
    /// `tokio::time::interval(Duration::ZERO)`.
    pub fn client_with_keepalive<S>(
        io: S,
        stream_buffer: usize,
        interval: Duration,
        timeout: Duration,
    ) -> io::Result<Self>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        Ok(Self::client_inner_with_budget(
            io,
            stream_buffer,
            Some((interval, timeout)),
            MAX_FRAME_SIZE,
            MAX_RECEIVE_BUFFER,
        ))
    }

    /// [`client_with_keepalive`](Self::client_with_keepalive) plus the
    /// remaining upstream smux knobs kcptun exposes: `max_frame_size` caps
    /// outbound frame payloads (`frameSize`, 8192 upstream) and
    /// `receive_budget` sizes the session-wide inbound window (`smuxbuf`,
    /// [`MAX_RECEIVE_BUFFER`] by default).
    pub fn client_kcptun<S>(
        io: S,
        stream_buffer: usize,
        keepalive: Duration,
        keepalive_timeout: Duration,
        max_frame_size: usize,
        receive_budget: usize,
    ) -> io::Result<Self>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        Ok(Self::client_inner_with_budget(
            io,
            stream_buffer,
            Some((keepalive, keepalive_timeout)),
            max_frame_size,
            receive_budget,
        ))
    }

    fn client_inner_with_budget<S>(
        io: S,
        stream_buffer: usize,
        keepalive: Option<(Duration, Duration)>,
        max_frame_size: usize,
        receive_budget_size: usize,
    ) -> Self
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (mut reader, mut writer) = tokio::io::split(io);
        let (writer_tx, mut writer_rx) = mpsc::channel::<OutMsg>(OUTBOUND_QUEUE);
        let (deferred_fin_tx, mut deferred_fin_rx) = mpsc::channel(DEFERRED_FIN_QUEUE);
        let state = Arc::new(SessionState {
            streams: Mutex::new(HashMap::new()),
            dead: AtomicBool::new(false),
            // Seed at session start: a peer that sends nothing yet still
            // gets the full first timeout window before being declared dead.
            last_recv_ms: AtomicU64::new(now_ms()),
        });
        let receive_budget = Arc::new(Semaphore::new(receive_budget_size.max(1)));
        let cancel = CancellationToken::new();

        // A single forwarder waits for writer capacity, preserving the
        // original FIFO without spawning one task per dropped stream.
        let deferred_writer_tx = writer_tx.clone();
        let deferred_cancel = cancel.clone();
        tokio::spawn(async move {
            while let Some(frame) = tokio::select! {
                _ = deferred_cancel.cancelled() => None,
                frame = deferred_fin_rx.recv() => frame,
            } {
                if tokio::select! {
                    _ = deferred_cancel.cancelled() => false,
                    sent = deferred_writer_tx.send(OutMsg::Frame(frame)) => sent.is_ok(),
                } {
                    continue;
                }
                break;
            }
        });

        // Writer task: outbound frames are already encoded, so the hot path
        // performs no second allocation or payload copy.
        let writer_state = Arc::clone(&state);
        let writer_cancel = cancel.clone();
        tokio::spawn(async move {
            loop {
                let msg = tokio::select! {
                    _ = writer_cancel.cancelled() => break,
                    msg = writer_rx.recv() => match msg {
                        Some(msg) => msg,
                        None => break,
                    },
                };
                match msg {
                    OutMsg::Frame(frame) => {
                        let result = tokio::select! {
                            _ = writer_cancel.cancelled() => break,
                            result = writer.write_all(&frame) => result,
                        };
                        if result.is_err() {
                            break;
                        }
                        // Buffered transports (snappy over kcptun) emit only
                        // on flush — a frame stranded in the encoder's input
                        // accumulator never reaches the wire, which would
                        // silently drop keepalive NOPs and stream FINs.
                        // Flush once the write queue drains: upstream's
                        // compStream flushes per write, and queued frames
                        // will each flush the backlog behind them anyway.
                        if writer_rx.is_empty() {
                            let result = tokio::select! {
                                _ = writer_cancel.cancelled() => break,
                                result = writer.flush() => result,
                            };
                            if result.is_err() {
                                break;
                            }
                        }
                    }
                    OutMsg::Flush(ack) => {
                        // FIFO: every frame queued before this request
                        // was received first (same channel) and is
                        // already written — flush the transport now.
                        // Cancellation must break out even of a stalled
                        // flush, otherwise a dropped session could keep
                        // the writer task (and the socket) alive.
                        let result = tokio::select! {
                            _ = writer_cancel.cancelled() => break,
                            result = writer.flush() => result,
                        };
                        if result.is_err() {
                            break;
                        }
                        // The channel delivered the sole Arc reference;
                        // unwrap and ack.  A stray clone would still
                        // resolve the receiver (with an error) when
                        // this one drops, never hanging the stream.
                        let _ = Arc::try_unwrap(ack).map(|sender| sender.send(()));
                    }
                }
            }
            writer_state.mark_dead();
            writer_cancel.cancel();
        });

        // Reader task: frame dispatch never awaits a stream inbox, so one
        // stalled consumer cannot stop its peers receiving data. A stream is
        // retired only once it holds more than its share of the receive
        // budget — a consumer that merely paused is buffered, not cut off.
        let reader_state = Arc::clone(&state);
        let reader_cancel = cancel.clone();
        let reader_writer_tx = writer_tx.clone();
        let reader_deferred_fin_tx = deferred_fin_tx.clone();
        tokio::spawn(async move {
            let mut header = [0u8; FRAME_HEADER_LEN];
            let mut discard = [0u8; 8 * 1024];
            // Expiry of the session-wide budget-stall deadline; `None` while
            // the reader is making progress. See `BUDGET_STALL_TIMEOUT`.
            let mut budget_deadline: Option<tokio::time::Instant> = None;
            'reader: loop {
                let header_result = tokio::select! {
                    _ = reader_cancel.cancelled() => break,
                    result = reader.read_exact(&mut header) => result,
                };
                if header_result.is_err() {
                    break;
                }
                let Ok((cmd, length, stream_id)) = Frame::decode_header(&header) else {
                    break;
                };
                // Any decodable frame — data or a bare NOP — proves the
                // peer's side of the session is still forwarding.
                reader_state.last_recv_ms.store(now_ms(), Ordering::Relaxed);
                match cmd {
                    CMD_PSH if length == 0 => continue,
                    CMD_PSH => {
                        let Some(entry) = reader_state.stream(stream_id) else {
                            if !drain_frame(&mut reader, &reader_cancel, &mut discard, length).await
                            {
                                break 'reader;
                            }
                            continue;
                        };
                        if entry.tx.is_closed() {
                            if !drain_frame(&mut reader, &reader_cancel, &mut discard, length).await
                            {
                                break 'reader;
                            }
                            continue;
                        }
                        // The session receive budget is the hard bound on
                        // bytes in flight; the per-stream share is enforced
                        // by the stall watchdog AFTER this frame is
                        // delivered, never here — at arrival time a healthy
                        // consumer that simply has not been scheduled yet is
                        // indistinguishable from a stalled one.
                        // Try without blocking first: arming a timer on every
                        // frame would tax the hot path, and the budget is
                        // uncontended in the common case.
                        let permit = match Arc::clone(&receive_budget)
                            .try_acquire_many_owned(length as u32)
                        {
                            Ok(permit) => {
                                // Uncontended, so the session has real
                                // headroom: this is the only progress signal
                                // that clears the stall deadline.
                                budget_deadline = None;
                                permit
                            }
                            Err(TryAcquireError::Closed) => break 'reader,
                            Err(TryAcquireError::NoPermits) => {
                                // One deadline shared by every consecutive
                                // blocked frame. A blocking acquire that
                                // eventually succeeds deliberately leaves it
                                // armed: scraping by means the budget is still
                                // saturated, and clearing on that would let a
                                // trickle of permits postpone abandonment
                                // forever, parking the session in a near-wedged
                                // state the pool still considers healthy.
                                let deadline = *budget_deadline.get_or_insert_with(|| {
                                    tokio::time::Instant::now() + BUDGET_STALL_TIMEOUT
                                });
                                let remaining =
                                    deadline.saturating_duration_since(tokio::time::Instant::now());
                                tokio::select! {
                                    _ = reader_cancel.cancelled() => break 'reader,
                                    result = Arc::clone(&receive_budget)
                                        .acquire_many_owned(length as u32) => match result {
                                        Ok(permit) => permit,
                                        Err(_) => break 'reader,
                                    },
                                    // Every budget holder has stalled. The
                                    // budget lives in chunks queued inside
                                    // inboxes whose consumers are not polling,
                                    // and the relay stops polling a read half
                                    // whose write half is blocked — so no layer
                                    // below can reclaim it and this session will
                                    // never dispatch again. Abandon it: the
                                    // physical connection closes, the server
                                    // releases its side, and the pool opens a
                                    // fresh session.
                                    _ = tokio::time::sleep(remaining) => {
                                        warn!(
                                            "smux: session receive budget \
                                             ({receive_budget_size} bytes) saturated for \
                                             {BUDGET_STALL_TIMEOUT:?}; abandoning session"
                                        );
                                        break 'reader;
                                    }
                                }
                            }
                        };
                        let mut payload = vec![0u8; length];
                        let payload_result = tokio::select! {
                            _ = reader_cancel.cancelled() => break,
                            result = reader.read_exact(&mut payload) => result,
                        };
                        if payload_result.is_err() {
                            break;
                        }
                        entry.stats.unread.fetch_add(length, Ordering::Release);
                        // Fast path: the inbox, whose bounded depth caps
                        // queue *overhead*. A full inbox parks the chunk in
                        // the spill instead of retiring the stream — FIFO is
                        // preserved because later chunks park too while the
                        // spill is non-empty, and the consumer drains the
                        // inbox first.
                        let chunk = InboundChunk {
                            data: Bytes::from(payload),
                            permit: Some(permit),
                            unread: Arc::clone(&entry.stats.unread),
                        };
                        let mut spill = entry.stats.spill.lock();
                        let parked = if spill.is_empty() {
                            match entry.tx.try_send(chunk) {
                                // `Ok`: parked in the inbox. `Closed`: the
                                // consumer is gone and `InboundChunk::drop`
                                // releases the budget permits and unread
                                // accounting — either way, no spill parking.
                                Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => false,
                                Err(mpsc::error::TrySendError::Full(chunk)) => {
                                    spill.push_back(chunk);
                                    true
                                }
                            }
                        } else {
                            spill.push_back(chunk);
                            true
                        };
                        let spill_len = spill.len();
                        drop(spill);
                        if parked {
                            entry.stats.notify.notify_one();
                        }
                        // Last-resort retire, the ONLY synchronous one: more
                        // than inbox+spill frames parked against a consumer
                        // that has not drained one of them is an adversarial
                        // tiny-frame flood (see [`SPILL_QUEUE`]). Ordinary
                        // traffic crosses the grace-gated byte cap long
                        // before this fires.
                        if spill_len > SPILL_QUEUE {
                            retire_stream(
                                &entry.aborted,
                                &entry.stats,
                                &reader_state,
                                &reader_writer_tx,
                                &reader_deferred_fin_tx,
                                stream_id,
                                "receive queue exhausted by frame flood",
                            );
                            continue;
                        }
                        // Arm the per-stream stall watchdog when this stream
                        // crosses its receive share. The CAS keeps one
                        // watchdog per stream; the running one owns the
                        // re-arm until it exits.
                        let over_share = entry.stats.unread.load(Ordering::Acquire)
                            > entry.stats.stream_buffer
                            || parked;
                        if over_share && !entry.stats.armed.swap(true, Ordering::AcqRel) {
                            let aborted = Arc::clone(&entry.aborted);
                            let stats = Arc::clone(&entry.stats);
                            let state = Arc::clone(&reader_state);
                            let writer_tx = reader_writer_tx.clone();
                            let deferred_fin_tx = reader_deferred_fin_tx.clone();
                            tokio::spawn(stall_watchdog(
                                aborted,
                                stats,
                                state,
                                writer_tx,
                                deferred_fin_tx,
                                stream_id,
                            ));
                        }
                    }
                    CMD_FIN | CMD_NOP => {
                        // Upstream smux treats a control frame WITH a
                        // payload as ErrInvalidProtocol and kills the
                        // session outright — draining-and-continuing
                        // would silently desync the frame boundary.
                        if length > 0 {
                            warn!(
                                "smux: protocol error: {} frame carries {length}B payload; \
                                 dropping session",
                                if cmd == CMD_FIN { "FIN" } else { "NOP" }
                            );
                            break 'reader;
                        }
                        if cmd == CMD_FIN {
                            // Peer half-closed: EOF the read side by dropping
                            // the sender.
                            reader_state.remove_stream(stream_id);
                        }
                    }
                    // v1 has no UPD; a server-initiated SYN is unexpected in
                    // client-only usage and anything else is a protocol error.
                    // Reject before allocating payload.
                    CMD_SYN => {
                        warn!("smux: protocol error: unexpected SYN from server; dropping session");
                        break 'reader;
                    }
                    other => {
                        warn!("smux: protocol error: unknown command {other}; dropping session");
                        break 'reader;
                    }
                }
            }
            reader_state.mark_dead();
            reader_cancel.cancel();
        });

        // Keepalive task (xtaci/smux `keepalive`): emit a NOP every
        // `interval`; declare the session dead once `timeout` (upstream's
        // `KeepAliveTimeout`, independent of the interval) passes with no
        // inbound frame. Both failure modes funnel through `mark_dead` +
        // `cancel`, so a dead writer or a silent peer ends the session the
        // same way the reader's exit does. Zero disables the task.
        if let Some((interval, timeout)) = keepalive.filter(|(i, t)| !i.is_zero() && !t.is_zero()) {
            let ka_state = Arc::clone(&state);
            let ka_cancel = cancel.clone();
            let ka_writer_tx = writer_tx.clone();
            tokio::spawn(async move {
                // `interval` ticks once immediately — upstream's first NOP
                // goes out a full interval after session start.
                let mut ticker =
                    tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
                loop {
                    tokio::select! {
                        _ = ka_cancel.cancelled() => break,
                        _ = ticker.tick() => {}
                    }
                    let silence = Duration::from_millis(
                        now_ms().saturating_sub(ka_state.last_recv_ms.load(Ordering::Relaxed)),
                    );
                    if silence > timeout {
                        debug!("smux: no inbound frame for {silence:?}; declaring session dead");
                        break;
                    }
                    // sid 0 — NOPs are session-scoped, not stream-scoped.
                    let sent = ka_writer_tx
                        .send(OutMsg::Frame(encode_frame(CMD_NOP, 0, &[])))
                        .await
                        .is_ok();
                    if !sent {
                        break;
                    }
                }
                ka_state.mark_dead();
                ka_cancel.cancel();
            });
        }

        Self {
            state,
            writer_tx,
            deferred_fin_tx,
            cancel,
            next_stream_id: AtomicU32::new(1),
            stream_buffer,
            // Never above the u16 length field; zero means the wire cap.
            max_frame_size: max_frame_size.clamp(1, u16::MAX as usize),
        }
    }

    /// Open a new stream.  Client IDs count up by 2 from 1.
    pub async fn open_stream(self: &Arc<Self>) -> io::Result<SmuxStream> {
        // The u32 counter wraps after ~2^31 opens on one session; a
        // colliding id must not silently replace a live stream entry.
        let id = loop {
            let id = self.next_stream_id.fetch_add(2, Ordering::Relaxed);
            if !self.state.streams.lock().contains_key(&id) {
                break id;
            }
        };
        let (tx, rx) = mpsc::channel(STREAM_QUEUE);
        let aborted = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(StreamStats {
            unread: Arc::new(AtomicUsize::new(0)),
            stream_buffer: self.stream_buffer,
            spill: Mutex::new(VecDeque::new()),
            armed: AtomicBool::new(false),
            notify: Notify::new(),
        });
        self.state.register_stream(
            id,
            StreamEntry {
                tx,
                aborted: Arc::clone(&aborted),
                stats: Arc::clone(&stats),
            },
        )?;
        // The guard removes the map entry if this future is cancelled
        // while waiting for outbound capacity — otherwise the id stays
        // consumed and the entry lingers until a server FIN.
        let mut guard = MapEntryGuard {
            state: Arc::clone(&self.state),
            id,
            armed: true,
        };
        if self
            .writer_tx
            .send(OutMsg::Frame(encode_frame(CMD_SYN, id, &[])))
            .await
            .is_err()
        {
            drop(guard);
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "smux writer gone",
            ));
        }
        if self.is_dead() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "smux session is closed",
            ));
        }
        guard.disarm();
        Ok(SmuxStream {
            id,
            session: Arc::clone(self),
            poll_sender: PollSender::new(self.writer_tx.clone()),
            rx,
            pending: None,
            eof: false,
            fin_sent: false,
            shutdown_frame: None,
            flush_rx: None,
            write_since_flush: false,
            aborted,
            stats,
        })
    }
}

/// Removes a stream-map entry when dropped while armed — the
/// cancellation-safe counterpart of `open_stream`'s insert-then-send.
struct MapEntryGuard {
    state: Arc<SessionState>,
    id: u32,
    armed: bool,
}

impl MapEntryGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for MapEntryGuard {
    fn drop(&mut self) {
        if self.armed {
            self.state.remove_stream(self.id);
        }
    }
}

impl Session {
    /// Tear the session down now even while streams still hold it: marks it
    /// dead (every stream fails) and stops the reader/writer/keepalive tasks,
    /// which drops the physical connection. Used by pool resets when the
    /// outbound-interface binding changes (issue #695). Idempotent.
    pub fn close(&self) {
        self.state.mark_dead();
        self.cancel.cancel();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.close();
    }
}

fn queue_fin(
    writer_tx: &mpsc::Sender<OutMsg>,
    deferred_fin_tx: &mpsc::Sender<Bytes>,
    stream_id: u32,
) {
    let frame = encode_frame(CMD_FIN, stream_id, &[]);
    match writer_tx.try_send(OutMsg::Frame(frame.clone())) {
        // Closed means the writer already broke out and marked the session dead.
        Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => return,
        Err(mpsc::error::TrySendError::Full(_)) => {}
    }
    match deferred_fin_tx.try_send(frame) {
        Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            // Both bounded queues are full, so the physical writer is wedged.
            // Dropping this FIN leaks one server-side stream until the session
            // ends; tearing the session down here would kill every healthy
            // stream on it, and this path is reachable from `Drop`.
            debug!("smux: dropping FIN for stream {stream_id}: outbound queues full");
        }
    }
}

/// Retire one stream: mark it aborted so its consumer surfaces an error
/// instead of a clean EOF, drop its map entry, and tell the peer. The
/// session and every other stream are untouched. Data already buffered in
/// the stream's inbox and spill is still delivered before the error shows.
fn retire_stream(
    aborted: &AtomicBool,
    stats: &StreamStats,
    state: &SessionState,
    writer_tx: &mpsc::Sender<OutMsg>,
    deferred_fin_tx: &mpsc::Sender<Bytes>,
    stream_id: u32,
    reason: &str,
) {
    aborted.store(true, Ordering::Release);
    info!(
        "smux: retiring stream {stream_id}: {reason} ({} bytes undelivered)",
        stats.unread.load(Ordering::Relaxed)
    );
    state.remove_stream(stream_id);
    queue_fin(writer_tx, deferred_fin_tx, stream_id);
}

/// Per-stream stall watchdog, armed by the reader (CAS on
/// [`StreamStats::armed`]) when a stream crosses its receive share.
///
/// Retirement must not be decided at frame-arrival time: in one buffered
/// burst the reader task processes frames back-to-back without yielding to
/// the consumer, so `unread` crosses any byte cap while a healthy,
/// actively reading consumer is merely unscheduled — cutting the stream
/// there is the exact regression this watchdog exists to prevent. Instead
/// it re-checks the live watermark ([`StreamStats::over_share`]) after a
/// full [`STREAM_STALL_GRACE`] window:
///
/// * drained back under the share inside the window — every ordinary
///   burst — the watchdog disarms (with a re-check that closes the race
///   against a concurrent re-arm by the reader) and the stream survives;
/// * still holding more than the share after the full window — the
///   consumer is stalled or structurally slower than the peer, with no
///   v1 flow control to hold it back — the stream is retired so its
///   budget reaches its peers instead of wedging the session until
///   [`BUDGET_STALL_TIMEOUT`] kills it wholesale.
async fn stall_watchdog(
    aborted: Arc<AtomicBool>,
    stats: Arc<StreamStats>,
    state: Arc<SessionState>,
    writer_tx: mpsc::Sender<OutMsg>,
    deferred_fin_tx: mpsc::Sender<Bytes>,
    stream_id: u32,
) {
    loop {
        tokio::time::sleep(STREAM_STALL_GRACE).await;
        // The stream may have been closed, FIN'd, or retired (by an earlier
        // watchdog or the frame-flood last resort) while we slept: nothing
        // left to guard. Deliberately checked BEFORE the share check so a
        // post-FIN consumer draining slowly is never aborted — its budget
        // release is bounded by BUDGET_STALL_TIMEOUT instead.
        if state.dead.load(Ordering::SeqCst) || state.stream(stream_id).is_none() {
            stats.armed.store(false, Ordering::Release);
            return;
        }
        if !stats.over_share() {
            // Drained inside the window: a burst, not a stall. Clear, then
            // re-check: if the reader armed a fresh watchdog in between,
            // that one owns the watch and we exit; if the share was crossed
            // again first, we re-own it and watch another full window.
            stats.armed.store(false, Ordering::Release);
            if stats.over_share() && !stats.armed.swap(true, Ordering::AcqRel) {
                continue;
            }
            return;
        }
        retire_stream(
            &aborted,
            &stats,
            &state,
            &writer_tx,
            &deferred_fin_tx,
            stream_id,
            "consumer stalled: undelivered payload held over its receive share",
        );
        // Leave `armed` set: the map entry is gone so the reader can never
        // arm again, and a stale `true` keeps any racing path from
        // spawning a second watchdog for this dead stream.
        return;
    }
}

impl SessionState {
    fn register_stream(&self, id: u32, entry: StreamEntry) -> io::Result<()> {
        let mut streams = self.streams.lock();
        if self.dead.load(Ordering::SeqCst) {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "smux session is closed",
            ));
        }
        streams.insert(id, entry);
        Ok(())
    }

    fn stream(&self, stream_id: u32) -> Option<StreamEntry> {
        self.streams.lock().get(&stream_id).cloned()
    }

    fn remove_stream(&self, stream_id: u32) {
        self.streams.lock().remove(&stream_id);
    }

    fn mark_dead(&self) {
        self.dead.store(true, Ordering::SeqCst);
        self.streams.lock().clear();
    }
}

/// One multiplexed stream (AsyncRead + AsyncWrite).
pub struct SmuxStream {
    id: u32,
    session: Arc<Session>,
    poll_sender: PollSender<OutMsg>,
    rx: mpsc::Receiver<InboundChunk>,
    /// Unread remainder of the last chunk handed to poll_read.
    pending: Option<InboundChunk>,
    eof: bool,
    fin_sent: bool,
    shutdown_frame: Option<Bytes>,
    /// Pending flush acknowledgement; see `poll_flush`.
    flush_rx: Option<oneshot::Receiver<()>>,
    /// Set when a data frame is queued after the current flush request was
    /// sent: the flush ack then no longer covers every write, so
    /// `poll_flush` must send another request once the pending one lands.
    write_since_flush: bool,
    aborted: Arc<AtomicBool>,
    /// Receive-side accounting + overflow parking shared with the reader;
    /// see [`StreamStats`].
    stats: Arc<StreamStats>,
}

impl SmuxStream {
    fn best_effort_fin(&mut self) {
        if self.fin_sent || self.aborted.load(Ordering::Acquire) {
            self.fin_sent = true;
            return;
        }
        self.fin_sent = true;
        queue_fin(
            &self.session.writer_tx,
            &self.session.deferred_fin_tx,
            self.id,
        );
    }
}

impl Drop for SmuxStream {
    fn drop(&mut self) {
        self.best_effort_fin();
        self.session.state.remove_stream(self.id);
    }
}

/// Hand one chunk to the caller's ReadBuf, keeping any unread remainder
/// parked as `pending`. Must return `Ready` — returning `Pending` after
/// writing into the caller's `ReadBuf` would lose those bytes (the buffer
/// is recreated on the next poll).
fn serve_chunk(this: &mut SmuxStream, mut chunk: InboundChunk, buf: &mut ReadBuf<'_>) {
    let n = chunk.data.len().min(buf.remaining());
    buf.put_slice(&chunk.data[..n]);
    chunk.advance(n);
    if !chunk.data.is_empty() {
        this.pending = Some(chunk);
    }
}

impl AsyncRead for SmuxStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        // Every progress path returns Ready immediately: returning Pending
        // after writing into the caller's ReadBuf would lose those bytes
        // (the buffer is recreated on the next poll), violating the
        // AsyncRead contract.  A zero-capacity buffer yields Ready(Ok(()))
        // per the tokio AsyncRead docs (a Pending here would leave
        // `read(&mut [])` parked forever).
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        if let Some(chunk) = this.pending.take() {
            serve_chunk(this, chunk, buf);
            Poll::Ready(Ok(()))
        } else if this.eof {
            Poll::Ready(Ok(()))
        } else {
            loop {
                match this.rx.poll_recv(cx) {
                    Poll::Ready(Some(chunk)) => {
                        if chunk.data.is_empty() {
                            // Zero-length PSH frames carry no payload (flush /
                            // ack signals from the peer); returning Ready with
                            // an empty ReadBuf would read as EOF to consumers.
                            continue;
                        }
                        serve_chunk(this, chunk, buf);
                        return Poll::Ready(Ok(()));
                    }
                    Poll::Ready(None) => {
                        // The inbox channel is closed (server FIN, retire,
                        // session teardown) — everything parked in the spill
                        // is still deliverable, so EOF only after the spill
                        // drains too.
                        match this.stats.pop_spill() {
                            Some(chunk) => {
                                if chunk.data.is_empty() {
                                    continue;
                                }
                                serve_chunk(this, chunk, buf);
                                return Poll::Ready(Ok(()));
                            }
                            None => {
                                // Don't latch `eof` on the error arms —
                                // upstream returns the close error on every
                                // read, not just the first.
                                if this.aborted.load(Ordering::Acquire) {
                                    return Poll::Ready(Err(io::Error::new(
                                        io::ErrorKind::ConnectionAborted,
                                        "smux stream retired: consumer stalled past its receive share",
                                    )));
                                }
                                if this.session.is_dead() {
                                    return Poll::Ready(Err(io::Error::new(
                                        io::ErrorKind::BrokenPipe,
                                        "smux session closed",
                                    )));
                                }
                                this.eof = true;
                                return Poll::Ready(Ok(()));
                            }
                        }
                    }
                    Poll::Pending => {
                        // The inbox is exhausted: serve the spill next (its
                        // chunks are older than anything that can arrive
                        // from now on — the reader only refills the inbox
                        // once the spill is empty). When the spill is empty
                        // too, park on its wake-up so a chunk parked between
                        // the empty check and here cannot be missed:
                        // `notify_one` stores a permit when no one is
                        // waiting, which this poll consumes.
                        match this.stats.pop_spill() {
                            Some(chunk) => {
                                if chunk.data.is_empty() {
                                    continue;
                                }
                                serve_chunk(this, chunk, buf);
                                return Poll::Ready(Ok(()));
                            }
                            None => {
                                let mut notified = std::pin::pin!(this.stats.notify.notified());
                                if notified.as_mut().poll(cx).is_ready() {
                                    // A stored permit or a fresh wake:
                                    // re-check the inbox and spill.
                                    continue;
                                }
                                return Poll::Pending;
                            }
                        }
                    }
                }
            }
        }
    }
}

impl AsyncWrite for SmuxStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let this = self.get_mut();
        if this.aborted.load(Ordering::Acquire) {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "smux stream retired: consumer stalled past its receive share",
            )));
        }
        if this.session.is_dead() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "smux session closed",
            )));
        }
        // A PSH queued behind the FIN would corrupt upstream ordering —
        // `stream.Write` after `Close` is `ErrClosed`.
        if this.fin_sent {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "smux stream closed",
            )));
        }
        let written = buf.len().min(this.session.max_frame_size);
        match this.poll_sender.poll_reserve(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) => Poll::Ready(Err(io::Error::other(error.to_string()))),
            Poll::Ready(Ok(())) => {
                let frame = encode_frame(CMD_PSH, this.id, &buf[..written]);
                this.poll_sender
                    .send_item(OutMsg::Frame(frame))
                    .map_err(|error| io::Error::other(error.to_string()))?;
                this.write_since_flush = true;
                Poll::Ready(Ok(written))
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        // A flush request travels the writer channel behind every frame
        // this stream queued earlier, so the acknowledgement means those
        // bytes reached the physical transport (matters for buffered
        // outer transports like WebSocket).
        if let Some(rx) = &mut this.flush_rx {
            match Pin::new(rx).poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(_)) => {
                    this.flush_rx = None;
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "smux writer gone",
                    )));
                }
                Poll::Ready(Ok(())) => {
                    this.flush_rx = None;
                    if !this.write_since_flush {
                        return Poll::Ready(Ok(()));
                    }
                    // Data was queued after the flush request went out:
                    // fall through and send another flush so the ack
                    // covers those writes too.
                }
            }
        }
        match this.poll_sender.poll_reserve(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) => Poll::Ready(Err(io::Error::other(error.to_string()))),
            Poll::Ready(Ok(())) => {
                let (tx, rx) = oneshot::channel();
                this.poll_sender
                    .send_item(OutMsg::Flush(Arc::new(tx)))
                    .map_err(|error| io::Error::other(error.to_string()))?;
                // Every write queued before this request is now covered.
                this.write_since_flush = false;
                // Poll the fresh receiver once to register the waker
                // before parking.
                let mut rx = rx;
                match Pin::new(&mut rx).poll(cx) {
                    Poll::Pending => {
                        this.flush_rx = Some(rx);
                        Poll::Pending
                    }
                    Poll::Ready(Err(_)) => Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "smux writer gone",
                    ))),
                    Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
                }
            }
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.aborted.load(Ordering::Acquire) {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "smux stream retired: consumer stalled past its receive share",
            )));
        }
        if this.fin_sent {
            return Poll::Ready(Ok(()));
        }
        let frame = this
            .shutdown_frame
            .take()
            .unwrap_or_else(|| encode_frame(CMD_FIN, this.id, &[]));
        match this.poll_sender.poll_reserve(cx) {
            Poll::Pending => {
                this.shutdown_frame = Some(frame);
                Poll::Pending
            }
            Poll::Ready(Err(error)) => Poll::Ready(Err(io::Error::other(error.to_string()))),
            Poll::Ready(Ok(())) => {
                this.poll_sender
                    .send_item(OutMsg::Frame(frame))
                    .map_err(|error| io::Error::other(error.to_string()))?;
                this.fin_sent = true;
                Poll::Ready(Ok(()))
            }
        }
    }
}

#[cfg(test)]
#[path = "../test/mux_smux_test.rs"]
mod tests;
