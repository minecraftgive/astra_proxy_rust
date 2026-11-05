#[tokio::test]
async fn forced_recovery_without_marker_preserves_live_pipe() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("control.sock");
    let listener = server(&pipe_name(&path).unwrap(), true).unwrap();
    assert!(!path.exists());
    assert!(super::force_recover_control_endpoint(&path).is_err());
    drop(listener);
    assert!(!super::force_recover_control_endpoint(&path).unwrap());
}
