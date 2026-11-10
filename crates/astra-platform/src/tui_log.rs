//! Private UI/manager activity log. Configuration payloads are never logged.
use crate::platform::OpenOptionsExt;
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};

/// An append-only activity log. A session log follows its source's newest
/// session file, so long-lived writers move into a session started later.
pub struct Log(Mutex<Target>);
struct Target {
    file: File,
    path: PathBuf,
    follow: Option<(PathBuf, &'static str)>,
    checked: Instant,
}
const FOLLOW_INTERVAL: Duration = Duration::from_secs(2);
static GUI: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Select the process-local frontend activity log before starting background work.
pub fn use_gui_log() {
    GUI.store(true, std::sync::atomic::Ordering::Relaxed);
}
/// Log source name of a frontend viewer pseudo-instance (`@tui`, `@gui`).
pub fn source_name(source: &str) -> Option<&'static str> {
    match source {
        "@tui" => Some("tui"),
        "@gui" => Some("gui"),
        _ => None,
    }
}
/// This process's frontend log source.
pub fn frontend_source() -> &'static str {
    if GUI.load(std::sync::atomic::Ordering::Relaxed) {
        "gui"
    } else {
        "tui"
    }
}
/// Begin a new frontend session; call once when a primary frontend starts,
/// not for detached windows or a resumed restart.
pub fn begin_session(home: &Path) {
    if let Err(error) = crate::log_sessions::new_session(home, frontend_source()) {
        eprintln!("Frontend log session could not be created: {error}");
    }
}
pub fn stamp(ms: u64) -> String {
    chrono::DateTime::<chrono::Local>::from(
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms),
    )
    .format("%H:%M:%S%.3f")
    .to_string()
}
impl Log {
    fn open_file(path: &Path) -> std::io::Result<File> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)
    }
    /// A fixed file, for logs outside the session layout.
    pub fn open(path: &Path) -> std::io::Result<Self> {
        Ok(Self(Mutex::new(Target {
            file: Self::open_file(path)?,
            path: path.to_owned(),
            follow: None,
            checked: Instant::now(),
        })))
    }
    /// The newest session of `source` under `home`, followed as sessions begin.
    pub fn session(home: &Path, source: &'static str) -> std::io::Result<Self> {
        let path = crate::log_sessions::latest_or_new(home, source)?;
        Ok(Self(Mutex::new(Target {
            file: Self::open_file(&path)?,
            path,
            follow: Some((home.to_owned(), source)),
            checked: Instant::now(),
        })))
    }
    /// This process's frontend session log.
    pub fn frontend(home: &Path) -> std::io::Result<Self> {
        Self::session(home, frontend_source())
    }
    pub fn write(&self, category: &str, message: &str) {
        // Single-line records; never accept Debug output of an Action or a profile.
        let message: String = message
            .chars()
            .filter(|c| !c.is_control())
            .take(1000)
            .collect();
        if let Ok(mut target) = self.0.lock() {
            target.follow_latest();
            let _ = writeln!(
                target.file,
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

impl Target {
    fn follow_latest(&mut self) {
        let Some((home, source)) = &self.follow else {
            return;
        };
        if self.checked.elapsed() < FOLLOW_INTERVAL {
            return;
        }
        self.checked = Instant::now();
        if let Some(latest) = crate::log_sessions::latest(home, source) {
            if latest != self.path {
                if let Ok(file) = Log::open_file(&latest) {
                    self.file = file;
                    self.path = latest;
                }
            }
        }
    }
}
