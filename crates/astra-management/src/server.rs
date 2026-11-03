//! Owner-only local control client for the independent server core.
pub use astra_model::server::*;
use std::{path::Path, time::Duration};
pub async fn call(path: &Path, action: Action) -> anyhow::Result<Reply> {
    tokio::time::timeout(Duration::from_secs(20), async {
        let mut stream = astra_platform::platform::connect_trusted(path).await?;
        let peer = astra_platform::platform::peer_process_id(&stream)?;
        astra_ipc::send_management(
            &mut stream,
            &Request {
                version: VERSION,
                action,
            },
        )
        .await?;
        let reply: Reply = astra_ipc::receive_management_versioned(&mut stream, VERSION).await?;
        anyhow::ensure!(
            reply.pid == peer
                && reply
                    .identity
                    .as_ref()
                    .is_some_and(|identity| identity.process.pid == peer),
            "Server reply identity does not match IPC peer"
        );
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
