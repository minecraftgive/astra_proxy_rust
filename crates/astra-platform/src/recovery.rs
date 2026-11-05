//! Read-only status reporting is independent of privileged network restoration.
pub use astra_model::recovery::{Notice, RecoveryState};
use std::{
    io,
    path::{Path, PathBuf},
};

pub fn path(profile: &Path, control: Option<&Path>) -> io::Result<PathBuf> {
    match control {
        Some(control) => Ok(control.with_extension("recovery.json")),
        None => crate::storage::runtime_path(profile, "recovery"),
    }
}
pub fn write(path: &Path, notice: &Notice) -> io::Result<()> {
    crate::storage::write_json(path, notice)
}
pub fn read(path: &Path) -> io::Result<Option<Notice>> {
    let bytes = match crate::storage::read_bounded(path, 64 * 1024) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(io::Error::other)
}

#[cfg(test)]
#[path = "test/recovery_test.rs"]
mod tests;
