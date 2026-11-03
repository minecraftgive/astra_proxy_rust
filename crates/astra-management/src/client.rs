use crate::v2::{Request, Response, MAX_FRAME_BYTES, VERSION};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Clone, Debug)]
pub struct ManagerClient {
    endpoint: PathBuf,
}
impl ManagerClient {
    pub fn new(endpoint: PathBuf) -> Self {
        Self { endpoint }
    }
    pub fn endpoint(&self) -> &Path {
        &self.endpoint
    }
    /// One attempt only. A lost reply may mean the command executed; query its operation ID.
    pub async fn request_v2(&self, request: Request) -> anyhow::Result<Response> {
        let id = request.id.clone();
        tokio::time::timeout(std::time::Duration::from_secs(2400), async {
            let mut stream = astra_platform::platform::connect_trusted(&self.endpoint).await?;
            send(&mut stream, &request).await?;
            let response: Response = receive(&mut stream).await?;
            anyhow::ensure!(
                response.version == VERSION,
                "management protocol v2 required"
            );
            anyhow::ensure!(response.id == id, "management response identity mismatch");
            Ok(response)
        })
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "management reply timed out; outcome unknown; query operation ID before retrying"
            )
        })?
    }
}
pub async fn send<W: AsyncWrite + Unpin, T: serde::Serialize>(
    stream: &mut W,
    value: &T,
) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(value)?;
    anyhow::ensure!(
        bytes.len() <= MAX_FRAME_BYTES,
        "management frame exceeds limit"
    );
    stream.write_all(b"AMV2").await?;
    stream.write_u32(bytes.len() as u32).await?;
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    Ok(())
}
pub async fn receive<R: AsyncRead + Unpin, T: serde::de::DeserializeOwned>(
    stream: &mut R,
) -> anyhow::Result<T> {
    let mut magic = [0u8; 4];
    stream.read_exact(&mut magic).await?;
    anyhow::ensure!(
        &magic == b"AMV2",
        "management protocol v2 required; restart the outdated manager"
    );
    let length = stream.read_u32().await? as usize;
    anyhow::ensure!(length <= MAX_FRAME_BYTES, "management frame exceeds limit");
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await?;
    Ok(serde_json::from_slice(&bytes)?)
}
