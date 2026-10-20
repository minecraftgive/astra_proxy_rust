//! Route-level metadata and byte accounting shared by every forwarding mode.
use astra_ipc::{Inbound, Network, RouteAction, Target};
use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    task::{Context, Poll},
    time::Instant,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

static NEXT_FLOW: AtomicU64 = AtomicU64::new(1);
pub(crate) struct Traffic {
    id: u64,
    source: Option<SocketAddr>,
    target: Target,
    inbound: Inbound,
    network: Network,
    action: RouteAction,
    started: Instant,
    up: AtomicU64,
    down: AtomicU64,
    packets_up: AtomicU64,
    packets_down: AtomicU64,
    failed: AtomicBool,
    connected: AtomicBool,
}
impl Traffic {
    pub(crate) fn new(
        source: Option<SocketAddr>,
        target: &Target,
        inbound: Inbound,
        network: Network,
        action: &RouteAction,
    ) -> Option<Self> {
        if !tracing::enabled!(target: "astra_route", tracing::Level::DEBUG) {
            return None;
        }
        Some(Self {
            id: NEXT_FLOW.fetch_add(1, Ordering::Relaxed),
            source,
            target: target.clone(),
            inbound,
            network,
            action: action.clone(),
            started: Instant::now(),
            up: AtomicU64::new(0),
            down: AtomicU64::new(0),
            packets_up: AtomicU64::new(0),
            packets_down: AtomicU64::new(0),
            failed: AtomicBool::new(false),
            connected: AtomicBool::new(false),
        })
    }
    pub(crate) fn connected(&self) {
        self.connected.store(true, Ordering::Relaxed);
        tracing::debug!(target: "astra_route", flow = self.id, phase = "forward", source = ?self.source, host = ?self.target.host, port = self.target.port, inbound = ?self.inbound, network = ?self.network, action = ?self.action, "forwarding started");
    }
    pub(crate) fn upload(&self, bytes: usize) {
        self.up.fetch_add(bytes as u64, Ordering::Relaxed);
        if self.network == Network::Udp {
            self.packets_up.fetch_add(1, Ordering::Relaxed);
        }
    }
    pub(crate) fn download(&self, bytes: usize) {
        self.down.fetch_add(bytes as u64, Ordering::Relaxed);
        if self.network == Network::Udp {
            self.packets_down.fetch_add(1, Ordering::Relaxed);
        }
    }
    pub(crate) fn failed(&self, error: impl std::fmt::Display) {
        if !self.failed.swap(true, Ordering::Relaxed) {
            tracing::debug!(target: "astra_route", flow = self.id, phase = "failed", source = ?self.source, host = ?self.target.host, port = self.target.port, inbound = ?self.inbound, network = ?self.network, action = ?self.action, error = %error, "forwarding failed");
        }
    }
}
impl Drop for Traffic {
    fn drop(&mut self) {
        let outcome = if self.failed.load(Ordering::Relaxed) {
            "failed"
        } else if self.connected.load(Ordering::Relaxed) {
            "closed"
        } else {
            "cancelled"
        };
        let up = self.up.load(Ordering::Relaxed);
        let down = self.down.load(Ordering::Relaxed);
        let elapsed_ms = self.started.elapsed().as_millis() as u64;
        if self.network == Network::Udp {
            tracing::debug!(target: "astra_route", flow = self.id, phase = "closed", source = ?self.source, host = ?self.target.host, port = self.target.port, inbound = ?self.inbound, network = ?self.network, action = ?self.action, outcome, up, down, packets_up = self.packets_up.load(Ordering::Relaxed), packets_down = self.packets_down.load(Ordering::Relaxed), elapsed_ms, "forwarding ended");
        } else {
            tracing::debug!(target: "astra_route", flow = self.id, phase = "closed", source = ?self.source, host = ?self.target.host, port = self.target.port, inbound = ?self.inbound, network = ?self.network, action = ?self.action, outcome, up, down, elapsed_ms, "forwarding ended");
        }
    }
}

