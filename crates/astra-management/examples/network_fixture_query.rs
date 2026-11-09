//! Explicitly invoked JSON bridge for local integration fixtures, not a service.
use astra_management::{v2::Request, ManagerClient};
use std::{io::Read, path::PathBuf, time::Duration};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let endpoint = PathBuf::from(
        std::env::args_os()
            .nth(1)
            .ok_or_else(|| anyhow::anyhow!("local endpoint required"))?,
    );
    if std::env::args().nth(2).as_deref() == Some("--identity") {
        let identity = astra_ipc::control::identity(&endpoint).await?;
        serde_json::to_writer(std::io::stdout(), &identity)?;
    } else {
        let mut bytes = Vec::new();
        std::io::stdin()
            .take(2 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        anyhow::ensure!(bytes.len() <= 2 * 1024 * 1024, "fixture request too large");
        let request: Request = serde_json::from_slice(&bytes)?;
        let response = tokio::time::timeout(
            Duration::from_secs(10),
            ManagerClient::new(endpoint).request_v2(request),
        )
        .await??;
        serde_json::to_writer(std::io::stdout(), &response)?;
    }
    println!();
    Ok(())
}
