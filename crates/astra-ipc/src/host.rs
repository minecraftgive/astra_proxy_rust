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

pub const HOST_PROTOCOL_VERSION: u16 = 1;
pub const MAX_CONTROL_BYTES: usize = 1024 * 1024;
pub const MAX_STREAM_DATA: usize = 32 * 1024;
pub const MAX_DATAGRAM_BYTES: usize = 65535;

/// Additive JSON telemetry. Deliberately not part of the bincode runtime Snapshot.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeView {
    #[serde(flatten)]
    pub snapshot: crate::runtime::Snapshot,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry: Option<RuntimeTelemetry>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Coverage {
    #[default]
    Unknown,
    Complete,
    Partial,
    Stale,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeTelemetry {
    pub host_boot_id: String,
    pub active_generation: Option<u64>,
    pub ingress_paused: bool,
    pub sample_sequence: u64,
    /// Milliseconds on the host's monotonic clock, not wall-clock timestamps.
    pub sample_elapsed_ms: u64,
    pub sample_age_ms: u64,
    pub coverage: Coverage,
    pub generations: Vec<GenerationTelemetry>,
    pub connection_generations: std::collections::BTreeMap<u64, u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GenerationTelemetry {
    pub generation: u64,
    pub sample_age_ms: Option<u64>,
    pub healthy: bool,
}

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
            output: Vec::new(),
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

#[cfg(test)]
mod tests {
    #[test]
    fn runtime_telemetry_json_is_additive_to_legacy_snapshot() {
        let snapshot = crate::runtime::Snapshot {
            pid: 42,
            upload: 123,
            ..Default::default()
        };
        let legacy = serde_json::to_string(&snapshot).unwrap();
        let view: super::RuntimeView = serde_json::from_str(&legacy).unwrap();
        assert_eq!(view.snapshot.pid, 42);
        assert!(view.telemetry.is_none());
        let enhanced = super::RuntimeView {
            snapshot,
            telemetry: Some(super::RuntimeTelemetry {
                host_boot_id: "boot".into(),
                sample_sequence: 7,
                ..Default::default()
            }),
        };
        let json = serde_json::to_string(&enhanced).unwrap();
        let old_reader: crate::runtime::Snapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(old_reader.upload, 123);
        let new_reader: super::RuntimeView = serde_json::from_str(&json).unwrap();
        assert_eq!(new_reader.telemetry.unwrap().sample_sequence, 7);
    }
    #[test]
    fn worker_negotiation_accepts_additions_but_rejects_missing_semantics() {
        let mut hello = super::Hello::worker("future-version");
        hello.capabilities.push("optional_future_feature".into());
        hello.validate_worker().unwrap();
        hello.capabilities.remove(0);
        assert_eq!(
            hello.validate_worker().unwrap_err().kind(),
            std::io::ErrorKind::Unsupported
        );
        let mut manager = super::Hello::worker("1");
        manager.role = super::PeerRole::Manager;
        assert!(manager.validate_worker().is_err());
    }
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    #[tokio::test]
    async fn control_json_roundtrip_and_version_rejection() {
        let (mut sender, mut receiver) = tokio::io::duplex(4096);
        let message = ControlEnvelope::new(17, ControlRequest::Drain(Drain { generation: 3 }));
        send_control(&mut sender, &message).await.unwrap();
        assert_eq!(
            receive_control::<_, ControlRequest>(&mut receiver)
                .await
                .unwrap(),
            message
        );
        let mut invalid = message;
        invalid.protocol_version += 1;
        assert_eq!(
            send_control(&mut sender, &invalid)
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        let bytes = serde_json::to_vec(&invalid).unwrap();
        sender.write_u32(bytes.len() as u32).await.unwrap();
        sender.write_all(&bytes).await.unwrap();
        assert_eq!(
            receive_control::<_, ControlRequest>(&mut receiver)
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[tokio::test]
    async fn oversized_control_rejected_before_payload_read() {
        let (mut sender, mut receiver) = tokio::io::duplex(8);
        sender
            .write_u32(MAX_CONTROL_BYTES as u32 + 1)
            .await
            .unwrap();
        assert_eq!(
            receive_control::<_, ControlRequest>(&mut receiver)
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    async fn half_close<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(left: S, right: S) {
        let mut client = HostStream::new(left);
        let mut server = HostStream::new(right);
        let task = tokio::spawn(async move {
            let mut request = Vec::new();
            server.read_to_end(&mut request).await.unwrap();
            assert_eq!(request, vec![7; MAX_STREAM_DATA * 3 + 123]);
            server
                .write_all(b"response after upload FIN")
                .await
                .unwrap();
            server.shutdown().await.unwrap();
        });
        client
            .write_all(&vec![7; MAX_STREAM_DATA * 3 + 123])
            .await
            .unwrap();
        client.shutdown().await.unwrap();
        client.shutdown().await.unwrap();
        assert_eq!(
            client.write(b"late").await.unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        assert_eq!(response, b"response after upload FIN");
        task.await.unwrap();
    }

    #[tokio::test]
    async fn half_close_over_fragmented_transport() {
        let (left, right) = tokio::io::duplex(3);
        timeout(Duration::from_secs(10), half_close(left, right))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn half_close_over_platform_local_stream() {
        let (left, right) = crate::platform::stream_pair().await.unwrap();
        timeout(Duration::from_secs(10), half_close(left, right))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn reset_is_distinct_from_fin() {
        let (left, right) = tokio::io::duplex(128);
        let mut sender = HostStream::new(left);
        let mut receiver = HostStream::new(right);
        sender.reset().await.unwrap();
        let error = receiver.read(&mut [0; 1]).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionReset);
        assert_eq!(
            receiver.write(b"late").await.unwrap_err().kind(),
            io::ErrorKind::ConnectionReset
        );
    }

    #[tokio::test]
    async fn malformed_and_truncated_streams_fail_closed() {
        for frame in [
            vec![DATA, 0, 1, 0, 0],
            vec![DATA, 0, 0, 0, 0],
            vec![FIN, 0, 0, 0, 1],
            vec![99, 0, 0, 0, 0],
        ] {
            let mut stream = HostStream::new(std::io::Cursor::new(frame));
            assert_eq!(
                stream.read(&mut [0; 1]).await.unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
        for frame in [vec![], vec![DATA, 0], vec![DATA, 0, 0, 0, 2, 42]] {
            let mut stream = HostStream::new(std::io::Cursor::new(frame));
            assert_eq!(
                stream
                    .read_to_end(&mut Vec::new())
                    .await
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::UnexpectedEof
            );
        }
    }

    #[tokio::test]
    async fn output_backpressure_is_bounded_to_one_frame() {
        let (left, _right) = tokio::io::duplex(1);
        let mut stream = HostStream::new(left);
        assert!(timeout(
            Duration::from_millis(20),
            stream.write(&vec![0; MAX_STREAM_DATA * 2])
        )
        .await
        .is_err());
        assert_eq!(stream.output.len(), MAX_STREAM_DATA + HEADER_BYTES);
        assert_eq!(
            stream.write(b"different").await.unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(stream.output.len(), MAX_STREAM_DATA + HEADER_BYTES);
    }

    #[tokio::test]
    async fn write_then_read_handshake_does_not_need_flush() {
        timeout(Duration::from_secs(5), async {
            let (left, right) = tokio::io::duplex(3);
            let mut client = HostStream::new(left);
            let mut server = HostStream::new(right);
            let task = tokio::spawn(async move {
                let mut request = [0; 4];
                server.read_exact(&mut request).await.unwrap();
                assert_eq!(&request, b"ping");
                server.write_all(b"pong").await.unwrap();
            });
            client.write_all(b"ping").await.unwrap();
            let mut reply = [0; 4];
            client.read_exact(&mut reply).await.unwrap();
            assert_eq!(&reply, b"pong");
            task.await.unwrap();
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn datagrams_preserve_empty_and_maximum_payloads() {
        let (mut sender, mut receiver) = tokio::io::duplex(128);
        let task = tokio::spawn(async move {
            send_datagram(&mut sender, &[]).await.unwrap();
            send_datagram(&mut sender, &vec![123; MAX_DATAGRAM_BYTES])
                .await
                .unwrap();
            assert_eq!(
                send_datagram(&mut sender, &vec![0; MAX_DATAGRAM_BYTES + 1])
                    .await
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput
            );
        });
        assert!(receive_datagram(&mut receiver).await.unwrap().is_empty());
        assert_eq!(
            receive_datagram(&mut receiver).await.unwrap(),
            vec![123; MAX_DATAGRAM_BYTES]
        );
        task.await.unwrap();
    }

    #[tokio::test]
    async fn datagram_length_is_checked_before_allocation() {
        let (mut sender, mut receiver) = tokio::io::duplex(8);
        sender
            .write_u32(MAX_DATAGRAM_BYTES as u32 + 1)
            .await
            .unwrap();
        assert_eq!(
            receive_datagram(&mut receiver).await.unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
