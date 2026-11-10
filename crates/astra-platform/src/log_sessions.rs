//! Per-session log files: `<home>/logs/<source>/<UTC timestamp>.log`.
//!
//! Each process start (a core, a server core or a frontend) opens a new file
//! named by its start time, so viewers can load only the current session and
//! list earlier ones. Only the newest [`KEEP`] files of a source are retained.
use crate::platform::OpenOptionsExt;
use std::{
    fs::{self, OpenOptions},
    io,
    path::{Path, PathBuf},
};

/// Session files retained per source, including the current one.
pub const KEEP: usize = 20;
const STAMP: &str = "%Y%m%dT%H%M%S%.3fZ";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// File stem, e.g. `20261110T073012.123Z`; the only id accepted by [`resolve`].
    pub id: String,
    pub started_ms: u64,
    pub size: u64,
    pub path: PathBuf,
}

/// Sources are instance names, `server-<name>`, `tui` or `gui`.
fn valid_source(source: &str) -> bool {
    !source.is_empty() && source != "." && source != ".." && !source.contains(['/', '\\', '\0'])
}

pub fn session_dir(home: &Path, source: &str) -> PathBuf {
    home.join("logs").join(source)
}

fn invalid(source: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("invalid log source: {source}"),
    )
}

fn create_private_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
        // An existing directory keeps its owner; tighten group/other access only.
        let mode = fs::metadata(path)?.permissions().mode();
        if mode & 0o077 != 0 {
            fs::set_permissions(path, fs::Permissions::from_mode(mode & !0o077))?;
        }
        Ok(())
    }
    #[cfg(not(unix))]
    fs::create_dir_all(path)
}

fn stamp(time: std::time::SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(time)
        .format(STAMP)
        .to_string()
}

/// Sort key: timestamp text, migrated legacy files before sessions of the same
/// instant, then a numeric clash suffix (`-1`, `-2`, …).
fn key(id: &str) -> Option<(String, bool, u32)> {
    let (base, rest) = id.split_at(id.find('Z')? + 1);
    chrono::NaiveDateTime::parse_from_str(base, STAMP).ok()?;
    let suffix = rest
        .rsplit('-')
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    Some((base.to_owned(), !rest.starts_with("-legacy"), suffix))
}

fn started_ms(id: &str) -> u64 {
    key(id)
        .and_then(|(base, _, _)| chrono::NaiveDateTime::parse_from_str(&base, STAMP).ok())
        .map(|time| time.and_utc().timestamp_millis().max(0) as u64)
        .unwrap_or(0)
}

/// All session files of a source, newest first.
pub fn list(home: &Path, source: &str) -> Vec<Session> {
    if !valid_source(source) {
        return vec![];
    }
    let directory = session_dir(home, source);
    // Logs written before sessions existed stay readable after an upgrade.
    if home.join(format!("{source}.log")).is_file() && create_private_dir(&directory).is_ok() {
        migrate_legacy(home, source, &directory);
    }
    let Ok(entries) = fs::read_dir(&directory) else {
        return vec![];
    };
    let mut sessions = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let id = path.file_name()?.to_str()?.strip_suffix(".log")?.to_owned();
            key(&id)?;
            let metadata = fs::symlink_metadata(&path).ok()?;
            metadata.is_file().then(|| Session {
                started_ms: started_ms(&id),
                size: metadata.len(),
                id,
                path,
            })
        })
        .collect::<Vec<_>>();
    sessions.sort_by_key(|session| std::cmp::Reverse(key(&session.id)));
    sessions
}

/// The newest session file, where writers append.
pub fn latest(home: &Path, source: &str) -> Option<PathBuf> {
    list(home, source).into_iter().next().map(|s| s.path)
}

/// The session viewers show by default: the newest one that has output, so a
/// process that has not written yet (or whose start was refused) does not hide
/// the previous session. The cursor resets onto the new file once it writes.
pub fn current(home: &Path, source: &str) -> Option<PathBuf> {
    let sessions = list(home, source);
    sessions
        .iter()
        .find(|session| session.size > 0)
        .or(sessions.first())
        .map(|session| session.path.clone())
}

/// A session id selected by a viewer; anything not listed is rejected.
pub fn resolve(home: &Path, source: &str, id: &str) -> Option<PathBuf> {
    list(home, source)
        .into_iter()
        .find(|session| session.id == id)
        .map(|session| session.path)
}

/// Start a new session file for a process about to launch.
pub fn new_session(home: &Path, source: &str) -> io::Result<PathBuf> {
    if !valid_source(source) {
        return Err(invalid(source));
    }
    let directory = session_dir(home, source);
    create_private_dir(&directory)?;
    migrate_legacy(home, source, &directory);
    let base = stamp(std::time::SystemTime::now());
    let mut attempt = 0u32;
    let path = loop {
        let name = if attempt == 0 {
            format!("{base}.log")
        } else {
            format!("{base}-{attempt}.log")
        };
        let path = directory.join(name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(_) => break path,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists && attempt < 1000 => {
                attempt += 1
            }
            Err(error) => return Err(error),
        }
    };
    prune(home, source, KEEP);
    Ok(path)
}

/// The newest session, starting one when the source has none yet.
pub fn latest_or_new(home: &Path, source: &str) -> io::Result<PathBuf> {
    match latest(home, source) {
        Some(path) => Ok(path),
        None => new_session(home, source),
    }
}

/// Remove a session that a refused launch never wrote to.
pub fn discard_if_empty(path: &Path) {
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file() && metadata.len() == 0) {
        let _ = fs::remove_file(path);
    }
}

/// Keep the newest `keep` sessions. The newest is never removed.
pub fn prune(home: &Path, source: &str, keep: usize) {
    for session in list(home, source).into_iter().skip(keep.max(1)) {
        if let Err(error) = fs::remove_file(&session.path) {
            if error.kind() != io::ErrorKind::NotFound {
                eprintln!(
                    "Old log session was not removed: {}: {error}",
                    session.path.display()
                );
            }
        }
    }
}

/// Move a pre-session `<home>/<source>.log` (and its `.1` rotation) into the
/// session directory, named after its modification time, so it stays listed.
fn migrate_legacy(home: &Path, source: &str, directory: &Path) {
    for name in [format!("{source}.log.1"), format!("{source}.log")] {
        let legacy = home.join(name);
        let Ok(metadata) = fs::symlink_metadata(&legacy) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let base = stamp(metadata.modified().unwrap_or(std::time::UNIX_EPOCH));
        let target = (0..1000)
            .map(|attempt| {
                directory.join(if attempt == 0 {
                    format!("{base}-legacy.log")
                } else {
                    format!("{base}-legacy-{attempt}.log")
                })
            })
            .find(|path| !path.exists());
        if let Some(target) = target {
            // A file another process still holds open may refuse to move on
            // Windows; it is then left in place and retried next session.
            let _ = fs::rename(&legacy, &target);
        }
    }
}

#[cfg(test)]
#[path = "test/log_sessions_test.rs"]
mod tests;
