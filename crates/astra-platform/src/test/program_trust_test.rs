use super::*;

#[test]
fn warning_is_appended_as_a_single_warn_record_without_a_subscriber() {
    let directory = tempfile::tempdir().unwrap();
    let log = directory.path().join("core.log");
    std::fs::write(&log, "existing log\n").unwrap();
    append_warning(&log, "untrusted path\ncontinuing\r\u{1b}");
    let text = std::fs::read_to_string(log).unwrap();
    assert!(text.starts_with("existing log\n"));
    assert!(text.contains(" WARN installation: untrusted pathcontinuing\n"));
    assert_eq!(text.lines().count(), 2);
}

#[test]
fn log_failure_is_best_effort_and_does_not_create_directories() {
    let directory = tempfile::tempdir().unwrap();
    append_warning(&directory.path().join("missing/core.log"), "continuing");
    append_warning(directory.path(), "continuing");
    assert!(!directory.path().join("missing").exists());
}

#[test]
fn hard_linked_log_is_not_modified() {
    let directory = tempfile::tempdir().unwrap();
    let original = directory.path().join("original");
    let log = directory.path().join("core.log");
    std::fs::write(&original, "keep").unwrap();
    std::fs::hard_link(&original, &log).unwrap();
    append_warning(&log, "continuing");
    assert_eq!(std::fs::read_to_string(original).unwrap(), "keep");
}

#[cfg(unix)]
#[test]
fn symlinked_log_is_not_modified() {
    let directory = tempfile::tempdir().unwrap();
    let original = directory.path().join("original");
    let log = directory.path().join("core.log");
    std::fs::write(&original, "keep").unwrap();
    std::os::unix::fs::symlink(&original, &log).unwrap();
    append_warning(&log, "continuing");
    assert_eq!(std::fs::read_to_string(original).unwrap(), "keep");
}
