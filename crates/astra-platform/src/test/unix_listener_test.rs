use super::*;
#[tokio::test]
async fn forced_recovery_preserves_live_sockets_non_sockets_and_shared_parents() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("ipc");
    let listener = LocalListener::bind(&path).unwrap();
    assert!(force_recover(&path).is_err());
    assert!(path.exists());
    std::fs::remove_file(&path).unwrap();
    assert!(force_recover(&path).is_err());
    drop(listener);
    assert!(!force_recover(&path).unwrap());
    std::fs::write(&path, b"keep").unwrap();
    assert!(force_recover(&path).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"keep");
    std::fs::remove_file(&path).unwrap();
    let target = directory.path().join("target");
    std::fs::write(&target, b"keep").unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();
    assert!(force_recover(&path).is_err());
    assert_eq!(std::fs::read(&target).unwrap(), b"keep");
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(directory.path().join("missing"), &path).unwrap();
    assert!(force_recover(&path).is_err());
    assert!(std::fs::symlink_metadata(&path)
        .unwrap()
        .file_type()
        .is_symlink());
    std::fs::remove_file(&path).unwrap();
    drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(force_recover(&path).is_err());
    assert!(path.exists());
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(force_recover(&path).unwrap());
    assert!(!path.exists());
}
#[tokio::test]
async fn live_socket_and_non_socket_are_preserved_but_abandoned_socket_recovers() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("ipc");
    let listener = LocalListener::bind(&path).unwrap();
    assert!(recover(&path).is_err());
    assert!(LocalListener::bind(&path).is_err());
    drop(listener);
    assert!(!path.exists());
    let abandoned = std::os::unix::net::UnixListener::bind(&path).unwrap();
    drop(abandoned);
    assert!(recover(&path).unwrap());
    std::fs::write(&path, b"keep").unwrap();
    assert!(LocalListener::bind(&path).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"keep");
}
#[tokio::test]
async fn listener_drop_preserves_replaced_file() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("ipc");
    let listener = LocalListener::bind(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, b"replacement").unwrap();
    drop(listener);
    assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
}
