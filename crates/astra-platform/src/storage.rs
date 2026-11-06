//! Writable data belongs outside the directory containing user configuration.
use std::{
    io,
    path::{Path, PathBuf},
};

/// A stable per-source filename, including the complete path to avoid collisions
/// between profiles with the same basename. No directories are created on reads.
pub fn runtime_path(source: &Path, category: &str) -> io::Result<PathBuf> {
    runtime_path_in(&runtime_root()?, source, category)
}

/// Resolve a profile cache within an explicitly selected service runtime directory.
pub fn runtime_path_in(root: &Path, source: &Path, category: &str) -> io::Result<PathBuf> {
    let absolute = std::path::absolute(source)?;
    let source = std::fs::canonicalize(&absolute).unwrap_or_else(|_| {
        absolute
            .parent()
            .and_then(|p| std::fs::canonicalize(p).ok())
            .map(|p| p.join(absolute.file_name().unwrap_or_default()))
            .unwrap_or(absolute)
    });
    let mut hash = 0xcbf29ce484222325u64;
    for byte in source.as_os_str().as_encoded_bytes() {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3);
    }
    Ok(root.join(category).join(format!("{hash:016x}.cache")))
}

/// The directory writable runtime state belongs in, also for elevated cores
/// that must keep using the invoking user's documents after dropping root.
pub fn runtime_root() -> io::Result<PathBuf> {
    // Elevated/service cores may have no home environment; resolve it only
    // when the launcher has not supplied an explicit runtime directory.
    std::env::var_os("ASTRA_RUNTIME_DIR").map_or_else(
        || crate::platform::home_dir().map(|home| home.join(".astra/runtime")),
        |runtime| Ok(PathBuf::from(runtime)),
    )
}

pub fn prepare_parent(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        crate::platform::set_private(parent, 0o700)?;
    }
    Ok(())
}

/// Create a private state directory, rejecting symbolic-link destinations.
pub fn private_dir(path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(path)?;
    if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "State directory cannot be a symlink",
        ));
    }
    crate::platform::set_private(path, 0o700)
}

/// Atomically replace a private file with the default storage policy.
pub fn atomic_write_default(path: &Path, bytes: &[u8]) -> io::Result<()> {
    atomic_write(path, bytes, WritePolicy::default())
}

/// File replacement requirements belong to the caller's transaction.
#[derive(Debug, Clone, Copy)]
pub struct WritePolicy {
    pub create_parent: bool,
    pub private: bool,
    pub adopt_owner: bool,
    pub sync_directory: bool,
    pub reject_symlinks: bool,
}
impl Default for WritePolicy {
    fn default() -> Self {
        Self {
            create_parent: false,
            private: true,
            adopt_owner: false,
            sync_directory: true,
            reject_symlinks: true,
        }
    }
}
impl WritePolicy {
    pub fn runtime() -> Self {
        Self {
            create_parent: true,
            adopt_owner: true,
            ..Self::default()
        }
    }
}
/// Read at most `limit` bytes; an over-limit file is never silently truncated.
pub fn read_bounded(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file exceeds size limit",
        ));
    }
    Ok(bytes)
}
/// Atomically replace a file using a unique temporary file in the same directory.
/// Candidate validation and multi-file transaction journals remain the caller's responsibility.
pub fn atomic_write(path: &Path, bytes: &[u8], policy: WritePolicy) -> io::Result<()> {
    use std::io::Write;
    if policy.create_parent {
        prepare_parent(path)?;
    }
    if policy.reject_symlinks {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if !metadata.is_file() || metadata.file_type().is_symlink() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "replacement target must be a regular file",
                ))
            }
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    if policy.private {
        crate::platform::set_private(temporary.path(), 0o600)?;
    }
    if policy.adopt_owner {
        crate::platform::adopt_owner(temporary.path())?;
    }
    temporary.write_all(bytes)?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    if policy.sync_directory {
        sync_directory(parent)?;
    }
    Ok(())
}
pub fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    std::fs::File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
/// Atomically replace a private runtime document; never touch source files.
pub fn write_json(path: &Path, value: &impl serde::Serialize) -> io::Result<()> {
    atomic_write(path, &serde_json_bytes(value)?, WritePolicy::runtime())
}

fn serde_json_bytes(value: &impl serde::Serialize) -> io::Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(io::Error::other)
}

#[cfg(test)]
#[path = "test/storage_test.rs"]
mod tests;
