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
}

impl<S> StableWrite<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            pending: Vec::new(),
        }
    }
}

impl<S: AsyncWrite + Unpin> StableWrite<S> {
    fn drain(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while !self.pending.is_empty() {
            let n = ready!(Pin::new(&mut self.inner).poll_write(cx, &self.pending))?;
            if n == 0 {
                return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
            }
            self.pending.drain(..n);
        }
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
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use tokio::io::AsyncWriteExt;

    /// Mimics meow's GunStream: the first write of a frame returns Pending, and a
    /// retry with different bytes is an error.
    #[derive(Default)]
    struct Strict {
        stashed: Option<Vec<u8>>,
        pendings: VecDeque<bool>,
        written: Vec<u8>,
    }
    impl AsyncWrite for Strict {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            if let Some(stashed) = &this.stashed {
                if stashed != buf {
                    return Poll::Ready(Err(io::Error::other("write buffer changed after Pending")));
                }
            } else {
                this.stashed = Some(buf.to_vec());
            }
            if this.pendings.pop_front().unwrap_or(false) {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            this.written.extend(this.stashed.take().unwrap());
            Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn pending_bytes_are_replayed_unchanged_before_new_data() {
        let strict = Strict {
            pendings: [true, true, false, true, false].into(),
            ..Default::default()
        };
        let mut stream = StableWrite::new(strict);
        // The second call presents a longer buffer, as tokio's copy may after Pending.
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert!(matches!(
            Pin::new(&mut stream).poll_write(&mut cx, b"abc"),
            Poll::Ready(Ok(3))
        ));
        stream.write_all(b"defgh").await.unwrap();
        stream.flush().await.unwrap();
        stream.write_all(b"ij").await.unwrap();
        stream.shutdown().await.unwrap();
        assert_eq!(stream.inner.written, b"abcdefghij");
    }
}
