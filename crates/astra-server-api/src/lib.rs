//! Owner-only control protocol shared by the independent server and its TUI.
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

pub const VERSION: u16 = 1;
#[derive(Serialize, Deserialize)]
pub enum Action {
    Status,
    Stop,
    Reload { path: Option<PathBuf> },
    Logs,
}
#[derive(Serialize, Deserialize)]
pub struct Request {
    pub version: u16,
    pub action: Action,
}
#[derive(Default, Serialize, Deserialize)]
pub struct Reply {
    pub version: u16,
    pub error: Option<String>,
    pub pid: u32,
    pub uptime_secs: u64,
    pub active: u64,
    pub accepted: u64,
    pub rejected: u64,
    pub upload: u64,
    pub download: u64,
    pub config: PathBuf,
    pub listeners: Vec<String>,
    pub logs: String,
}
pub async fn call(path: &Path, action: Action) -> anyhow::Result<Reply> {
    tokio::time::timeout(Duration::from_secs(20), async {
        let mut stream = astra_ipc::platform::LocalStream::connect(path).await?;
        astra_ipc::send_management(
            &mut stream,
            &Request {
                version: VERSION,
                action,
            },
        )
        .await?;
        let reply: Reply = astra_ipc::receive_management(&mut stream).await?;
        anyhow::ensure!(
            reply.version == VERSION,
            "server management version mismatch"
        );
        if let Some(error) = &reply.error {
            anyhow::bail!("{error}");
        }
        Ok(reply)
    })
    .await?
}
