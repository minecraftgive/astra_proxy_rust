//! Versioned host/worker protocol, separate from the v4 rule/control protocol.
//!
//! Control, individual TCP streams and UDP datagrams use separate connections.
//! Authenticate a connection before using this module; framing is not authentication.
//! A TCP FIN closes only its direction, including over Windows named pipes.

use crate::{Inbound, ProcessInfo};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
    io,
    net::SocketAddr,
    path::PathBuf,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};

pub const HOST_PROTOCOL_VERSION: u16 = astra_model::HOST_PROTOCOL_VERSION;
pub const MAX_CONTROL_BYTES: usize = 1024 * 1024;
pub const MAX_STREAM_DATA: usize = 32 * 1024;
pub const MAX_DATAGRAM_BYTES: usize = 65535;

#[cfg(test)]
use astra_model::runtime::{RuntimeTelemetry, RuntimeView};
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct FlowId {
    /// Unique host boot identifier. Never reuse it after a host restart.
    pub boot: String,
    pub generation: u64,
    pub sequence: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IngressMeta {
    pub flow_id: FlowId,
    pub source: SocketAddr,
    pub local: SocketAddr,
    pub original_destination: Option<SocketAddr>,
    pub inbound: Inbound,
    pub process: Option<ProcessInfo>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PeerRole {
    Host,
    Worker,
    Manager,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Hello {
    pub role: PeerRole,
    pub version: String,
    pub capabilities: Vec<String>,
}

impl Hello {
    pub const REQUIRED_WORKER_CAPABILITIES: &[&str] = &[
        "tcp_framing_v1",
        "udp_datagrams_v1",
        "trusted_ingress_v1",
        "shared_dns_mapping_v1",
        "generation_drain_v1",
    ];

    pub fn worker(version: impl Into<String>) -> Self {
        Self {
            role: PeerRole::Worker,
            version: version.into(),
            capabilities: Self::REQUIRED_WORKER_CAPABILITIES
                .iter()
                .map(|value| (*value).into())
                .collect(),
        }
    }

    /// Unknown capabilities are additive; missing semantics refuse activation.
    /// Binary versions need not match the resident host's version.
    pub fn validate_worker(&self) -> io::Result<()> {
        if self.role != PeerRole::Worker || self.version.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid worker identity",
            ));
        }
        for capability in Self::REQUIRED_WORKER_CAPABILITIES {
            if !self.capabilities.iter().any(|value| value == capability) {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("worker lacks required capability {capability}"),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Prepare {
    pub generation: u64,
    pub release_dir: PathBuf,
    pub proxy_config: PathBuf,
    pub rule_config: Option<PathBuf>,
    pub config_digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Ready {
    pub generation: u64,
    pub version: String,
    pub config_digest: String,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Activate {
    pub generation: u64,
    /// Compare-and-swap guard against a concurrent update.
    pub expected_active: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Drain {
    pub generation: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Drained {
    pub generation: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Rollback {
    pub generation: u64,
    pub expected_active: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GenerationState {
    Preparing,
    Ready,
    Active,
    Draining,
    Drained,
    Failed,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionCounts {
    pub tcp: u64,
    pub udp_associations: u64,
    pub udp_flows: u64,
    pub dns_requests: u64,
    pub pending_handoffs: u64,
}

impl SessionCounts {
    pub fn is_idle(&self) -> bool {
        self.tcp == 0
            && self.udp_associations == 0
            && self.udp_flows == 0
            && self.dns_requests == 0
            && self.pending_handoffs == 0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GenerationStatus {
    pub generation: u64,
    pub version: String,
    pub state: GenerationState,
    pub sessions: SessionCounts,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateStatus {
    pub active_generation: Option<u64>,
    pub host_version: String,
    pub pending_host_version: Option<String>,
    pub generations: Vec<GenerationStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", content = "body", rename_all = "snake_case")]
pub enum ControlRequest {
    Hello(Hello),
    Prepare(Prepare),
    Activate(Activate),
    Drain(Drain),
    UpdateStatus,
    Rollback(Rollback),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", content = "body", rename_all = "snake_case")]
pub enum ControlResponse {
    Hello(Hello),
    Ready(Ready),
    Activated { generation: u64 },
    Draining { generation: u64 },
    Drained(Drained),
    UpdateStatus(UpdateStatus),
    RolledBack { generation: u64 },
    Error { code: String, message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ControlEnvelope<T> {
    pub protocol_version: u16,
    pub request_id: u64,
    pub message: T,
}

impl<T> ControlEnvelope<T> {
    pub fn new(request_id: u64, message: T) -> Self {
        Self {
            protocol_version: HOST_PROTOCOL_VERSION,
            request_id,
            message,
        }
    }
}

/// Big-endian length followed by UTF-8 JSON. A malformed/truncated frame makes
/// the connection unusable; callers must close it instead of retrying a read.
pub async fn send_control<W: AsyncWrite + Unpin, T: Serialize>(
    stream: &mut W,
    message: &ControlEnvelope<T>,
) -> io::Result<()> {
    validate_version(message.protocol_version)?;
    let bytes = serde_json::to_vec(message).map_err(invalid_data)?;
    if bytes.len() > MAX_CONTROL_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "host control frame too large",
        ));
    }
    stream.write_u32(bytes.len() as u32).await?;
    stream.write_all(&bytes).await?;
    stream.flush().await
}

pub async fn receive_control<R: AsyncRead + Unpin, T: DeserializeOwned>(
    stream: &mut R,
) -> io::Result<ControlEnvelope<T>> {
    let length = stream.read_u32().await? as usize;
    if length == 0 || length > MAX_CONTROL_BYTES {
        return Err(invalid_data("invalid host control frame length"));
    }
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await?;
    let message: ControlEnvelope<T> = serde_json::from_slice(&bytes).map_err(invalid_data)?;
    validate_version(message.protocol_version)?;
    Ok(message)
}

fn validate_version(version: u16) -> io::Result<()> {
    if version != HOST_PROTOCOL_VERSION {
        return Err(invalid_data("incompatible host protocol version"));
    }
    Ok(())
}

fn invalid_data(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

const DATA: u8 = 1;
const FIN: u8 = 2;
const RESET: u8 = 3;
const HEADER_BYTES: usize = 5;

/// One logical TCP stream on one authenticated data connection.
///
/// At most one 32 KiB output frame is buffered. Incoming payload is read directly
/// into the caller's ReadBuf. There are no detached tasks or unbounded queues.
/// `shutdown()` sends FIN without closing the underlying bidirectional pipe;
/// reads remain available until the peer sends FIN. Dropping without FIN is an
/// abrupt close, reported as UnexpectedEof by the peer.
/// Writes complete after the whole frame is sent, so write-then-read handshakes
/// do not require an extra flush. If a write is cancelled, close/reset the stream
/// rather than attempting a different write on the partially sent frame.
pub struct HostStream<S> {
    inner: S,
    header: [u8; HEADER_BYTES],
    header_read: usize,
    remaining: usize,
    read_fin: bool,
    write_fin: bool,
    failed: Option<io::ErrorKind>,
    output: Vec<u8>,
    written: usize,
}

impl<S> HostStream<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            header: [0; HEADER_BYTES],
            header_read: 0,
            remaining: 0,
            read_fin: false,
            write_fin: false,
            failed: None,
            output: Vec::with_capacity(HEADER_BYTES + MAX_STREAM_DATA),
            written: 0,
        }
    }

    fn error(&self) -> Option<io::Error> {
        self.failed
            .map(|kind| io::Error::new(kind, "host stream failed"))
    }

    fn frame(&mut self, tag: u8, bytes: &[u8]) {
        debug_assert!(self.output.is_empty());
        self.output.push(tag);
        self.output
            .extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        self.output.extend_from_slice(bytes);
    }
}

impl<S: AsyncWrite + Unpin> HostStream<S> {
    fn poll_output(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while self.written < self.output.len() {
            let count = match Pin::new(&mut self.inner).poll_write(cx, &self.output[self.written..])
            {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(error)) => {
                    self.failed = Some(error.kind());
                    return Poll::Ready(Err(error));
                }
                Poll::Ready(Ok(0)) => {
                    self.failed = Some(io::ErrorKind::WriteZero);
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "host stream write stalled",
                    )));
                }
                Poll::Ready(Ok(count)) => count,
            };
            self.written += count;
        }
        self.output.clear();
        self.written = 0;
        Poll::Ready(Ok(()))
    }

    /// Abort both logical directions. Already accepted data is flushed before
    /// RESET so a partial frame can never be mistaken for its header.
    pub async fn reset(&mut self) -> io::Result<()> {
        self.flush().await?;
        self.frame(RESET, &[]);
        std::future::poll_fn(|cx| self.poll_output(cx)).await?;
        self.inner.flush().await?;
        self.failed = Some(io::ErrorKind::ConnectionReset);
        Ok(())
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for HostStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        if let Some(error) = this.error() {
            return Poll::Ready(Err(error));
        }
        if this.read_fin {
            return Poll::Ready(Ok(()));
        }
        loop {
            if this.remaining != 0 {
                let limit = this.remaining.min(buf.remaining());
                let mut slice = buf.take(limit);
                match Pin::new(&mut this.inner).poll_read(cx, &mut slice) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(error)) => {
                        this.failed = Some(error.kind());
                        return Poll::Ready(Err(error));
                    }
                    Poll::Ready(Ok(())) => {}
                }
                let count = slice.filled().len();
                if count == 0 {
                    this.failed = Some(io::ErrorKind::UnexpectedEof);
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "truncated host DATA frame",
                    )));
                }
                // SAFETY: the sub-buffer has initialized these bytes in the parent.
                unsafe {
                    buf.assume_init(count);
                }
                buf.advance(count);
                this.remaining -= count;
                return Poll::Ready(Ok(()));
            }
            while this.header_read < HEADER_BYTES {
                let mut header = ReadBuf::new(&mut this.header[this.header_read..]);
                match Pin::new(&mut this.inner).poll_read(cx, &mut header) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(error)) => {
                        this.failed = Some(error.kind());
                        return Poll::Ready(Err(error));
                    }
                    Poll::Ready(Ok(())) => {}
                }
                let count = header.filled().len();
                if count == 0 {
                    this.failed = Some(io::ErrorKind::UnexpectedEof);
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "host stream closed without FIN",
                    )));
                }
                this.header_read += count;
            }
            this.header_read = 0;
            let length = u32::from_be_bytes(this.header[1..].try_into().unwrap()) as usize;
            match (this.header[0], length) {
                (DATA, 1..=MAX_STREAM_DATA) => this.remaining = length,
                (FIN, 0) => {
                    this.read_fin = true;
                    return Poll::Ready(Ok(()));
                }
                (RESET, 0) => {
                    this.failed = Some(io::ErrorKind::ConnectionReset);
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::ConnectionReset,
                        "peer reset host stream",
                    )));
                }
                _ => {
                    this.failed = Some(io::ErrorKind::InvalidData);
                    return Poll::Ready(Err(invalid_data("invalid host stream frame")));
                }
            }
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for HostStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if let Some(error) = this.error() {
            return Poll::Ready(Err(error));
        }
        if this.write_fin {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "host stream write direction finished",
            )));
        }
        if bytes.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if this.output.is_empty() {
            this.frame(DATA, &bytes[..bytes.len().min(MAX_STREAM_DATA)]);
        } else if this.output[0] != DATA || !bytes.starts_with(&this.output[HEADER_BYTES..]) {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a cancelled host write must be closed/reset before sending different data",
            )));
        }
        let count = this.output.len() - HEADER_BYTES;
        match this.poll_output(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Ready(Ok(())) => Poll::Ready(Ok(count)),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Some(error) = this.error() {
            return Poll::Ready(Err(error));
        }
        match this.poll_output(cx) {
            Poll::Ready(Ok(())) => Pin::new(&mut this.inner).poll_flush(cx),
            other => other,
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Some(error) = this.error() {
            return Poll::Ready(Err(error));
        }
        match this.poll_output(cx) {
            Poll::Ready(Ok(())) => {}
            other => return other,
        }
        if !this.write_fin {
            this.write_fin = true;
            this.frame(FIN, &[]);
        }
        match this.poll_output(cx) {
            Poll::Ready(Ok(())) => Pin::new(&mut this.inner).poll_flush(cx),
            other => other,
        }
    }
}

