//! Bounded, resumable log transport on a dedicated management connection.
use crate::protocol::*;
use astra_platform::platform::LocalStream as UnixStream;
use std::time::Duration;
use tokio::time::timeout;

pub async fn connect(
    socket: &std::path::Path,
    name: String,
    cursor: Option<LogCursor>,
) -> anyhow::Result<UnixStream> {
    timeout(Duration::from_secs(5), async {
        let mut stream =
            crate::protocol::subscribe(socket, Action::FollowLogs { name, cursor }).await?;
        let reply: Reply = astra_ipc::receive_management(&mut stream).await?;
        anyhow::ensure!(
            reply.version == astra_ipc::API_VERSION,
            "Unsupported log stream version"
        );
        if let Some(error) = reply.error {
            anyhow::bail!("{error}");
        }
        Ok(stream)
    })
    .await?
}
