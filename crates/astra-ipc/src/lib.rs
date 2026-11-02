//! Rule and management IPC carry metadata only. The separate `host` module
//! provides authenticated-by-caller data channels for resident ingress workers.
use crate::platform::LocalStream as UnixStream;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub mod control;
pub mod diagnostics;
#[cfg(windows)]
pub mod elevation;
pub mod groups;
pub mod host;
pub mod logging;
pub mod nettool;
pub mod platform;
pub mod progress;
pub mod runtime;
pub mod session;
pub mod storage;
#[cfg(unix)]
pub mod unix_handoff;

pub const API_VERSION: u16 = 4;
const MAX_MESSAGE_BYTES: u32 = 8192;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Hash, PartialEq, Eq)]
pub enum Network {
    Tcp,
    Udp,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Hash, PartialEq, Eq)]
pub enum Inbound {
    Socks5,
    HttpConnect,
    Tun,
    Tproxy,
    Redirect,
    Mixed,
}

#[derive(Debug, Clone, Serialize, Deserialize, Hash, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteRequest {
    /// Observed real destination retained alongside a DNS/SNI hostname.
    /// Synthetic Fake IP addresses must never populate this field.
    #[serde(default)]
    pub destination_ip: Option<std::net::IpAddr>,
    pub api_version: u16,
    pub id: u64,
    pub target: Target,
    pub network: Network,
    pub inbound: Inbound,
    pub process: Option<ProcessInfo>,
}

/// Observed socket owner, supplied by the local proxy, never by client headers.
#[derive(Debug, Clone, Serialize, Deserialize, Hash, PartialEq, Eq)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum RouteAction {
    Direct,
    Block,
    Proxy(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteResponse {
    pub revision: u64,
    pub api_version: u16,
    pub id: u64,
    pub action: RouteAction,
}

pub async fn send<T: Serialize>(stream: &mut UnixStream, message: &T) -> io::Result<()> {
    send_bounded(stream, message, MAX_MESSAGE_BYTES).await
}

/// Management messages may include configuration text, unlike route frames.
pub async fn send_management<T: Serialize>(
    stream: &mut (impl tokio::io::AsyncWrite + Unpin),
    message: &T,
) -> io::Result<()> {
    send_bounded(stream, message, 1024 * 1024).await
}

async fn send_bounded<T: Serialize>(
    stream: &mut (impl tokio::io::AsyncWrite + Unpin),
    message: &T,
    limit: u32,
) -> io::Result<()> {
    let data =
        bincode::serialize(message).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if data.len() > limit as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "oversized IPC frame",
        ));
    }
    stream.write_u32(data.len() as u32).await?;
    stream.write_all(&data).await
}

pub async fn receive<T: DeserializeOwned>(stream: &mut UnixStream) -> io::Result<T> {
    receive_bounded(stream, MAX_MESSAGE_BYTES).await
}

pub async fn receive_management<T: DeserializeOwned>(stream: &mut UnixStream) -> io::Result<T> {
    receive_bounded(stream, 1024 * 1024).await
}

async fn receive_bounded<T: DeserializeOwned>(
    stream: &mut UnixStream,
    limit: u32,
) -> io::Result<T> {
    let len = stream.read_u32().await?;
    if len == 0 || len > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid IPC frame length",
        ));
    }
    let mut frame = vec![0u8; len as usize];
    stream.read_exact(&mut frame).await?;
    bincode::deserialize(&frame).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub use platform::shutdown_signal;

/// Extensible metadata carried inside the existing v4 route envelope.
/// Release versions are informational; wire compatibility is independent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoreInfo {
    pub version: String,
    pub wire_version: u16,
    pub features: Vec<String>,
}
impl CoreInfo {
    pub fn local(version: &str) -> Self {
        Self { version: version.into(), wire_version: API_VERSION,
            features: vec!["route".into(), "destination-ip".into(), "revision-subscription".into()] }
    }
    pub fn common_features(&self, peer: &Self) -> Vec<String> {
        self.features.iter().filter(|f| peer.features.contains(f)).cloned().collect()
    }
}
pub const CORE_INFO_HOST: &str = "astra-core-info.invalid";

pub async fn exchange_core_info(stream: &mut UnixStream, version: &str) -> io::Result<Option<CoreInfo>> {
    let local = CoreInfo::local(version);
    let request = RouteRequest {
        destination_ip: None, api_version: API_VERSION, id: u64::MAX,
        target: Target { host: CORE_INFO_HOST.into(), port: 0 },
        network: Network::Tcp, inbound: Inbound::Socks5,
        process: Some(ProcessInfo { pid: 0, name: CORE_INFO_HOST.into(),
            path: serde_json::to_string(&local)? }),
    };
    send(stream, &request).await?;
    let response: RouteResponse = receive(stream).await?;
    if response.id != request.id || response.api_version != API_VERSION {
        return Err(io::Error::other("incompatible core wire protocol"));
    }
    // Older v4 cores treat the probe as an ordinary route request.
    if let RouteAction::Proxy(value) = response.action {
        if let Some(json) = value.strip_prefix("astra-core-info:") {
            let peer: CoreInfo = serde_json::from_str(json)?;
            if peer.wire_version != API_VERSION {
                return Err(io::Error::other("incompatible core wire protocol"));
            }
            tracing::info!(?peer, features = ?local.common_features(&peer), "core capabilities exchanged");
            return Ok(Some(peer));
        }
    }
    Ok(None)
}
