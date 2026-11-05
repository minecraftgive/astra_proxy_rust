use super::*;
#[test]
fn replacements_are_atomic_private_and_clean_up_candidates() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.json");
    atomic_write(&path, b"old", WritePolicy::default()).unwrap();
    atomic_write(&path, b"new", WritePolicy::default()).unwrap();
    assert_eq!(read_bounded(&path, 3).unwrap(), b"new");
    assert!(read_bounded(&path, 2).is_err());
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    let invalid = directory.path().join("directory");
    std::fs::create_dir(&invalid).unwrap();
    assert!(atomic_write(&invalid, b"never committed", WritePolicy::default()).is_err());
    assert!(invalid.is_dir());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[cfg(unix)]
#[test]
fn replacement_rejects_a_symlink_without_touching_its_target() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("target");
    let link = directory.path().join("link");
    std::fs::write(&target, b"preserved").unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(atomic_write(&link, b"changed", WritePolicy::default()).is_err());
    assert_eq!(std::fs::read(target).unwrap(), b"preserved");
}

#[test]
fn profiles_with_same_basename_have_independent_runtime_files() {
    let a = runtime_path(Path::new("profiles/a/proxy.toml"), "subscriptions").unwrap();
    let b = runtime_path(Path::new("profiles/b/proxy.toml"), "subscriptions").unwrap();
    assert_ne!(a, b);
    assert_eq!(
        a,
        runtime_path(Path::new("profiles/a/proxy.toml"), "subscriptions").unwrap()
    );
    assert!(!a.starts_with(std::path::absolute("profiles").unwrap()));
}
