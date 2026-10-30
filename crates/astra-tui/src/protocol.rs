pub use astra_ipc::logging::LogLevel;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Proxy,
    Rule,
}
impl Kind {
    pub fn binary(self) -> &'static str {
        match self {
            Self::Proxy => {
                if cfg!(windows) {
                    "astra-proxy.exe"
                } else {
                    "astra-proxy"
                }
            }
            Self::Rule => {
                if cfg!(windows) {
                    "astra-rule.exe"
                } else {
                    "astra-rule"
                }
            }
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
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
        request: astra_ipc::RouteRequest,
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
    /// Streams `astra_ipc::nettool::Event`s; the manager fills the request's configuration.
    NetTool {
        name: String,
        #[serde(with = "json_text")]
        request: astra_ipc::nettool::Request,
    },
    StoragePaths,
    Runtime {
        name: String,
        command: astra_ipc::runtime::Command,
        expected_pid: Option<u32>,
    },
    Session,
    WatchList,
    WatchRuntime {
        name: String,
        expected_pid: u32,
        interval_ms: u64,
    },
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
    let seconds = if matches!(action, Action::Shutdown) {
        2400
    } else {
        180
    };
    let lane = if matches!(
        action,
        Action::List
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
) -> anyhow::Result<astra_ipc::platform::LocalStream> {
    let mut stream = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        astra_ipc::platform::connect_trusted(socket),
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
    use astra_ipc::nettool::{Egress, Probe};

    #[tokio::test]
    async fn network_tool_requests_survive_management_frames() {
        let (mut client, mut server) = astra_ipc::platform::stream_pair().await.unwrap();
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
            let request = astra_ipc::nettool::Request {
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
