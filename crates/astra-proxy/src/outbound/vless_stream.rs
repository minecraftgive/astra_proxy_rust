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
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn client_first_and_fragmented_response_with_addons() {
        let (client, mut server) = tokio::io::duplex(1);
        let task = tokio::spawn(async move {
            let mut request = [0; 4];
            server.read_exact(&mut request).await.unwrap();
            assert_eq!(&request, b"ping");
            server.write_all(b"\x00\x03abcdata").await.unwrap();
            server.shutdown().await.unwrap();
        });
        let mut stream = VlessStream::new(client);
        stream.write_all(b"ping").await.unwrap();
        let mut payload = Vec::new();
        stream.read_to_end(&mut payload).await.unwrap();
        assert_eq!(payload, b"data");
        task.await.unwrap();
    }

    #[tokio::test]
    async fn rejects_truncated_and_invalid_headers() {
        for bytes in [&b"\x00"[..], &b"\x01\x00"[..], &b"\x00\x02a"[..]] {
            let mut stream = VlessStream::new(bytes);
            assert!(stream.read_u8().await.is_err());
        }
    }
}
