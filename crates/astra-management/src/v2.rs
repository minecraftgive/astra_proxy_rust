//! Typed management v2. All counters and revisions crossing JSON are decimal strings.
use crate::update_json;
pub use astra_platform::program_info::ProgramInfo;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
#[cfg(feature = "bindings")]
use ts_rs::TS;

mod installation;
pub use installation::*;

pub const VERSION: u16 = 2;
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub version: u16,
    pub id: String,
    pub agent_id: Option<String>,
    pub body: RequestBody,
}
impl Request {
    pub fn new(id: impl Into<String>, body: RequestBody) -> Self {
        Self {
            version: VERSION,
            id: id.into(),
            agent_id: None,
            body,
        }
    }
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum RequestBody {
    Local(LocalCommand),
    Query(Query),
    Execute {
        operation_id: String,
        command: Command,
    },
    Drafts(DraftCommand),
    Artifacts(ArtifactCommand),
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Query {
    MaintenanceStatus,
    Hello,
    ProgramInfo,
    InstallationStatus,
    Agents,
    Instances,
    Monitor {
        instance: String,
    },
    Runtime {
        instance: String,
    },
    Document {
        instance: String,
    },
    Schema {
        kind: InstanceKind,
        #[serde(default)]
        path: Option<astra_model::document::DocumentPath>,
    },
    Capabilities {
        instance: String,
    },
    NodeTags {
        instance: String,
    },
    SubscriptionNodes {
        instance: String,
    },
    DiagnosticTargets {
        instance: String,
    },
    /// One header probe of `outbound`: TCP handshake time, plus the exit IP when `ip`.
    ExitProbe {
        instance: String,
        outbound: String,
        ip: bool,
        /// Keep the probe core loaded this long for the next round; none exits after answering.
        keep_alive_secs: Option<u32>,
    },
    LogLevel {
        instance: String,
    },
    StoragePaths,
    LogChunk {
        instance: String,
        cursor: Option<LogCursor>,
    },
    Tasks {
        limit: Option<u16>,
    },
    Logs {
        instance: String,
    },
    UpdateStatus,
    UpdateOperationStatus {
        operation_id: String,
    },
    Task {
        operation_id: String,
    },
    Events {
        after: Option<String>,
        limit: Option<u16>,
    },
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InstanceKind {
    Proxy,
    Rule,
    Server,
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Command {
    InstallCurrentPackage {
        package_id: String,
        expected_release_id: String,
        scope: InstallationScope,
    },
    CancelTask {
        operation_id: String,
    },
    Create {
        name: String,
        kind: InstanceKind,
    },
    Add {
        name: String,
        kind: InstanceKind,
        config: PathBuf,
    },
    Remove {
        instance: String,
    },
    Start {
        instance: String,
    },
    /// Start without interactive elevation; fails if any dependency needs it.
    StartSilent {
        instance: String,
    },
    /// Discard old session metadata after explicit user confirmation.
    ForceStart {
        instance: String,
    },
    Stop {
        instance: String,
    },
    Restart {
        instance: String,
    },
    Reload {
        instance: String,
    },
    Validate {
        instance: String,
    },
    PrepareConfig {
        instance: String,
    },
    Runtime {
        instance: String,
        command: RuntimeCommand,
        expected_pid: Option<u32>,
    },
    UpdateSubscriptions {
        instance: String,
    },
    Diagnostics {
        instance: String,
        targets: Option<Vec<String>>,
    },
    ExplainRoute {
        instance: String,
        request: RouteQuery,
    },
    SetLogLevel {
        instance: String,
        #[cfg_attr(
            feature = "bindings",
            ts(type = r#""error" | "warn" | "info" | "debug" | "trace" | "route""#)
        )]
        level: crate::LogLevel,
    },
    BindStart {
        instance: String,
        targets: Vec<String>,
    },
    CloneConfig {
        instance: String,
        destination: PathBuf,
    },
    SetConfig {
        instance: String,
        path: PathBuf,
    },
    NetTool {
        instance: String,
        request: NetToolRequest,
    },
    Update {
        #[cfg_attr(feature = "bindings", ts(type = "unknown"))]
        request: crate::updates::UpdateRequest,
    },
    RecoverUpdate {
        operation_id: String,
    },
    UpdateArtifact {
        artifact_id: String,
    },
    Shutdown,
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RuntimeCommand {
    Snapshot,
    SetMode(
        #[cfg_attr(feature = "bindings", ts(type = r#""rule" | "global" | "direct""#))]
        astra_model::runtime::Mode,
    ),
    Select {
        group: String,
        member: String,
    },
    TestGroup(String),
    Disconnect(Option<String>),
    SystemProxy(bool),
    Tun(bool),
    Reset,
    AllowLan(bool),
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetToolRequest {
    #[cfg_attr(feature = "bindings", ts(type = "unknown"))]
    pub egress: astra_model::nettool::Egress,
    #[cfg_attr(feature = "bindings", ts(type = "unknown"))]
    pub probe: astra_model::nettool::Probe,
    pub count: u32,
    pub interval_ms: String,
    pub timeout_ms: String,
}
impl NetToolRequest {
    pub fn into_core(self) -> anyhow::Result<astra_model::nettool::Request> {
        Ok(astra_model::nettool::Request {
            config: String::new(),
            egress: self.egress,
            probe: self.probe,
            count: self.count,
            interval_ms: self.interval_ms.parse()?,
            timeout_ms: self.timeout_ms.parse()?,
        })
    }
}
impl From<astra_model::nettool::Request> for NetToolRequest {
    fn from(value: astra_model::nettool::Request) -> Self {
        Self {
            egress: value.egress,
            probe: value.probe,
            count: value.count,
            interval_ms: value.interval_ms.to_string(),
            timeout_ms: value.timeout_ms.to_string(),
        }
    }
}
impl RuntimeCommand {
    pub fn into_core(self) -> anyhow::Result<astra_model::runtime::Command> {
        use astra_model::runtime::Command as C;
        Ok(match self {
            Self::Snapshot => C::Snapshot,
            Self::SetMode(v) => C::SetMode(v),
            Self::Select { group, member } => C::Select { group, member },
            Self::TestGroup(v) => C::TestGroup(v),
            Self::Disconnect(v) => C::Disconnect(v.map(|v| v.parse()).transpose()?),
            Self::SystemProxy(v) => C::SystemProxy(v),
            Self::Tun(v) => C::Tun(v),
            Self::Reset => C::Reset,
            Self::AllowLan(v) => C::AllowLan(v),
        })
    }
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteQuery {
    pub host: String,
    pub port: u16,
    #[cfg_attr(feature = "bindings", ts(type = r#""Tcp" | "Udp""#))]
    pub network: astra_model::Network,
    #[cfg_attr(
        feature = "bindings",
        ts(type = r#""Socks5" | "HttpConnect" | "Tun" | "Tproxy" | "Redirect" | "Mixed""#)
    )]
    pub inbound: astra_model::Inbound,
    pub destination_ip: Option<std::net::IpAddr>,
    #[cfg_attr(
        feature = "bindings",
        ts(type = "{ pid: number; name: string; path: string } | null")
    )]
    pub process: Option<astra_model::ProcessInfo>,
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum DraftCommand {
    Apply {
        draft_id: String,
        expected_revision: String,
        command: astra_model::document::EditCommand,
    },
    Schema {
        draft_id: String,
        path: astra_model::document::DocumentPath,
    },
    Open {
        instance: String,
        draft_id: String,
    },
    Read {
        draft_id: String,
    },
    Edit {
        draft_id: String,
        expected_revision: String,
        text: String,
    },
    Save {
        draft_id: String,
        expected_revision: String,
        expected_source_revision: String,
        operation_id: String,
    },
    Discard {
        draft_id: String,
        expected_revision: String,
    },
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub version: u16,
    pub id: String,
    pub result: Result<ResponseBody, ApiError>,
}
impl Response {
    pub fn success(id: String, value: ResponseBody) -> Self {
        Self {
            version: VERSION,
            id,
            result: Ok(value),
        }
    }
    pub fn error(id: String, code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            version: VERSION,
            id,
            result: Err(ApiError {
                code,
                message: message.into(),
                details: None,
            }),
        }
    }
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    VersionMismatch,
    InvalidRequest,
    NotFound,
    Conflict,
    Offline,
    OutcomeUnknown,
    Unsupported,
    RequiresLocalAction,
    Unauthorized,
    Internal,
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "bindings", ts(optional))]
    pub details: Option<SaveOutcome>,
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ResponseBody {
    MaintenanceStatus(
        #[cfg_attr(feature = "bindings", ts(type = "unknown"))]
        Option<Box<crate::maintenance::Record>>,
    ),
    InstallationStatus(InstallationStatus),
    InstallationResult(InstallationResult),
    InstallationLaunch(InstallationLaunch),
    Local(serde_json::Value),
    Cancellation(Cancellation),
    Hello(Hello),
    ProgramInfo(ProgramInfo),
    Agents(Vec<Agent>),
    Instances(Vec<Instance>),
    Monitor(MonitorSnapshot),
    Runtime(RuntimeSnapshot),
    Document(Document),
    Schema(astra_model::document::Schema),
    Capabilities(Vec<String>),
    NodeTags(Vec<String>),
    SubscriptionNodes(
        #[cfg_attr(feature = "bindings", ts(type = "Record<string, unknown>"))]
        std::collections::BTreeMap<String, crate::SubscriptionNodes>,
    ),
    DiagnosticTargets(serde_json::Value),
    Diagnostics(serde_json::Value),
    ExitProbe(serde_json::Value),
    RouteExplanation(serde_json::Value),
    NetTool(Vec<serde_json::Value>),
    LogLevel(
        #[cfg_attr(
            feature = "bindings",
            ts(type = r#""error" | "warn" | "info" | "debug" | "trace" | "route""#)
        )]
        crate::LogLevel,
    ),
    StoragePaths(
        #[cfg_attr(feature = "bindings", ts(type = "{ config: string; state: string }"))]
        crate::StoragePaths,
    ),
    Draft(Draft),
    Artifact(Artifact),
    ArtifactChunk(ArtifactChunk),
    Task(Task),
    Tasks(Vec<Task>),
    Events(EventBatch),
    Logs {
        text: String,
    },
    LogChunk(LogChunk),
    UpdateStatus(
        #[cfg_attr(feature = "bindings", ts(type = "unknown"))]
        #[serde(with = "update_json")]
        Box<crate::updates::UpdateStatus>,
    ),
    Done {
        message: String,
    },
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ArtifactCommand {
    Begin {
        id: String,
        name: String,
        size: String,
        sha256: String,
    },
    Write {
        id: String,
        offset: String,
        base64: String,
        sha256: String,
    },
    Finish {
        id: String,
    },
    Read {
        id: String,
        offset: String,
        limit: Option<u32>,
    },
    Remove {
        id: String,
    },
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    pub id: String,
    pub name: String,
    pub size: String,
    pub received: String,
    pub sha256: String,
    pub complete: bool,
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactChunk {
    pub id: String,
    pub offset: String,
    pub next: String,
    pub base64: String,
    pub eof: bool,
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub id: String,
    pub name: String,
    pub boot_id: String,
    pub online: bool,
    pub last_seen_ms: String,
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub version: u16,
    pub role: String,
    pub epoch: String,
    pub build_id: String,
    pub capabilities: Vec<String>,
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum LocalCommand {
    PrepareMaintenance {
        target_directory: PathBuf,
        expected_manifest_sha256: String,
        expected_epoch: String,
    },
    CommitMaintenance {
        operation_id: String,
    },
    RestoreMaintenance {
        operation_id: String,
        #[serde(default)]
        dependencies_only: bool,
    },
    FinishMaintenance {
        operation_id: String,
    },
    CancelMaintenance {
        operation_id: String,
    },
    AbortMaintenance {
        operation_id: String,
    },
    MaintenanceTun {
        operation_id: String,
        instance: String,
        ticket: Option<String>,
    },
    PrepareInstallation {
        package_id: String,
        expected_release_id: String,
        scope: InstallationScope,
    },
    PrepareElevation {
        instance: String,
        expected_pid: Option<u32>,
    },
    WaitElevation {
        instance: String,
        ticket: String,
    },
    CancelElevation {
        instance: String,
        ticket: String,
    },
    EnableTun {
        instance: String,
        expected_pid: Option<u32>,
    },
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", content = "message", rename_all = "snake_case")]
pub enum Cancellation {
    AlreadyFinished(String),
    UnsafeToCancel(String),
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Instance {
    pub id: String,
    pub name: String,
    pub kind: InstanceKind,
    pub config: PathBuf,
    pub pid: Option<u32>,
    pub state: String,
    pub uptime_secs: String,
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub changed: bool,
    #[serde(default)]
    #[cfg_attr(feature = "bindings", ts(type = "unknown"))]
    pub details: Option<crate::InstanceDetails>,
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogCursor {
    pub device: String,
    pub inode: String,
    pub offset: String,
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum LogChunk {
    Data {
        bytes: Vec<u8>,
        cursor: LogCursor,
        reset: bool,
        skip_prefix: bool,
    },
    Waiting,
    Error(String),
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "snapshot", rename_all = "snake_case")]
pub enum RuntimeSnapshot {
    Client(serde_json::Value),
    Server(ServerSnapshot),
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerSnapshot {
    pub pid: u32,
    pub uptime_secs: String,
    pub active: String,
    pub accepted: String,
    pub rejected: String,
    pub upload: String,
    pub download: String,
    pub listeners: Vec<String>,
}
/// Process metrics cover only the managed PID, not child workers or the host OS.
/// Network counters cover instance relay traffic, not physical network adapters.
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MonitorSnapshot {
    #[serde(default)]
    pub internal: Option<astra_model::monitor::InternalSnapshot>,
    pub sampled_at_ms: String,
    pub instance: String,
    pub pid: Option<u32>,
    pub uptime_secs: String,
    /// 100% means one logical CPU. None until a second process sample exists.
    pub cpu_percent: Option<f64>,
    pub memory_bytes: Option<String>,
    pub virtual_memory_bytes: Option<String>,
    pub upload_bytes: Option<String>,
    pub download_bytes: Option<String>,
    pub active_connections: Option<u32>,
    pub total_connections: Option<String>,
    pub error: Option<String>,
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    pub instance: String,
    pub path: PathBuf,
    pub text: String,
    pub revision: String,
    pub kind: InstanceKind,
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Draft {
    pub id: String,
    pub instance: String,
    pub path: PathBuf,
    pub original: String,
    pub text: String,
    pub revision: String,
    pub source_revision: String,
    pub kind: InstanceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "bindings", ts(optional))]
    pub last_save: Option<SaveOutcome>,
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SaveState {
    Applied,
    NextStart,
    Rejected,
    Restored,
    RecoveryFailed,
    OutcomeUnknown,
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaveOutcome {
    pub state: SaveState,
    pub disk_saved: Option<bool>,
    pub runtime_applied: Option<bool>,
    pub recovery: Option<String>,
    pub operation_id: String,
    pub message: String,
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Running,
    Complete,
    Failed,
    OutcomeUnknown,
    NeedsRecovery,
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub id: String,
    pub state: TaskState,
    pub message: String,
    pub started_ms: String,
    pub finished_ms: Option<String>,
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub sequence: String,
    pub operation_id: Option<String>,
    pub kind: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "bindings", ts(optional))]
    pub payload: Option<EventPayload>,
    pub timestamp_ms: String,
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum EventPayload {
    Diagnostics(serde_json::Value),
    NetTool(serde_json::Value),
}
#[cfg_attr(feature = "bindings", derive(TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventBatch {
    pub events: Vec<Event>,
    pub cursor: String,
    pub resync_required: bool,
}

/// Stringify JSON integer counters before browser delivery, preserving booleans and fractions.
pub fn browser_numbers(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Number(n) if n.is_i64() || n.is_u64() => {
            serde_json::Value::String(n.to_string())
        }
        serde_json::Value::Array(values) => values.into_iter().map(browser_numbers).collect(),
        serde_json::Value::Object(values) => values
            .into_iter()
            .map(|(k, v)| (k, browser_numbers(v)))
            .collect(),
        other => other,
    }
}
