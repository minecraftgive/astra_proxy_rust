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
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub version: u16,
    pub action: Action,
}
#[derive(Debug, Serialize, Deserialize)]
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
pub async fn call(socket: &std::path::Path, action: Action) -> anyhow::Result<Reply> {
    let seconds = if matches!(action, Action::Shutdown) {
        2400
    } else {
        180
    };
    tokio::time::timeout(std::time::Duration::from_secs(seconds), async {
        let mut stream = astra_ipc::platform::LocalStream::connect(socket).await?;
        astra_ipc::send_management(
            &mut stream,
            &Request {
                version: astra_ipc::API_VERSION,
                action,
            },
        )
        .await?;
        let reply: Reply = astra_ipc::receive_management(&mut stream).await?;
        anyhow::ensure!(
            reply.version == astra_ipc::API_VERSION,
            "Unsupported manager version"
        );
        if let Some(error) = &reply.error {
            anyhow::bail!("{error}");
        }
        Ok(reply)
    })
    .await?
}
