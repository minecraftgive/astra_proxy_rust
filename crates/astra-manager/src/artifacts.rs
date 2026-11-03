//! Resumable bounded artifacts. An artifact ID can never address an arbitrary host path.
use anyhow::{Context, Result};
use astra_management::v2::*;
use base64::Engine;
use sha2::Digest;
use std::{
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};
const CHUNK: usize = 256 * 1024;
pub(crate) struct PackageView {
    pub path: PathBuf,
    directory: PathBuf,
}
impl Drop for PackageView {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_dir(&self.directory);
    }
}
pub(crate) fn package_view(root: &Path, id: &str) -> Result<PackageView> {
    let source = complete_path(root, id)?;
    let (meta, _) = paths(root, id)?;
    let artifact = load(&meta)?;
    let name = artifact.name.to_ascii_lowercase();
    let extension = if name.ends_with(".zip") {
        "zip"
    } else if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        "tar.gz"
    } else {
        anyhow::bail!("Update artifacts must be ZIP or TAR.GZ packages");
    };
    let directory = root.join("staging").join(uuid::Uuid::new_v4().to_string());
    crate::private_dir(&directory)?;
    let view = PackageView {
        path: directory.join(format!("package.{extension}")),
        directory,
    };
    if std::fs::hard_link(&source, &view.path).is_err() {
        std::fs::copy(source, &view.path)?;
    }
    astra_platform::platform::set_private(&view.path, 0o600)?;
    Ok(view)
}
fn paths(root: &Path, id: &str) -> Result<(PathBuf, PathBuf)> {
    anyhow::ensure!(
        !id.is_empty()
            && id.len() <= 100
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "invalid artifact ID"
    );
    Ok((
        root.join(format!("{id}.json")),
        root.join(format!("{id}.blob")),
    ))
}
fn load(path: &Path) -> Result<Artifact> {
    Ok(serde_json::from_slice(
        &astra_platform::storage::read_bounded(path, 16 * 1024)?,
    )?)
}
fn persist(path: &Path, artifact: &Artifact) -> Result<()> {
    crate::atomic_write(path, &serde_json::to_vec(artifact)?)
}
pub fn complete_path(root: &Path, id: &str) -> Result<PathBuf> {
    let (meta, body) = paths(root, id)?;
    let artifact = load(&meta)?;
    anyhow::ensure!(artifact.complete, "Artifact is not verified and complete");
    anyhow::ensure!(
        body.metadata()?.len().to_string() == artifact.size,
        "Artifact size changed"
    );
    Ok(body)
}
pub fn handle(root: &Path, max: u64, command: ArtifactCommand) -> Result<ResponseBody> {
    crate::private_dir(root)?;
    match command {
        ArtifactCommand::Begin {
            id,
            name,
            size,
            sha256,
        } => {
            let total = size.parse::<u64>().context("invalid artifact size")?;
            anyhow::ensure!(total <= max, "Artifact exceeds configured upload limit");
            anyhow::ensure!(
                sha256.len() == 64 && sha256.bytes().all(|b| b.is_ascii_hexdigit()),
                "invalid SHA-256"
            );
            let (meta, body) = paths(root, &id)?;
            if meta.exists() {
                let existing = load(&meta)?;
                anyhow::ensure!(
                    existing.size == size && existing.sha256.eq_ignore_ascii_case(&sha256),
                    "Conflict: artifact ID identifies different content"
                );
                return Ok(ResponseBody::Artifact(existing));
            }
            let mut reserved = 0u64;
            let mut count = 0;
            for entry in std::fs::read_dir(root)? {
                let path = entry?.path();
                if path.extension().is_some_and(|v| v == "json") {
                    let artifact = load(&path)?;
                    reserved = reserved.saturating_add(artifact.size.parse::<u64>()?);
                    count += 1;
                }
            }
            anyhow::ensure!(
                count < 64 && reserved.saturating_add(total) <= max.saturating_mul(2),
                "Artifact storage quota reached; remove completed artifacts before uploading"
            );
            use astra_platform::platform::OpenOptionsExt;
            let file = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&body)?;
            file.sync_all()?;
            let artifact = Artifact {
                id,
                name: name.chars().filter(|c| !c.is_control()).take(120).collect(),
                size: total.to_string(),
                received: "0".into(),
                sha256: sha256.to_ascii_lowercase(),
                complete: false,
            };
            persist(&meta, &artifact)?;
            Ok(ResponseBody::Artifact(artifact))
        }
        ArtifactCommand::Write {
            id,
            offset,
            base64,
            sha256,
        } => {
            anyhow::ensure!(
                base64.len() <= CHUNK.div_ceil(3) * 4,
                "Artifact chunk exceeds limit"
            );
            let bytes = base64::engine::general_purpose::STANDARD.decode(base64)?;
            anyhow::ensure!(bytes.len() <= CHUNK, "Artifact chunk exceeds limit");
            anyhow::ensure!(
                sha256.eq_ignore_ascii_case(&format!("{:x}", sha2::Sha256::digest(&bytes))),
                "Artifact chunk SHA-256 mismatch"
            );
            let (meta, body) = paths(root, &id)?;
            let mut artifact = load(&meta)?;
            anyhow::ensure!(!artifact.complete, "Artifact already finalized");
            let offset = offset.parse::<u64>()?;
            let received = artifact.received.parse::<u64>()?;
            let total = artifact.size.parse::<u64>()?;
            let end = offset
                .checked_add(bytes.len() as u64)
                .context("artifact offset overflow")?;
            anyhow::ensure!(end <= total, "Artifact write exceeds declared size");
            let mut file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(body)?;
            if offset < received {
                anyhow::ensure!(
                    end <= received,
                    "Conflict: overlapping chunk extends past persisted cursor"
                );
                file.seek(SeekFrom::Start(offset))?;
                let mut previous = vec![0; bytes.len()];
                file.read_exact(&mut previous)?;
                anyhow::ensure!(
                    previous == bytes,
                    "Conflict: retried chunk contains different data"
                );
            } else {
                anyhow::ensure!(offset == received, "Conflict: artifact cursor changed");
                // A crash after bytes but before metadata is safe: replay rewrites the same offset.
                file.seek(SeekFrom::Start(offset))?;
                file.write_all(&bytes)?;
                file.sync_all()?;
                artifact.received = end.to_string();
                persist(&meta, &artifact)?;
            }
            Ok(ResponseBody::Artifact(artifact))
        }
        ArtifactCommand::Finish { id } => {
            let (meta, body) = paths(root, &id)?;
            let mut artifact = load(&meta)?;
            anyhow::ensure!(
                artifact.received == artifact.size,
                "Artifact upload is incomplete"
            );
            let mut file = std::fs::File::open(body)?;
            anyhow::ensure!(
                file.metadata()?.len().to_string() == artifact.size,
                "Artifact length differs from manifest"
            );
            let mut hash = sha2::Sha256::new();
            let mut buffer = [0; 64 * 1024];
            loop {
                let size = file.read(&mut buffer)?;
                if size == 0 {
                    break;
                }
                hash.update(&buffer[..size]);
            }
            anyhow::ensure!(
                format!("{:x}", hash.finalize()) == artifact.sha256,
                "Artifact SHA-256 mismatch; content was not accepted"
            );
            artifact.complete = true;
            persist(&meta, &artifact)?;
            Ok(ResponseBody::Artifact(artifact))
        }
        ArtifactCommand::Read { id, offset, limit } => {
            let path = complete_path(root, &id)?;
            let mut file = std::fs::File::open(path)?;
            let size = file.metadata()?.len();
            let offset = offset.parse::<u64>()?;
            anyhow::ensure!(offset <= size, "Artifact read cursor exceeds length");
            file.seek(SeekFrom::Start(offset))?;
            let mut bytes = vec![
                0;
                usize::try_from((size - offset).min(u64::from(
                    limit.unwrap_or(CHUNK as u32).min(CHUNK as u32)
                )))?
            ];
            file.read_exact(&mut bytes)?;
            let next = offset + bytes.len() as u64;
            Ok(ResponseBody::ArtifactChunk(ArtifactChunk {
                id,
                offset: offset.to_string(),
                next: next.to_string(),
                base64: base64::engine::general_purpose::STANDARD.encode(bytes),
                eof: next == size,
            }))
        }
        ArtifactCommand::Remove { id } => {
            let (meta, body) = paths(root, &id)?;
            let _ = load(&meta)?;
            std::fs::remove_file(body)?;
            std::fs::remove_file(meta)?;
            Ok(ResponseBody::Done {
                message: "Artifact removed".into(),
            })
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retries_are_idempotent_and_only_verified_bytes_can_be_read() {
        let root = tempfile::tempdir().unwrap();
        let data = b"verified content";
        let digest = format!("{:x}", sha2::Sha256::digest(data));
        handle(
            root.path(),
            1024,
            ArtifactCommand::Begin {
                id: "a".into(),
                name: "../irrelevant.zip".into(),
                size: data.len().to_string(),
                sha256: digest,
            },
        )
        .unwrap();
        let command = ArtifactCommand::Write {
            id: "a".into(),
            offset: "0".into(),
            base64: base64::engine::general_purpose::STANDARD.encode(data),
            sha256: format!("{:x}", sha2::Sha256::digest(data)),
        };
        handle(root.path(), 1024, command.clone()).unwrap();
        handle(root.path(), 1024, command).unwrap();
        assert!(complete_path(root.path(), "a").is_err());
        handle(
            root.path(),
            1024,
            ArtifactCommand::Finish { id: "a".into() },
        )
        .unwrap();
        assert_eq!(
            std::fs::read(complete_path(root.path(), "a").unwrap()).unwrap(),
            data
        );
        assert!(paths(root.path(), "../escape").is_err());
        let view = package_view(root.path(), "a").unwrap();
        assert_eq!(view.path.extension().unwrap(), "zip");
        assert_eq!(std::fs::read(&view.path).unwrap(), data);
        let path = view.path.clone();
        drop(view);
        assert!(!path.exists());
    }
}
