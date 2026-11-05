use super::*;
#[test]
fn reading_status_never_recovers_or_creates_files() {
    let directory = tempfile::tempdir().unwrap();
    let status = directory.path().join("core.recovery.json");
    assert!(read(&status).unwrap().is_none());
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    let notice = Notice {
        state: RecoveryState::Pending,
        timestamp_ms: 1,
        message: "A route requires manual inspection".into(),
        journal: directory.path().join("journal"),
        boot_id: "boot".into(),
    };
    write(&status, &notice).unwrap();
    assert_eq!(
        read(&status).unwrap().unwrap().state,
        RecoveryState::Pending
    );
    assert!(!notice.journal.exists());
}
