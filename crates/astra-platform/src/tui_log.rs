//! Private UI/manager activity log. Configuration payloads are never logged.
use crate::platform::OpenOptionsExt;
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::Path,
    sync::Mutex,
};

pub struct Log(Mutex<File>);
static GUI: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Select the process-local frontend activity log before starting background work.
pub fn use_gui_log() {
    GUI.store(true, std::sync::atomic::Ordering::Relaxed);
}
pub fn source_file(source: &str) -> Option<&'static str> {
    match source {
        "@tui" => Some("tui.log"),
        "@gui" => Some("gui.log"),
        _ => None,
    }
}
pub fn frontend_path(home: &Path) -> std::path::PathBuf {
    home.join(if GUI.load(std::sync::atomic::Ordering::Relaxed) {
        "gui.log"
    } else {
        "tui.log"
    })
}
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
                stamp(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64
                ),
                category,
                message
            );
        }
    }
}
