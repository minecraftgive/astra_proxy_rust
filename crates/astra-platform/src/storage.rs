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
    Ok(std::env::var_os("ASTRA_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or(crate::platform::home_dir()?.join(".astra/runtime")))
}

pub fn prepare_parent(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        crate::platform::set_private(parent, 0o700)?;
    }
    Ok(())
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
mod tests {
    use super::*;
    #[test]
    fn replacements_are_atomic_private_and_clean_up_candidates() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        atomic_write(&path, b"old", WritePolicy::default()).unwrap();
        atomic_write(&path, b"new", WritePolicy::default()).unwrap();
        assert_eq!(read_bounded(&path, 3).unwrap(), b"new");
        assert!(read_bounded(&path, 2).is_err());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        let invalid = directory.path().join("directory");
        std::fs::create_dir(&invalid).unwrap();
        assert!(atomic_write(&invalid, b"never committed", WritePolicy::default()).is_err());
        assert!(invalid.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn replacement_rejects_a_symlink_without_touching_its_target() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        let link = directory.path().join("link");
        std::fs::write(&target, b"preserved").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(atomic_write(&link, b"changed", WritePolicy::default()).is_err());
        assert_eq!(std::fs::read(target).unwrap(), b"preserved");
    }

    #[test]
    fn profiles_with_same_basename_have_independent_runtime_files() {
        let a = runtime_path(Path::new("profiles/a/proxy.toml"), "subscriptions").unwrap();
        let b = runtime_path(Path::new("profiles/b/proxy.toml"), "subscriptions").unwrap();
        assert_ne!(a, b);
        assert_eq!(
            a,
            runtime_path(Path::new("profiles/a/proxy.toml"), "subscriptions").unwrap()
        );
        assert!(!a.starts_with(std::path::absolute("profiles").unwrap()));
    }
}