/// Dedicated datagram channel. The caller establishes flow metadata separately.
/// Length zero is a datagram, never EOF. No payload shares the control channel.
pub async fn send_datagram<W: AsyncWrite + Unpin>(
    stream: &mut W,
    payload: &[u8],
) -> io::Result<()> {
    if payload.len() > MAX_DATAGRAM_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "host datagram too large",
        ));
    }
    stream.write_u32(payload.len() as u32).await?;
    stream.write_all(payload).await?;
    stream.flush().await
}

pub async fn receive_datagram<R: AsyncRead + Unpin>(stream: &mut R) -> io::Result<Vec<u8>> {
    let length = stream.read_u32().await? as usize;
    if length > MAX_DATAGRAM_BYTES {
        return Err(invalid_data("host datagram too large"));
    }
    let mut payload = vec![0; length];
    stream.read_exact(&mut payload).await?;
    Ok(payload)
}

pub async fn send_message<T: Serialize, S: AsyncWrite + Unpin>(
    s: &mut S,
    body: T,
) -> anyhow::Result<()> {
    send_control(s, &ControlEnvelope::new(0, body)).await?;
    Ok(())
}
pub async fn receive_message<T: serde::de::DeserializeOwned, S: AsyncRead + Unpin>(
    s: &mut S,
) -> anyhow::Result<T> {
    Ok(receive_control(s).await?.message)
}

pub async fn request_json_control<T: Serialize>(
    path: &std::path::Path,
    command: T,
) -> anyhow::Result<serde_json::Value> {
    let mut stream = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        astra_platform::platform::connect_trusted(path),
    )
    .await??;
    send_message(&mut stream, command).await?;
    let result: std::result::Result<serde_json::Value, String> = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        receive_message(&mut stream),
    )
    .await??;
    result.map_err(anyhow::Error::msg)
}

#[cfg(test)]
#[path = "test/host_test.rs"]
mod tests;
