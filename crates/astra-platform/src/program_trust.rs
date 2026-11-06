//! Advisory installation diagnostics, separate from executable validity and IPC trust.
use std::{
    fs::File,
    io::{self, Write},
    path::{Path, PathBuf},
};

/// A usable executable path with an optional, non-blocking installation warning.
#[derive(Debug)]
pub struct ProgramTrust {
    pub path: PathBuf,
    pub warning: Option<String>,
}

/// Persist a warning independently of the tracing filter. A log failure must
/// never turn an advisory installation check into a launch requirement.
/// Call after rotating the instance log so the warning stays with this launch.
pub fn append_warning(log: &Path, message: &str) {
    if let Ok(mut file) = open_log(log) {
        let message: String = message.chars().filter(|c| !c.is_control()).collect();
        let record = format!(
            "[{}] WARN installation: {message}\n",
            chrono::Local::now().format("%H:%M:%S%.3f")
        );
        let _ = file.write_all(record.as_bytes());
    }
}

#[cfg(windows)]
fn open_log(path: &Path) -> io::Result<File> {
    crate::elevation::open_log(path)
}

#[cfg(unix)]
fn open_log(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.nlink() != 1 || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "installation warning log must be a regular, single-link file owned by this user",
        ));
    }
    Ok(file)
}

#[cfg(test)]
#[path = "test/program_trust_test.rs"]
mod tests;
