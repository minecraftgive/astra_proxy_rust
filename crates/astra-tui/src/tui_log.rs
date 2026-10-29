//! Private UI/manager activity log. Configuration payloads are never logged.
use astra_ipc::platform::OpenOptionsExt;
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::Path,
    sync::Mutex,
};

pub struct Log(Mutex<File>);
pub fn stamp(ms: u64) -> String {
    chrono::DateTime::<chrono::Local>::from(
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms),
    )
    .format("%H:%M:%S%.3f")
    .to_string()
}
impl Log {
    pub fn open(path: &Path) -> std::io::Result<Self> {
        Ok(Self(Mutex::new(
            OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(path)?,
        )))
    }
    pub fn write(&self, category: &str, message: &str) {
        // Single-line records; never accept Debug output of an Action or a profile.
        let message: String = message
            .chars()
            .filter(|c| !c.is_control())
            .take(1000)
            .collect();
        if let Ok(mut file) = self.0.lock() {
            let _ = writeln!(
                file,
                "[{}] INFO {} {}",
                stamp(astra_ipc::progress::now_ms()),
                category,
                message
            );
        }
    }
}
