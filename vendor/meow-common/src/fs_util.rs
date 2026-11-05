//! Shared cleanup for the `{name}.{pid}.{n}.tmp` scratch siblings that
//! atomic write-then-rename producers (`meow_config::unique_scratch_path`,
//! `SelectorStore`, fake-IP `FileStore`) leave behind when the process
//! dies between create and rename (issue #621).
//!
//! Ordinary failures remove their own scratch; only a crash or SIGKILL can
//! orphan one — which is why cleanup is a best-effort sweep at the next
//! open/write rather than a guarantee.

use std::path::Path;
use std::time::{Duration, SystemTime};

/// Default staleness bound for scratch files: a live write+rename
/// completes in milliseconds, so anything an hour old is a leftover.
pub const SCRATCH_STALE_AGE: Duration = Duration::from_secs(3600);

/// Minimum age before the dead-pid arm may remove a scratch. A live
/// writer holds a scratch for milliseconds (and a streaming download
/// keeps mtime fresh throughout), so anything older than this whose
/// owner pid is dead is a crash leftover — while a *live* writer in a
/// different pid namespace (container sharing a mounted dir) still gets
/// a grace window instead of losing an in-flight file (issue #621
/// review). Coarse mtimes and namespaces are a heuristic either way;
/// the worst case of a false positive is one failed rename that
/// self-heals on the next write.
const DEAD_PID_MIN_AGE: Duration = Duration::from_secs(1);

/// Best-effort removal of `"{target}.tmp"` (the legacy shared scratch
/// name) and `"{target}.{pid}.{n}.tmp"` siblings left behind by crashed
/// writers. Returns the number of files removed.
///
/// A sibling is removed when either:
///
/// - its mtime is older than `max_age` — a live writer's scratch exists
///   for milliseconds, so an old file cannot still be in flight; or
/// - (unix only) it is at least `DEAD_PID_MIN_AGE` (1s) old and the pid
///   embedded in its name is not this process and belongs to a dead
///   process — the common restart-after-crash case.
///
/// Anything failing both checks is kept: a young scratch may belong to a
/// live writer mid-flight. Names that don't match `{base}.tmp` or
/// `{base}.{digits}.{digits}.tmp` are never touched (note: a *foreign*
/// file matching the exact shape is still removed when stale — that's
/// the intended sweep of a dead sibling's leftovers), and every error is
/// ignored — sweeping is strictly best-effort.
pub fn sweep_scratch_siblings(target: &Path, max_age: Duration) -> usize {
    // `Path::new("file").parent()` is `Some("")` — a bare filename means
    // the current dir, and `read_dir("")` would fail outright.
    let dir = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let Some(base) = target.file_name().and_then(|n| n.to_str()) else {
        return 0;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let now = SystemTime::now();
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(rest) = name.strip_prefix(base) else {
            continue;
        };
        let pid = match rest.strip_prefix('.') {
            // Legacy shared-scratch name `{base}.tmp` — no pid embedded;
            // age is the only staleness signal.
            Some("tmp") => None,
            Some(s) => match s.strip_suffix(".tmp").and_then(|t| t.split_once('.')) {
                // Parse the pid as u64 so names carrying a pid beyond
                // the u32 range still parse — they can't be probed but
                // remain age-sweepable (issue #621 review).
                Some((pid, n)) => match (pid.parse::<u64>(), n.parse::<u64>()) {
                    (Ok(pid), Ok(_)) => Some(pid),
                    _ => continue,
                },
                None => continue,
            },
            None => continue,
        };
        let stale_by_age = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|mtime| now.duration_since(mtime).unwrap_or_default() > max_age);
        // The dead-pid arm also requires the age floor — see
        // DEAD_PID_MIN_AGE.
        let dead_owner = pid.is_some_and(creator_dead)
            && entry
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|mtime| {
                    now.duration_since(mtime).unwrap_or_default() > DEAD_PID_MIN_AGE
                });
        if (stale_by_age || dead_owner) && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Whether the pid embedded in a scratch name is definitely dead — i.e.
/// the file can no longer be in flight under its creator. Non-unix
/// platforms have no portable probe, so only age applies there.
///
/// `pid == 0` stays "alive" incidentally: `kill(0, 0)` probes our own
/// process *group* and succeeds — the safe direction. Pids beyond
/// `i32::MAX` can't be probed without a negative-pid group wrap, so they
/// are kept until age makes them stale.
#[cfg(unix)]
fn creator_dead(pid: u64) -> bool {
    if pid == u64::from(std::process::id()) {
        return false;
    }
    // `kill(pid, 0)` probes existence without signalling: ESRCH = dead.
    // EPERM means the process exists but we may not signal it — alive.
    // `try_from` rejects pids > i32::MAX — a raw `as i32` wrap would turn
    // them negative and probe a process *group* instead.
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    let rc = unsafe { libc::kill(pid, 0) };
    rc != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

#[cfg(not(unix))]
fn creator_dead(_pid: u64) -> bool {
    false
}

#[cfg(test)]
#[path = "test/fs_util_test.rs"]
mod tests;
