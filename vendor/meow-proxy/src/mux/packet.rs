//! UDP-over-mux packet stream (sing-mux clientPacketConn / serverPacketConn).
//!
//! A UDP flow is one mux stream opened with the stream-request flagUDP bit;
//! its payload is length-prefixed datagrams: `[len u16 BE][data]` in both
//! directions (identical framing to meow's VLESS UDP-over-TCP).  The first
//! server→client byte is still the stream response status (consumed by
//! `MuxStream`), and the stream is bound to the destination carried in the
//! stream request — datagrams carry no per-packet address.

use super::client::{MuxSession, MuxStream};
use bytes::{BufMut, BytesMut};
use meow_common::{MeowError, ProxyPacketConn, Result};
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

/// One UDP flow multiplexed over a mux stream.  Dropping it half-closes
/// the stream and decrements the session's stream count.
pub struct MuxPacketConn {
    reader: Mutex<PacketReader>,
    writer: Mutex<PacketWriter>,
    session: Arc<MuxSession>,
    /// The destination bound in the stream request; every read reports this
    /// as the datagram source (no per-packet addressing in sing-mux UDP).
    destination: SocketAddr,
}

impl MuxPacketConn {
    pub(crate) fn new(
        stream: MuxStream,
        session: Arc<MuxSession>,
        destination: SocketAddr,
    ) -> Self {
        // Split so a blocked read (no upstream datagram yet) cannot
        // head-of-line block writes — the SOCKS5 UDP relay runs the
        // server→client reader on a separate task from client→server
        // writes, and QUIC-style flows write while waiting for reads.
        let (reader, writer) = tokio::io::split(stream);
        Self {
            reader: Mutex::new(PacketReader::new(reader)),
            writer: Mutex::new(PacketWriter::new(writer)),
            session,
            destination,
        }
    }
}

impl Drop for MuxPacketConn {
    fn drop(&mut self) {
        self.session.streams.fetch_sub(1, Ordering::SeqCst);
    }
}

impl MuxPacketConn {
    /// Write one datagram as `[len u16 BE][data]`.
    pub async fn write_datagram(&self, data: &[u8]) -> Result<usize> {
        if data.len() > u16::MAX as usize {
            return Err(MeowError::Proxy(format!(
                "mux: UDP packet ({} bytes) exceeds u16 framing",
                data.len()
            )));
        }
        let mut writer = self.writer.lock().await;
        writer.write_datagram(data).await.map_err(MeowError::Io)?;
        Ok(data.len())
    }

    /// Read one datagram: `[len u16 BE][data]`.  The stream response status
    /// byte is consumed lazily on the first read by `MuxStream`.
    pub async fn read_datagram(&self, buf: &mut [u8]) -> Result<usize> {
        let mut reader = self.reader.lock().await;
        reader.read_datagram(buf).await
    }
}

/// Cancel-safe datagram reader.  Length and payload progress survive a
/// dropped `read_packet` future (callers may cancel a read by dropping
/// the future, e.g. on task abort).
struct PacketReader {
    stream: tokio::io::ReadHalf<MuxStream>,
    len: [u8; 2],
    len_pos: usize,
    payload: Vec<u8>,
    payload_pos: usize,
}

impl PacketReader {
    fn new(stream: tokio::io::ReadHalf<MuxStream>) -> Self {
        Self {
            stream,
            len: [0; 2],
            len_pos: 0,
            payload: Vec::new(),
            payload_pos: 0,
        }
    }

    fn reset(&mut self) {
        self.len_pos = 0;
        self.payload.clear();
        self.payload_pos = 0;
    }

    async fn read_datagram(&mut self, out: &mut [u8]) -> Result<usize> {
        while self.len_pos < self.len.len() {
            let read = self
                .stream
                .read(&mut self.len[self.len_pos..])
                .await
                .map_err(MeowError::Io)?;
            if read == 0 {
                return Err(MeowError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "mux: EOF while reading UDP packet length",
                )));
            }
            self.len_pos += read;
        }

        if self.payload.is_empty() && self.payload_pos == 0 {
            self.payload
                .resize(u16::from_be_bytes(self.len) as usize, 0);
        }
        while self.payload_pos < self.payload.len() {
            let read = self
                .stream
                .read(&mut self.payload[self.payload_pos..])
                .await
                .map_err(MeowError::Io)?;
            if read == 0 {
                return Err(MeowError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "mux: EOF while reading UDP packet payload",
                )));
            }
            self.payload_pos += read;
        }

        let packet_len = self.payload.len();
        if packet_len > out.len() {
            self.reset();
            return Err(MeowError::Proxy(format!(
                "mux: UDP packet ({packet_len} bytes) exceeds read buffer ({} bytes)",
                out.len()
            )));
        }
        out[..packet_len].copy_from_slice(&self.payload);
        self.reset();
        Ok(packet_len)
    }
}