pub(crate) struct Stream<S> {
    pub inner: S,
    pub traffic: Option<Traffic>,
}
impl<S: AsyncRead + Unpin> AsyncRead for Stream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let result = Pin::new(&mut this.inner).poll_read(cx, buf);
        if let Some(traffic) = &this.traffic {
            match &result {
                Poll::Ready(Ok(())) => traffic.download(buf.filled().len() - before),
                Poll::Ready(Err(error)) => traffic.failed(error),
                _ => {}
            }
        }
        result
    }
}
impl<S: AsyncWrite + Unpin> AsyncWrite for Stream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_write(cx, bytes);
        if let Some(traffic) = &this.traffic {
            match &result {
                Poll::Ready(Ok(size)) => traffic.upload(*size),
                Poll::Ready(Err(error)) => traffic.failed(error),
                _ => {}
            }
        }
        result
    }
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_write_vectored(cx, bytes);
        if let Some(traffic) = &this.traffic {
            match &result {
                Poll::Ready(Ok(size)) => traffic.upload(*size),
                Poll::Ready(Err(error)) => traffic.failed(error),
                _ => {}
            }
        }
        result
    }
    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_flush(cx);
        if let (Some(traffic), Poll::Ready(Err(error))) = (&this.traffic, &result) {
            traffic.failed(error);
        }
        result
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_shutdown(cx);
        if let (Some(traffic), Poll::Ready(Err(error))) = (&this.traffic, &result) {
            traffic.failed(error);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tracing_subscriber::prelude::*;
    #[tokio::test(flavor = "current_thread")]
    async fn stream_counts_actual_transferred_bytes_and_handles_half_close() {
        let subscriber =
            tracing_subscriber::registry().with(tracing_subscriber::EnvFilter::new("debug"));
        let _guard = tracing::subscriber::set_default(subscriber);
        let (inner, mut peer) = tokio::io::duplex(64);
        let target = Target {
            host: "example.test".into(),
            port: 443,
        };
        let traffic = Traffic::new(
            Some("127.0.0.1:40000".parse().unwrap()),
            &target,
            Inbound::Socks5,
            Network::Tcp,
            &RouteAction::Direct,
        );
        assert!(traffic.is_some());
        let mut stream = Stream { inner, traffic };
        tokio::try_join!(
            async {
                let slices = [io::IoSlice::new(b"he"), io::IoSlice::new(b"llo")];
                let size = stream.write_vectored(&slices).await?;
                stream.write_all(&b"hello"[size..]).await?;
                stream.shutdown().await?;
                let mut response = Vec::new();
                stream.read_to_end(&mut response).await?;
                assert_eq!(response, b"world!");
                Ok::<_, io::Error>(())
            },
            async {
                let mut request = Vec::new();
                peer.read_to_end(&mut request).await?;
                assert_eq!(request, b"hello");
                peer.write_all(b"world!").await?;
                peer.shutdown().await
            }
        )
        .unwrap();
        let traffic = stream.traffic.as_ref().unwrap();
        assert_eq!(traffic.up.load(Ordering::Relaxed), 5);
        assert_eq!(traffic.down.load(Ordering::Relaxed), 6);
        assert!(!traffic.failed.load(Ordering::Relaxed));
    }
    #[tokio::test(flavor = "current_thread")]
    async fn failed_write_is_reported_without_counting_unsent_bytes() {
        let subscriber =
            tracing_subscriber::registry().with(tracing_subscriber::EnvFilter::new("debug"));
        let _guard = tracing::subscriber::set_default(subscriber);
        let (inner, peer) = tokio::io::duplex(64);
        drop(peer);
        let target = Target {
            host: "example.test".into(),
            port: 443,
        };
        let traffic = Traffic::new(
            None,
            &target,
            Inbound::Tun,
            Network::Tcp,
            &RouteAction::Direct,
        );
        let mut stream = Stream { inner, traffic };
        assert!(stream.write_all(b"not sent").await.is_err());
        let traffic = stream.traffic.as_ref().unwrap();
        assert!(traffic.failed.load(Ordering::Relaxed));
        assert_eq!(traffic.up.load(Ordering::Relaxed), 0);
    }
}
