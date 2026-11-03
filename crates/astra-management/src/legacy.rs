pub use astra_model::logging::LogLevel;
pub mod updates;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct LogCursor {
    pub device: u64,
    pub inode: u64,
    pub offset: u64,
}
#[derive(Debug, Serialize, Deserialize)]
pub enum LogEvent {
    Data {
        bytes: Vec<u8>,
        cursor: LogCursor,
        reset: bool,
        skip_prefix: bool,
    },
    Waiting,
    Error(String),
}

pub use astra_model::CoreKind as Kind;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Instance {
    pub name: String,
    pub kind: Kind,
    pub config: PathBuf,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Row {
    pub instance: Instance,
    pub pid: Option<u32>,
    pub state: String,
    pub uptime: u64,
    pub changed: bool,
}
/// Optional JSON data in List replies keeps the existing bincode row layout
/// compatible with older frontends and managers.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct InstanceDetails {
    pub network_recovery: Option<astra_model::recovery::Notice>,
    pub server_status: Option<crate::v2::ServerSnapshot>,
    pub modes: Vec<String>,
    pub listeners: Vec<String>,
    pub bindings: Vec<String>,
    pub outbounds: Vec<String>,
    pub outbound_count: usize,
    pub rules: usize,
    pub log_level: LogLevel,
    pub config_unavailable: bool,
    pub diagnostics_supported: bool,
    pub node_tags_supported: bool,
    pub explanation_supported: bool,
    pub subscription_nodes_supported: bool,
    pub runtime_supported: bool,
    /// Enabling TUN needs the sudo hand-off on this platform and account.
    pub elevation_supported: bool,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubscriptionNodes {
    pub url: String,
    pub tags: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoragePaths {
    pub config: PathBuf,
    pub state: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Action {
    List,
    Add(Instance),
    Remove(String),
    Start(String),
    Stop(String),
    Restart(String),
    Reload(String),
    SetConfig(String, PathBuf),
    Read(String),
    Save {
        name: String,
        original: String,
        text: String,
    },
    CloneConfig(String, PathBuf),
    Validate(String),
    Logs(String),
    Shutdown,
    /// Returns the default document only when a missing file was created.
    PrepareConfig(String),
    Create {
        name: String,
        kind: Kind,
    },
    FollowLogs {
        name: String,
        cursor: Option<LogCursor>,
    },
    GetLogLevel(String),
    SetLogLevel {
        name: String,
        level: LogLevel,
    },
    BindStart {
        name: String,
        targets: Vec<String>,
    },
    Diagnostics(String),
    NodeTags(String),
    ExplainRoute {
        name: String,
        request: astra_model::RouteRequest,
    },
    UpdateSubscriptions(String),
    SubscriptionNodes(String),
    DiagnosticTargets(String),
    DiagnosticsSelected {
        name: String,
        targets: Vec<String>,
    },
    DiagnosticsStream {
        name: String,
        targets: Vec<String>,
    },
    /// Streams `astra_model::nettool::Event`s; the manager fills the request's configuration.
    NetTool {
        name: String,
        #[serde(with = "json_text")]
        request: astra_model::nettool::Request,
    },
    StoragePaths,
    Runtime {
        name: String,
        command: astra_model::runtime::Command,
        expected_pid: Option<u32>,
    },
    Session,
    WatchList,
    WatchRuntime {
        name: String,
        expected_pid: u32,
        interval_ms: u64,
    },
    /// Unix only: reserve a private hand-off endpoint for a sudo-launched core.
    /// The reply carries `astra_platform::unix_handoff::Offer` as JSON.
    PrepareElevation {
        name: String,
        expected_pid: Option<u32>,
    },
    CancelElevation {
        name: String,
        ticket: String,
    },
    /// Blocks until the prepared core is serving, then returns its runtime
    /// snapshot as JSON.
    WaitElevation {
        name: String,
        ticket: String,
    },
    /// Append-only: keep existing bincode discriminants stable for older UIs.
    UpdatePackage(PathBuf),
    UpdateStatus,
    Rollback,
    UpdateTransaction {
        #[serde(with = "json_text")]
        request: updates::UpdateRequest,
    },
    RecoverUpdate {
        operation_id: String,
    },
    WatchUpdates,
    UpdateOperationStatus {
        operation_id: String,
    },
    /// Read-only process and network diagnostics, JSON response.
    Monitor {
        name: String,
    },
}
impl Action {
    /// Safe activity description: never includes configuration text, URLs or credentials.
    pub fn summary(&self) -> String {
        let label = match self {
            Self::List | Self::WatchList => "List instances",
            Self::Start(_) => "Start instance",
            Self::Stop(_) => "Stop instance",
            Self::Restart(_) => "Restart instance",
            Self::Reload(_) => "Reload configuration",
            Self::Read(_) => "Read configuration",
            Self::Save { .. } => "Save configuration",
            Self::Create { .. } | Self::Add(_) => "Create instance",
            Self::Remove(_) => "Remove instance",
            Self::Validate(_) => "Validate configuration",
            Self::Logs(_) | Self::FollowLogs { .. } => "Read logs",
            Self::Runtime { .. } => "Apply runtime command",
            Self::WatchRuntime { .. } => "Watch runtime",
            Self::Monitor { .. } => "Read monitor",
            Self::UpdatePackage(_) | Self::UpdateTransaction { .. } => "Update release",
            Self::Rollback => "Roll back release",
            Self::RecoverUpdate { .. } => "Recover update",
            Self::Shutdown => "Shut down manager",
            _ => "Management request",
        };
        label.into()
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub version: u16,
    pub action: Action,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reply {
    pub version: u16,
    pub error: Option<String>,
    pub rows: Vec<Row>,
    pub text: String,
}
impl Reply {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            version: astra_ipc::API_VERSION,
            error: None,
            rows: vec![],
            text: text.into(),
        }
    }
}
/// Management frames are bincode, which cannot decode serde-tagged enums such as
/// `nettool::Egress` and `nettool::Probe`; fields holding them cross as JSON text.
mod json_text {
    use serde::{de::DeserializeOwned, Deserialize, Deserializer, Serialize, Serializer};
    pub fn serialize<T: Serialize, S: Serializer>(
        value: &T,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serde_json::to_string(value)
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
    pub fn deserialize<'de, T: DeserializeOwned, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<T, D::Error> {
        serde_json::from_str(&String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

pub async fn call(socket: &std::path::Path, action: Action) -> anyhow::Result<Reply> {
    use std::{
        collections::HashMap,
        sync::{Mutex, OnceLock},
    };
    type Peer = astra_ipc::session::Peer<Request, Reply>;
    static POOL: OnceLock<Mutex<HashMap<(PathBuf, u8), Peer>>> = OnceLock::new();
    let seconds = if matches!(
        action,
        Action::Shutdown
            | Action::UpdatePackage(_)
            | Action::UpdateTransaction { .. }
            | Action::RecoverUpdate { .. }
    ) {
        2400
    } else {
        180
    };
    let lane = if matches!(action, Action::WaitElevation { .. }) {
        // A hand-off can legitimately wait for minutes; keep it off the lane
        // used by quick mutations so the UI stays responsive.
        2
    } else if matches!(
        action,
        Action::List
            | Action::Monitor { .. }
            | Action::UpdateStatus
            | Action::UpdateOperationStatus { .. }
            | Action::StoragePaths
            | Action::GetLogLevel(_)
            | Action::NodeTags(_)
            | Action::SubscriptionNodes(_)
    ) {
        0
    } else {
        1
    };
    let peer = {
        let mut pool = POOL.get_or_init(Default::default).lock().unwrap();
        pool.retain(|_, peer| !peer.is_closed());
        if pool.len() >= 64 {
            if let Some(key) = pool.keys().next().cloned() {
                pool.remove(&key);
            }
        }
        pool.entry((socket.to_owned(), lane))
            .or_insert_with(|| {
                Peer::new(socket.to_owned(), || Request {
                    version: astra_ipc::API_VERSION,
                    action: Action::Session,
                })
            })
            .clone()
    };
    let reply = peer
        .request(
            Request {
                version: astra_ipc::API_VERSION,
                action,
            },
            std::time::Duration::from_secs(seconds),
        )
        .await?;
    anyhow::ensure!(
        reply.version == astra_ipc::API_VERSION,
        "Unsupported manager version"
    );
    if let Some(error) = &reply.error {
        anyhow::bail!("{error}");
    }
    Ok(reply)
}
pub async fn subscribe(
    socket: &std::path::Path,
    action: Action,
) -> anyhow::Result<astra_platform::platform::LocalStream> {
    let mut stream = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        astra_platform::platform::connect_trusted(socket),
    )
    .await??;
    astra_ipc::send_management(
        &mut stream,
        &Request {
            version: astra_ipc::API_VERSION,
            action,
        },
    )
    .await?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_model::nettool::Egress;
    use astra_model::nettool::Probe;

    #[tokio::test]
    async fn update_actions_preserve_existing_discriminants_and_json_request_identity() {
        use tokio::io::AsyncReadExt;
        let (mut client, mut server) = astra_platform::platform::stream_pair().await.unwrap();
        for (action, expected) in [
            (Action::List, 0),
            (Action::UpdateStatus, 38),
            (Action::Rollback, 39),
            (Action::WatchUpdates, 42),
        ] {
            astra_ipc::send_management(&mut client, &action)
                .await
                .unwrap();
            assert_eq!(server.read_u32().await.unwrap(), 4);
            assert_eq!(server.read_u32_le().await.unwrap(), expected);
        }
        let request = updates::UpdateRequest::new(updates::UpdateIntent::Rollback {
            target_directory: PathBuf::from("fixed-release"),
        });
        astra_ipc::send_management(
            &mut client,
            &Action::UpdateTransaction {
                request: request.clone(),
            },
        )
        .await
        .unwrap();
        let received: Action = astra_ipc::receive_management(&mut server).await.unwrap();
        assert!(matches!(received,Action::UpdateTransaction {request:actual} if actual == request));
    }

    #[tokio::test]
    async fn network_tool_requests_survive_management_frames() {
        let (mut client, mut server) = astra_platform::platform::stream_pair().await.unwrap();
        let egresses = [
            Egress::System,
            Egress::Interface("以太网 2".into()),
            Egress::Outbound("HK 01".into()),
        ];
        let probes = [
            Probe::Dns {
                name: "example.test".into(),
                record: "AAAA".into(),
                server: "tls://1.1.1.1".into(),
            },
            Probe::Tcp {
                host: "2001:db8::1".into(),
                port: 443,
            },
            Probe::Icmp {
                host: "192.0.2.1".into(),
            },
        ];
        for (egress, probe) in egresses.into_iter().zip(probes) {
            let request = astra_model::nettool::Request {
                config: "standalone=true\n".into(),
                egress: egress.clone(),
                probe: probe.clone(),
                count: 3,
                interval_ms: 200,
                timeout_ms: 3000,
            };
            astra_ipc::send_management(
                &mut client,
                &Request {
                    version: astra_ipc::API_VERSION,
                    action: Action::NetTool {
                        name: "proxy".into(),
                        request,
                    },
                },
            )
            .await
            .unwrap();
            let received: Request = astra_ipc::receive_management(&mut server).await.unwrap();
            let (name, request) = match received.action {
                Action::NetTool { name, request } => (name, request),
                other => panic!("{other:?}"),
            };
            assert_eq!(name, "proxy");
            assert_eq!((request.egress, request.probe), (egress, probe));
            assert_eq!(
                (request.count, request.interval_ms, request.timeout_ms),
                (3, 200, 3000)
            );
            assert_eq!(request.config, "standalone=true\n");
        }
    }
}
