//! Writable data belongs outside the directory containing user configuration.
use std::{
    io,
    path::{Path, PathBuf},
};

/// A stable per-source filename, including the complete path to avoid collisions
/// between profiles with the same basename. No directories are created on reads.
pub fn runtime_path(source: &Path, category: &str) -> io::Result<PathBuf> {
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
    let root = std::env::var_os("ASTRA_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or(crate::platform::home_dir()?.join(".astra/runtime"));
    Ok(root.join(category).join(format!("{hash:016x}.cache")))
}

pub fn prepare_parent(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        crate::platform::set_private(parent, 0o700)?;
    }
    Ok(())
}

/// Atomically replace a private runtime document; never touch source files.
pub fn write_json(path: &Path, value: &impl serde::Serialize) -> io::Result<()> {
    use crate::platform::OpenOptionsExt;
    use std::io::Write;
    prepare_parent(path)?;
    let bytes = serde_json_bytes(value)?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    let temporary = path.with_extension(format!("{}-{nonce}.tmp", std::process::id()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}
fn serde_json_bytes(value: &impl serde::Serialize) -> io::Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
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
