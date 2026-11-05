//! sing-mux session padding (sagernet/sing-mux `padding.go`).
//!
//! With `padding: true` the client sends a version-1 request header with the
//! padding flag set, and then *both* ends wrap the physical connection in
//! this layer before the smux / yamux / h2mux session starts. The request
//! header itself is not framed. The first [`FIRST_PADDINGS`] writes in each
//! direction go out as
//!
//! ```text
//! [data_len u16 BE][padding_len u16 BE][data][padding_len bytes]
//! ```
//!
//! with `padding_len = 256 + rand(512)` and at most 65535 data bytes per
//! frame. Everything after that passes through unframed.

use rand::Rng;
use std::io;
use std::pin::Pin;
use std::task::{ready, Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Framed writes (and reads) per direction (`kFirstPaddings`).
pub const FIRST_PADDINGS: u8 = 16;
const HEADER_LEN: usize = 4;
/// Padding length is `PADDING_MIN + rand(PADDING_SPAN)`, i.e. 256..=767.
const PADDING_MIN: u16 = 256;
const PADDING_SPAN: u16 = 512;
/// Stack scratch for discarding inbound padding. A sing-mux peer pads at
/// most 767 bytes, so one read usually skips a whole frame's padding.
const SKIP_SCRATCH: usize = 1024;

/// A connection carrying a padded sing-mux session.
///
/// Writes are committed a frame at a time: a frame is built in an internal
/// buffer and reported as written once it is there, so a caller that
/// retries a `Pending` write with different bytes cannot tear or duplicate
/// a frame. The buffer is drained before new data is taken and on
/// flush/shutdown, and released once the padded phase is over.
pub struct PaddingConn<S> {
    inner: S,
    /// Frame headers parsed so far (`readPadding`).
    read_frames: u8,
    /// Header bytes collected across polls.
    read_header: [u8; HEADER_LEN],
    read_header_len: usize,
    /// Data bytes of the current frame not yet returned (`readRemaining`).
    read_remaining: usize,
    /// Padding bytes of the current frame not yet discarded
    /// (`paddingRemaining`).
    padding_remaining: usize,
    /// Frames committed so far (`writePadding`).
    write_frames: u8,
    /// Committed frame bytes; `write_pos` marks how far `inner` took them.
    write_buf: Vec<u8>,
    write_pos: usize,
}

impl<S> PaddingConn<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            read_frames: 0,
            read_header: [0; HEADER_LEN],
            read_header_len: 0,
            read_remaining: 0,
            padding_remaining: 0,
            write_frames: 0,
            write_buf: Vec::new(),
            write_pos: 0,
        }
    }
}

fn truncated(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::UnexpectedEof,
        format!("mux padding: connection closed inside a frame {what}"),
    )
}

impl<S: AsyncRead + Unpin> AsyncRead for PaddingConn<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        loop {
            if this.read_remaining > 0 {
                let limit = this.read_remaining.min(buf.remaining());
                let mut data = ReadBuf::new(buf.initialize_unfilled_to(limit));
                ready!(Pin::new(&mut this.inner).poll_read(cx, &mut data))?;
                let n = data.filled().len();
                if n == 0 {
                    return Poll::Ready(Err(truncated("payload")));
                }
                buf.advance(n);
                this.read_remaining -= n;
                return Poll::Ready(Ok(()));
            }
            if this.padding_remaining > 0 {
                let mut scratch = [0u8; SKIP_SCRATCH];
                let limit = this.padding_remaining.min(SKIP_SCRATCH);
                let mut skip = ReadBuf::new(&mut scratch[..limit]);
                ready!(Pin::new(&mut this.inner).poll_read(cx, &mut skip))?;
                let n = skip.filled().len();
                if n == 0 {
                    return Poll::Ready(Err(truncated("padding")));
                }
                this.padding_remaining -= n;
                continue;
            }
            if this.read_frames >= FIRST_PADDINGS {
                return Pin::new(&mut this.inner).poll_read(cx, buf);
            }
            while this.read_header_len < HEADER_LEN {
                let mut header = ReadBuf::new(&mut this.read_header[this.read_header_len..]);
                ready!(Pin::new(&mut this.inner).poll_read(cx, &mut header))?;
                let n = header.filled().len();
                if n == 0 {
                    // EOF on a frame boundary is a clean close.
                    return Poll::Ready(if this.read_header_len == 0 {
                        Ok(())
                    } else {
                        Err(truncated("header"))
                    });
                }
                this.read_header_len += n;
            }
            let [d0, d1, p0, p1] = this.read_header;
            this.read_header_len = 0;
            this.read_remaining = usize::from(u16::from_be_bytes([d0, d1]));
            this.padding_remaining = usize::from(u16::from_be_bytes([p0, p1]));
            this.read_frames += 1;
            // A zero-length frame must not surface as EOF: loop on to its
            // padding and the next frame.
        }
    }
}

impl<S: AsyncWrite + Unpin> PaddingConn<S> {
    /// Push committed frame bytes into `inner`.
    fn poll_drain(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while self.write_pos < self.write_buf.len() {
            let n = ready!(
                Pin::new(&mut self.inner).poll_write(cx, &self.write_buf[self.write_pos..])
            )?;
            if n == 0 {
                return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
            }
            self.write_pos += n;
        }
        self.write_pos = 0;
        if self.write_frames >= FIRST_PADDINGS {
            // Padded phase over: no frame is ever built again.
            self.write_buf = Vec::new();
        } else {
            self.write_buf.clear();
        }
        Poll::Ready(Ok(()))
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for PaddingConn<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        ready!(this.poll_drain(cx))?;
        if this.write_frames >= FIRST_PADDINGS {
            return Pin::new(&mut this.inner).poll_write(cx, buf);
        }
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        // sing-mux splits a larger write into 65535-byte frames; here each
        // poll commits one frame and the caller's write loop does the rest.
        let len = u16::try_from(buf.len()).unwrap_or(u16::MAX);
        let padding = PADDING_MIN + rand::rng().random_range(0..PADDING_SPAN);
        let frame_len = HEADER_LEN + usize::from(len) + usize::from(padding);
        this.write_buf.reserve(frame_len);
        this.write_buf.extend_from_slice(&len.to_be_bytes());
        this.write_buf.extend_from_slice(&padding.to_be_bytes());
        this.write_buf.extend_from_slice(&buf[..usize::from(len)]);
        this.write_buf.resize(frame_len, 0);
        this.write_frames += 1;
        // The frame is committed. Hand `inner` what it takes now; whatever
        // is left (Pending, or an error that recurs on retry) is drained by
        // the next write/flush/shutdown before anything else.
        let _ = this.poll_drain(cx);
        Poll::Ready(Ok(usize::from(len)))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(this.poll_drain(cx))?;
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(this.poll_drain(cx))?;
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

impl<S: meow_common::ProxyConn> meow_common::ProxyConn for PaddingConn<S> {
    fn remote_destination(&self) -> String {
        self.inner.remote_destination()
    }
}

#[cfg(test)]
#[path = "../test/mux_padding_test.rs"]
mod tests;
