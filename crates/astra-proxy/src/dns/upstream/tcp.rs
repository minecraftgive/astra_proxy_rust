use anyhow::{ensure, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
pub(crate) async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    query: &[u8],
) -> Result<Vec<u8>> {
    ensure!(
        (12..=65535).contains(&query.len()),
        "invalid DNS message size"
    );
    stream.write_u16(query.len() as u16).await?;
    stream.write_all(query).await?;
    stream.flush().await?;
    let size = stream.read_u16().await? as usize;
    ensure!(size >= 12, "invalid DNS frame");
    let mut bytes = vec![0; size];
    stream.read_exact(&mut bytes).await?;
    Ok(bytes)
}
