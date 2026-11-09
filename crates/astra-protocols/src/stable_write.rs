//! Adapts streams whose writes must be retried with the identical buffer after `Pending`.
//!
//! meow's gRPC and h2 streams encode a frame on the first `poll_write` and reject a
//! retry whose bytes differ. Tokio's `copy` legally tops up its buffer while a write
//! is pending and retries with a longer slice, which reset gRPC/XHTTP connections
//! under load. Bytes refused with `Pending` are kept here and replayed unchanged.
use std::{
    io,
    pin::Pin,
    task::{ready, Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub struct StableWrite<S> {
    inner: S,
    /// Bytes already reported as written but not yet accepted by `inner`.
    pending: Vec<u8>,
    consumed: usize,
}

impl<S> StableWrite<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            pending: Vec::new(),
            consumed: 0,
        }
    }
}

impl<S: AsyncWrite + Unpin> StableWrite<S> {
    fn drain(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while self.consumed < self.pending.len() {
            let n =
                ready!(Pin::new(&mut self.inner).poll_write(cx, &self.pending[self.consumed..]))?;
            if n == 0 {
                return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
            }
            self.consumed += n;
        }
        // Pending writes are exceptional; do not retain their allocation for
        // the rest of a potentially long-lived connection once drained.
        self.pending = Vec::new();
        self.consumed = 0;
        Poll::Ready(Ok(()))
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for StableWrite<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for StableWrite<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        ready!(this.drain(cx))?;
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        match Pin::new(&mut this.inner).poll_write(cx, buf) {
            Poll::Pending => {
                // Take ownership of these exact bytes; later calls replay them first.
                this.pending.extend_from_slice(buf);
                Poll::Ready(Ok(buf.len()))
            }
            ready => ready,
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(this.drain(cx))?;
        Pin::new(&mut this.inner).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(this.drain(cx))?;
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
#[path = "test/stable_write_test.rs"]
mod tests;
