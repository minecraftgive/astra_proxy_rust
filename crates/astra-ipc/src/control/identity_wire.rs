//! Preserve legacy bincode identity bytes while adding a bounded JSON extension.
use super::*;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncWrite, AsyncWriteExt};

const MAGIC: &[u8] = b"ASTRA-CAPS\0";
const MAX_EXTENSION: usize = 4096;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Extension {
    version: u16,
    capabilities: Vec<String>,
}

pub(super) fn encode(identity: &Identity) -> io::Result<Vec<u8>> {
    let mut bytes = bincode::serialize(identity).map_err(invalid)?;
    if let Some(capabilities) = &identity.capabilities {
        let extension = serde_json::to_vec(&Extension {
            version: 1,
            capabilities: capabilities.clone(),
        })
        .map_err(invalid)?;
        validate_capabilities(capabilities)?;
        if extension.len() > MAX_EXTENSION {
            return Err(invalid("identity capability extension exceeds limit"));
        }
        bytes.extend_from_slice(MAGIC);
        bytes.extend(extension);
    }
    Ok(bytes)
}
pub(super) fn decode(bytes: &[u8]) -> io::Result<Identity> {
    let mut identity: Identity = bincode::deserialize(bytes).map_err(invalid)?;
    let prefix = bincode::serialized_size(&identity).map_err(invalid)? as usize;
    let extension = bytes
        .get(prefix..)
        .ok_or_else(|| invalid("invalid identity prefix"))?;
    if extension.is_empty() {
        return Ok(identity);
    }
    let extension = extension
        .strip_prefix(MAGIC)
        .ok_or_else(|| invalid("unknown identity extension"))?;
    if extension.len() > MAX_EXTENSION {
        return Err(invalid("identity capability extension exceeds limit"));
    }
    let extension: Extension = serde_json::from_slice(extension).map_err(invalid)?;
    if extension.version != 1 {
        return Err(invalid("unsupported identity capability extension version"));
    }
    validate_capabilities(&extension.capabilities)?;
    identity.capabilities = Some(extension.capabilities);
    Ok(identity)
}
fn validate_capabilities(capabilities: &[String]) -> io::Result<()> {
    if capabilities.len() > 64
        || capabilities.iter().any(|s| {
            s.is_empty()
                || s.len() > 80
                || !s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        })
    {
        return Err(invalid("invalid core capability list"));
    }
    Ok(())
}
fn invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}
pub(super) async fn send(
    stream: &mut (impl AsyncWrite + Unpin),
    identity: &Identity,
) -> io::Result<()> {
    let bytes = encode(identity)?;
    if bytes.len() > 16 * 1024 {
        return Err(invalid("identity frame exceeds limit"));
    }
    stream.write_u32(bytes.len() as u32).await?;
    stream.write_all(&bytes).await
}
