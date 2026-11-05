use super::*;
use std::io::Write;
use std::time::Duration;

fn write(path: &Path) {
    std::fs::File::create(path)
        .unwrap()
        .write_all(b"x")
        .unwrap();
}

#[test]
fn sweep_removes_old_scratch_keeps_target_and_foreign() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("cache.json");
    write(&target);
    let scratch = dir.path().join("cache.json.1.42.tmp");
    write(&scratch);
    write(&dir.path().join("cache.json.bak")); // not a scratch name
    write(&dir.path().join("cache.json.tmp.x")); // wrong suffix
    write(&dir.path().join("other.1.2.tmp")); // different base

    // Fresh scratch is kept — could be in flight.
    assert_eq!(sweep_scratch_siblings(&target, Duration::from_secs(1)), 0);
    assert!(scratch.exists());

    // With a zero bound every matching scratch is stale.
    let removed = sweep_scratch_siblings(&target, Duration::ZERO);
    assert_eq!(removed, 1);
    assert!(!scratch.exists());
    assert!(target.exists());
    assert!(dir.path().join("cache.json.bak").exists());
    assert!(dir.path().join("other.1.2.tmp").exists());
}

#[test]
fn sweep_removes_legacy_shared_tmp_name() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("live.yaml");
    write(&dir.path().join("live.yaml.tmp"));
    assert_eq!(sweep_scratch_siblings(&target, Duration::ZERO), 1);
    assert!(!dir.path().join("live.yaml.tmp").exists());
}

#[test]
fn young_legacy_tmp_and_single_component_names_are_kept() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("live.yaml");
    // Young legacy scratch: could still be in flight — kept.
    write(&dir.path().join("live.yaml.tmp"));
    // `{base}.{n}.tmp` (single component, no pid): not our scheme.
    write(&dir.path().join("live.yaml.42.tmp"));
    // A pid > i32::MAX can't be probed by kill() at all — kept; age
    // is the only staleness signal for it (see creator_dead).
    write(&dir.path().join("live.yaml.3000000000.5.tmp"));
    // A pid beyond the u32 range can't be probed either — but the
    // name still parses (u64), so age still sweeps it.
    let huge = dir.path().join("live.yaml.9999999999999.5.tmp");
    write(&huge);
    assert_eq!(
        sweep_scratch_siblings(&target, Duration::from_secs(3600)),
        0
    );
    assert!(dir.path().join("live.yaml.tmp").exists());
    assert!(dir.path().join("live.yaml.42.tmp").exists());
    assert!(dir.path().join("live.yaml.3000000000.5.tmp").exists());
    assert!(huge.exists());
    // …and once old, all scratch names are swept regardless of how
    // unprobeable their embedded pid is.
    std::fs::File::options()
        .write(true)
        .open(&huge)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(7200))
        .unwrap();
    assert_eq!(
        sweep_scratch_siblings(&target, Duration::from_secs(3600)),
        1
    );
    assert!(!huge.exists());
}

#[cfg(unix)]
#[test]
fn dead_pid_scratch_is_removed_once_past_the_floor() {
    // A pid that cannot exist: `i32::MAX` exceeds every platform's
    // pid ceiling (linux caps at 2^22), so `kill` fails ESRCH.
    // (`u32::MAX` would wrap to -1 — signal-everything, which succeeds.)
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("sel.json");
    let scratch = dir.path().join(format!("sel.json.{}.7.tmp", i32::MAX));
    write(&scratch);

    // Below DEAD_PID_MIN_AGE the dead-pid arm must not fire: a live
    // writer in a foreign pid namespace could still be in flight.
    assert_eq!(sweep_scratch_siblings(&target, Duration::MAX), 0);
    assert!(scratch.exists());

    // Age the file past the floor — a restart-after-crash leftover.
    std::fs::File::options()
        .write(true)
        .open(&scratch)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(5))
        .unwrap();
    assert_eq!(sweep_scratch_siblings(&target, Duration::MAX), 1);
    assert!(!scratch.exists());
}

#[cfg(unix)]
#[test]
fn live_pid_scratch_is_kept_when_young() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("sel.json");
    let scratch = dir
        .path()
        .join(format!("sel.json.{}.7.tmp", std::process::id()));
    write(&scratch);
    assert_eq!(
        sweep_scratch_siblings(&target, Duration::from_secs(3600)),
        0
    );
    assert!(scratch.exists());
}