struct PacketWriter<W = tokio::io::WriteHalf<MuxStream>> {
    stream: W,
    /// Cancellation-safe write state: the combined frame (length prefix
    /// and data) and the byte offset already written. A `write_datagram`
    /// future dropped mid-write re-enters and resumes from `offset`
    /// instead of re-writing the length prefix and desyncing the peer
    /// framing. Mirrors `PacketReader`'s persistent-progress pattern.
    pending: Option<(bytes::Bytes, usize)>,
}

impl<W: tokio::io::AsyncWrite + Unpin> PacketWriter<W> {
    fn new(stream: W) -> Self {
        Self {
            stream,
            pending: None,
        }
    }

    async fn write_datagram(&mut self, data: &[u8]) -> std::io::Result<()> {
        // Recovering from a cancelled write: the pending frame owns its
        // bytes, so a leftover frame is never a caller error.
        //
        // - Same data: this call is a retry of the cancelled write —
        //   resume the pending frame from its offset.
        // - Different data, nothing on the wire yet (offset 0): nothing
        //   is committed — discard the stale frame and send the new one.
        // - Different data after a partial write: the length prefix is
        //   committed, so first complete the stale frame on the wire
        //   (preserving peer framing — the cancelled datagram is
        //   delivered late rather than truncated), then frame and send
        //   the new datagram.  Two wire datagrams, no error, no drop.
        //
        // Datagram identity is content-based: after a cancelled partial
        // write, a *new* datagram carrying identical bytes is
        // indistinguishable from a retry of the cancelled one and will
        // be coalesced into a single wire datagram — acceptable under
        // UDP's lossy delivery semantics.
        //
        // Errors surfaced here are always stream I/O errors (the
        // transport died mid-frame), never caller misuse: draining a
        // stale frame on a dead stream reports the underlying write
        // error, and the frame is kept so the state stays consistent
        // if the caller retries anyway.
        if let Some((frame, offset)) = &self.pending {
            if &frame[2..] != data {
                if *offset == 0 {
                    self.pending = None;
                } else {
                    self.drain_pending().await?;
                }
            }
        }

        // Prepare the combined frame on first entry (or after the
        // previous one completed).  Owning the bytes is required because
        // the caller's `&[u8]` may be gone after cancellation.
        if self.pending.is_none() {
            let mut frame = BytesMut::with_capacity(2 + data.len());
            frame.put_u16(data.len() as u16);
            frame.put_slice(data);
            self.pending = Some((frame.freeze(), 0));
        }

        self.drain_pending().await?;
        self.stream.flush().await
    }

    /// Write the pending frame to completion, resuming from the stored
    /// offset if a previous call was cancelled mid-write.  Uses
    /// individual `write` calls (not `write_all`) so each successful
    /// write updates the offset before the next await point —
    /// `write_all` loses its internal progress on cancellation, which
    /// would duplicate already-sent bytes.  On error the frame (and its
    /// offset) survive, so a later call can still resume it.
    async fn drain_pending(&mut self) -> std::io::Result<()> {
        loop {
            let Some((frame, offset)) = self.pending.as_ref() else {
                return Ok(());
            };
            if *offset >= frame.len() {
                self.pending = None;
                return Ok(());
            }
            let n = self.stream.write(&frame[*offset..]).await?;
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "mux: zero-length write during datagram send",
                ));
            }
            self.pending.as_mut().unwrap().1 += n;
        }
    }
}

#[async_trait::async_trait]
impl ProxyPacketConn for MuxPacketConn {
    async fn read_packet(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr)> {
        let n = self.read_datagram(buf).await?;
        Ok((n, self.destination))
    }

    async fn write_packet(&self, buf: &[u8], _addr: &SocketAddr) -> Result<usize> {
        self.write_datagram(buf).await
    }

    fn local_addr(&self) -> Result<SocketAddr> {
        Err(MeowError::NotSupported(
            "MuxPacketConn has no local UDP addr (UDP-over-mux)".into(),
        ))
    }

    fn close(&self) -> Result<()> {
        // The stream half-closes when MuxPacketConn is dropped.
        Ok(())
    }
}
#[cfg(test)]
#[path = "../test/mux_packet_test.rs"]
mod tests;
