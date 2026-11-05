//! Persistent backing for `SelectorGroup` choices.
//!
//! Upstream Go mihomo stores user-picked selector targets in a BoltDB
//! `cache.db` (`adapter/outboundgroup/selector.go` → `cachefile.Cache().SetSelected`).
//! We keep the same semantics with a simpler JSON file written through on
//! every successful `select()`, so the user's pick survives process restart.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::warn;

static GLOBAL_STORE: std::sync::OnceLock<Arc<SelectorStore>> = std::sync::OnceLock::new();

/// Map of `{group_name → selected_proxy_name}` persisted to a JSON file.
pub struct SelectorStore {
    path: PathBuf,
    map: Mutex<HashMap<String, String>>,
    /// Serializes the disk half of `set()` — see its comment (issue #621).
    write_lock: Mutex<()>,
    /// A past `write_atomic` failed — the file is behind the map. The
    /// early-return in `set()` must not skip the retry when the value is
    /// unchanged (issue #621 review).
    dirty: std::sync::atomic::AtomicBool,
}

impl SelectorStore {
    /// Open a store at `path`. Missing / unreadable / malformed files are
    /// treated as empty (with a warn), so a fresh install just starts blank.
    pub fn open(path: PathBuf) -> Arc<Self> {
        // Remove scratch siblings left by a crash between create and
        // rename (issue #621) — best-effort, before the load.
        meow_common::fs_util::sweep_scratch_siblings(
            &path,
            meow_common::fs_util::SCRATCH_STALE_AGE,
        );
        let map = match std::fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice::<HashMap<String, String>>(&bytes).unwrap_or_else(|e| {
                    warn!(path = %path.display(), error = %e,
                        "selector store: malformed JSON, starting empty");
                    HashMap::new()
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => {
                warn!(path = %path.display(), error = %e,
                    "selector store: read failed, starting empty");
                HashMap::new()
            }
        };
        let store = Arc::new(Self {
            path,
            map: Mutex::new(map),
            write_lock: Mutex::new(()),
            dirty: std::sync::atomic::AtomicBool::new(false),
        });
        let _ = GLOBAL_STORE.set(Arc::clone(&store));
        store
    }

    /// Process-wide store used by runtime config rebuilds so rebuilt groups
    /// retain the same persistence side-channel as startup-created groups.
    ///
    /// Returns `None` until the first call to [`Self::open`]. The first
    /// `open()` call wins the process-global slot; subsequent calls still
    /// return their own `Arc<Self>` but do **not** replace the global.
    /// Tests or library users that create multiple stores should be aware
    /// that only the first store is reachable via `global()`.
    pub fn global() -> Option<Arc<Self>> {
        GLOBAL_STORE.get().cloned()
    }

    pub fn get(&self, group: &str) -> Option<String> {
        self.map.lock().get(group).cloned()
    }

    /// Update the in-memory map and flush the whole file atomically. IO
    /// errors only warn — losing the persistence side-channel must never
    /// fail the user's selection.
    pub fn set(&self, group: &str, selected: &str) {
        use std::sync::atomic::Ordering::Relaxed;
        {
            let mut g = self.map.lock();
            if g.get(group).is_some_and(|v| v == selected) && !self.dirty.load(Relaxed) {
                return;
            }
            g.insert(group.to_string(), selected.to_string());
            self.dirty.store(true, Relaxed);
        }
        // Serialize writers and take the snapshot *inside* the write lock:
        // the rename that lands last then always carries the newest
        // committed map — without this, a slow writer holding an older
        // snapshot could publish it after a newer writer, leaving the
        // persisted choice behind the last in-memory one (issue #621).
        let _write = self.write_lock.lock();
        let snapshot = self.map.lock().clone();
        match write_atomic(&self.path, &snapshot) {
            Ok(()) => self.dirty.store(false, Relaxed),
            // Re-assert dirty on failure *inside* the write lock: a
            // concurrent `set` may have committed a newer map + flag while
            // we wrote, and a successful sibling's `store(false)` can land
            // between that commit and this failed write — without the
            // re-store, a same-value retry would early-return forever and
            // the disk would stay stale (issue #621 review).
            Err(e) => {
                self.dirty.store(true, Relaxed);
                warn!(path = %self.path.display(), error = %e,
                    "selector store: persist failed");
            }
        }
    }
}

fn write_atomic(path: &Path, map: &HashMap<String, String>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let json = serde_json::to_vec_pretty(map)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    // Unique scratch per call — concurrent `set()`s (API handlers plus
    // url-test/fallback auto-switch, none laned) must not share a fixed
    // tmp or one writer's rename publishes the other's splice (issue #543
    // review).
    let tmp = scratch_path(path);
    let result = std::fs::write(&tmp, json).and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn scratch_path(path: &Path) -> PathBuf {
    // Same unique-suffix scheme as `meow-config::unique_scratch_path`;
    // `AtomicU` resolves to AtomicU32 where 64-bit atomics are missing.
    static COUNTER: meow_common::atomic::AtomicU = meow_common::atomic::AtomicU::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".{}.{}.tmp", std::process::id(), n));
    PathBuf::from(name)
}

#[cfg(test)]
#[path = "../test/group_selector_store_test.rs"]
mod tests;
