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
mod tests {
    use super::*;
    #[test]
    fn reading_status_never_recovers_or_creates_files() {
        let directory = tempfile::tempdir().unwrap();
        let status = directory.path().join("core.recovery.json");
        assert!(read(&status).unwrap().is_none());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        let notice = Notice {
            state: RecoveryState::Pending,
            timestamp_ms: 1,
            message: "A route requires manual inspection".into(),
            journal: directory.path().join("journal"),
            boot_id: "boot".into(),
        };
        write(&status, &notice).unwrap();
        assert_eq!(
            read(&status).unwrap().unwrap().state,
            RecoveryState::Pending
        );
        assert!(!notice.journal.exists());
    }
}
