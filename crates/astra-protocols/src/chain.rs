//! A fixed prefix dialer. It never reselects groups between TCP and UDP legs.
use crate::{
    native::{self, NativeOutbound},
    outbound::Stream,
};
use anyhow::Context as _;
use astra_model::Target;
use meow_proxy::dialer::{TcpDialer, UdpTarget};
use std::{
    io,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub struct PrefixDialer(pub Arc<NativeOutbound>, pub String);

// Outbound streams need Send, whereas meow transports also require Sync.
// Polling takes exclusive access; the mutex is only a type-safe Sync bridge.
struct SharedStream(Mutex<Stream>);
impl AsyncRead for SharedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(
            self.0
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
        .poll_read(cx, buf)
    }
}
impl AsyncWrite for SharedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(
            self.0
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
        .poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(
            self.0
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
        .poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(
            self.0
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
        .poll_shutdown(cx)
    }
}

#[async_trait::async_trait]
impl TcpDialer for PrefixDialer {
    async fn dial(
        &self,
        host: &str,
        port: u16,
        _internal: bool,
    ) -> io::Result<Box<dyn meow_transport::Stream>> {
        let stream = native::connect(
            &self.0,
            &Target {
                host: host.into(),
                port,
            },
        )
        .await
        .with_context(|| format!("front hop {}", self.1))
        .map_err(io::Error::other)?;
        Ok(Box::new(SharedStream(Mutex::new(stream))))
    }
    fn is_proxy(&self) -> bool {
        true
    }
    fn supports_udp(&self) -> bool {
        native::supports_udp(&self.0)
    }
    async fn dial_udp_conn(
        &self,
        remote: UdpTarget,
        _internal: bool,
    ) -> io::Result<Arc<dyn meow_common::ProxyPacketConn>> {
        if !self.supports_udp() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "prefix chain does not support UDP",
            ));
        }
        let target = match remote {
            UdpTarget::Addr(addr) => Target {
                host: addr.ip().to_string(),
                port: addr.port(),
            },
            UdpTarget::Name { host, port } => Target {
                host: host.to_string(),
                port,
            },
        };
        // Do not convert errors to Unsupported: callers must not fall back to
        // local DNS after an attempted remote domain association.
        Ok(Arc::from(
            native::connect_udp(&self.0, &target)
                .await
                .map_err(io::Error::other)?,
        ))
    }
}
