use super::*;

fn touch(dir: &Path, id: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(format!("{id}.log"));
    fs::write(&path, bytes).unwrap();
    path
}

#[test]
fn new_sessions_are_timestamped_sorted_newest_first_and_never_collide() {
    let home = tempfile::tempdir().unwrap();
    let first = new_session(home.path(), "proxy").unwrap();
    let second = new_session(home.path(), "proxy").unwrap();
    assert_ne!(first, second);
    assert_eq!(
        first.parent(),
        Some(session_dir(home.path(), "proxy").as_path())
    );
    let id = first.file_stem().unwrap().to_str().unwrap();
    assert!(key(id).is_some(), "{id}");
    let sessions = list(home.path(), "proxy");
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[0].path, second);
    assert!(sessions[0].started_ms >= sessions[1].started_ms);
    assert!(sessions[0].started_ms > 1_700_000_000_000);
    assert_eq!(latest(home.path(), "proxy"), Some(second));
}

#[test]
fn clash_suffixes_sort_after_their_base_and_unrelated_files_are_ignored() {
    let home = tempfile::tempdir().unwrap();
    let dir = session_dir(home.path(), "core");
    fs::create_dir_all(&dir).unwrap();
    touch(&dir, "20260101T000000.000Z", b"a");
    let clash = touch(&dir, "20260101T000000.000Z-1", b"b");
    touch(&dir, "notes", b"x");
    fs::write(dir.join("20260101T000000.000Z.txt"), "x").unwrap();
    let sessions = list(home.path(), "core");
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[0].path, clash);
}

#[test]
fn current_prefers_the_newest_session_with_output() {
    let home = tempfile::tempdir().unwrap();
    let dir = session_dir(home.path(), "core");
    fs::create_dir_all(&dir).unwrap();
    let written = touch(&dir, "20260101T000000.000Z", b"old core output\n");
    let empty = touch(&dir, "20260102T000000.000Z", b"");
    assert_eq!(latest(home.path(), "core"), Some(empty.clone()));
    assert_eq!(current(home.path(), "core"), Some(written));
    fs::write(&empty, "new core output\n").unwrap();
    assert_eq!(current(home.path(), "core"), Some(empty.clone()));
    discard_if_empty(&empty);
    assert!(empty.exists());
}

#[test]
fn prune_keeps_the_newest_sessions() {
    let home = tempfile::tempdir().unwrap();
    let dir = session_dir(home.path(), "core");
    fs::create_dir_all(&dir).unwrap();
    for day in 1..=25 {
        touch(&dir, &format!("202601{day:02}T000000.000Z"), b"x");
    }
    prune(home.path(), "core", KEEP);
    let sessions = list(home.path(), "core");
    assert_eq!(sessions.len(), KEEP);
    assert_eq!(sessions[0].id, "20260125T000000.000Z");
    assert_eq!(sessions.last().unwrap().id, "20260106T000000.000Z");
    // A new session also prunes, still keeping KEEP files.
    new_session(home.path(), "core").unwrap();
    assert_eq!(list(home.path(), "core").len(), KEEP);
}

#[test]
fn resolve_accepts_only_listed_ids_and_sources_cannot_traverse() {
    let home = tempfile::tempdir().unwrap();
    let path = new_session(home.path(), "proxy").unwrap();
    let id = path.file_stem().unwrap().to_str().unwrap().to_owned();
    assert_eq!(resolve(home.path(), "proxy", &id), Some(path));
    for bad in ["../proxy", "..", "", "x/../../etc"] {
        assert_eq!(resolve(home.path(), "proxy", bad), None, "{bad}");
    }
    for bad in ["..", "a/b", "a\\b", ""] {
        assert!(new_session(home.path(), bad).is_err(), "{bad}");
        assert!(list(home.path(), bad).is_empty());
    }
}

#[test]
fn legacy_logs_move_into_the_session_list() {
    let home = tempfile::tempdir().unwrap();
    fs::write(home.path().join("proxy.log"), "legacy current\n").unwrap();
    fs::write(home.path().join("proxy.log.1"), "legacy rotated\n").unwrap();
    let fresh = new_session(home.path(), "proxy").unwrap();
    assert!(!home.path().join("proxy.log").exists());
    assert!(!home.path().join("proxy.log.1").exists());
    let sessions = list(home.path(), "proxy");
    assert_eq!(sessions.len(), 3);
    assert_eq!(sessions[0].path, fresh);
    assert!(sessions[1..].iter().all(|s| s.id.contains("-legacy")));
    // The fresh session is empty, so viewers keep showing legacy output.
    let text = fs::read_to_string(current(home.path(), "proxy").unwrap()).unwrap();
    assert!(text.starts_with("legacy"));
}

#[cfg(unix)]
#[test]
fn session_directories_and_files_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let path = new_session(home.path(), "proxy").unwrap();
    let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(path.parent().unwrap()), 0o700);
    assert_eq!(mode(&path), 0o600);
}
