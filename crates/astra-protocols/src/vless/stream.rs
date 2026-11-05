use std::{
    io,
    pin::Pin,
    task::{ready, Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// VLESS response framing belongs to the downlink. Uplink writes must be usable
/// before the server sends its response (e.g. a destination waiting for ClientHello).
pub(super) struct VlessStream<S> {
    inner: S,
    header: [u8; 2],
    header_read: usize,
    addons_remaining: Option<usize>,
}

impl<S> VlessStream<S> {
    pub(super) fn new(inner: S) -> Self {
        Self {
            inner,
            header: [0; 2],
            header_read: 0,
            addons_remaining: None,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for VlessStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        dst: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if dst.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let this = self.get_mut();
        while this.header_read < 2 {
            let mut buf = ReadBuf::new(&mut this.header[this.header_read..]);
            ready!(Pin::new(&mut this.inner).poll_read(cx, &mut buf))?;
            let n = buf.filled().len();
            if n == 0 {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated VLESS response",
                )));
            }
            this.header_read += n;
        }
        if this.header[0] != 0 {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported VLESS response version",
            )));
        }
        let remaining = this.addons_remaining.get_or_insert(this.header[1] as usize);
        while *remaining > 0 {
            let mut discard = [0; 255];
            let mut buf = ReadBuf::new(&mut discard[..*remaining]);
            ready!(Pin::new(&mut this.inner).poll_read(cx, &mut buf))?;
            let n = buf.filled().len();
            if n == 0 {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated VLESS addons",
                )));
            }
            *remaining -= n;
        }
        Pin::new(&mut this.inner).poll_read(cx, dst)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for VlessStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
#[path = "../test/vless_stream_test.rs"]
mod tests;
