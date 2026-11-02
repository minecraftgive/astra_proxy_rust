use anyhow::{ensure, Result};
use astra_ipc::Inbound;
use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};

#[path = "inbound/shared_http.rs"]
mod http;
#[path = "inbound/shared_socks5.rs"]
mod socks5;
pub use http::handle_http;
pub use socks5::handle_socks5;

#[path = "inbound/shared_authentication.rs"]
mod authentication;
pub(crate) use authentication::{authorize_connect, http_authorized};
/// Serve a trusted host-provided stream using the original TCP endpoints.
/// These addresses must come from the accepting socket, never client headers.
pub async fn handle_stream<S>(
    client: S,
    app: Arc<crate::App>,
    inbound: Inbound,
    source: SocketAddr,
    local: SocketAddr,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    ensure!(
        matches!(
            inbound,
            Inbound::Socks5 | Inbound::HttpConnect | Inbound::Mixed
        ),
        "stream entry requires a SOCKS5, HTTP or mixed inbound"
    );
    ensure!(
        app.inbound_allowed(source.ip()),
        "inbound source is not allowed"
    );
    let mut client = BufferedStream::new(client);
    let inbound = if inbound == Inbound::Mixed {
        tokio::time::timeout(Duration::from_secs(10), client.prefetch(1)).await??;
        if client.buffer[0] == 5 {
            Inbound::Socks5
        } else {
            Inbound::HttpConnect
        }
    } else {
        inbound
    };
    match inbound {
        Inbound::Socks5 => socks5::handle_stream(client, app, source, local).await,
        Inbound::HttpConnect => http::handle_stream(client, app, source, local).await,
        _ => unreachable!(),
    }
}

pub async fn handle_mixed(client: tokio::net::TcpStream, app: Arc<crate::App>) -> Result<()> {
    let source = client.peer_addr()?;
    let local = client.local_addr()?;
    handle_stream(client, app, Inbound::Mixed, source, local).await
}

/// Bounded handshake lookahead for streams without a socket-specific peek API.
/// Bytes are replayed to the protocol parser, including pipelined tunnel data.
pub(super) struct BufferedStream<S> {
    inner: S,
    buffer: Vec<u8>,
    position: usize,
}

impl<S: AsyncRead + Unpin> BufferedStream<S> {
    fn new(inner: S) -> Self {
        Self {
            inner,
            buffer: Vec::new(),
            position: 0,
        }
    }

    async fn read_more(&mut self, limit: usize) -> io::Result<()> {
        let mut bytes = [0; 2048];
        let size = bytes.len().min(limit.saturating_sub(self.buffer.len()));
        if size == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "proxy request headers exceed limit",
            ));
        }
        let read = self.inner.read(&mut bytes[..size]).await?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed during proxy handshake",
            ));
        }
        self.buffer.extend_from_slice(&bytes[..read]);
        Ok(())
    }

    async fn prefetch(&mut self, count: usize) -> io::Result<()> {
        while self.buffer.len() < count {
            self.read_more(count).await?;
        }
        Ok(())
    }

    async fn headers(&mut self) -> Result<String> {
        loop {
            if let Some(end) = self
                .buffer
                .windows(4)
                .position(|bytes| bytes == b"\r\n\r\n")
            {
                return Ok(String::from_utf8(self.buffer[..end].to_vec())?);
            }
            self.read_more(32 * 1024).await?;
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for BufferedStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.position < this.buffer.len() && output.remaining() > 0 {
            let count = output.remaining().min(this.buffer.len() - this.position);
            output.put_slice(&this.buffer[this.position..this.position + count]);
            this.position += count;
            if this.position == this.buffer.len() {
                this.buffer = Vec::new();
                this.position = 0;
            }
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_read(cx, output)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for BufferedStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, bytes)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write_vectored(cx, bytes)
    }
}

#[cfg(test)]
#[path = "inbound/shared_tests.rs"]
mod tests;
