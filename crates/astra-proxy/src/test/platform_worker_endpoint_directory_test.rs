use super::*;
struct Directory(std::path::PathBuf);
impl Directory {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("astra-endpoint-dir-{}", uuid::Uuid::new_v4()));
        #[allow(unused_mut)]
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn directory_validation_requires_an_existing_real_directory() {
    let directory = Directory::new();
    assert!(validate_endpoint_directory(&directory.0.join("missing")).is_err());
    let file = directory.0.join("file");
    std::fs::write(&file, b"not a directory").unwrap();
    assert!(validate_endpoint_directory(&file).is_err());
    #[cfg(windows)]
    validate_endpoint_directory(&directory.0).unwrap();
    #[cfg(unix)]
    {
        let uid = unsafe { libc::geteuid() };
        validate_unix_directory(&directory.0, uid).unwrap();
        if uid == 0 {
            // Root-owned state cannot impersonate an ordinary invoking user.
            assert!(validate_endpoint_directory(&directory.0).is_err());
        } else {
            validate_endpoint_directory(&directory.0).unwrap();
        }
    }
}

#[cfg(unix)]
#[test]
fn rejects_shared_permissions_wrong_owner_and_links_without_chmod() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let directory = Directory::new();
    let uid = unsafe { libc::geteuid() };
    assert!(validate_unix_directory(&directory.0, uid.wrapping_add(1)).is_err());
    let link = directory.0.join("alias");
    std::os::unix::fs::symlink(&directory.0, &link).unwrap();
    assert!(validate_endpoint_directory(&link).is_err());
    std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(validate_unix_directory(&directory.0, uid).is_err());
    assert_eq!(
        std::fs::metadata(&directory.0).unwrap().mode() & 0o7777,
        0o755
    );
}

#[cfg(unix)]
#[test]
fn a_private_directory_cannot_be_replaced_through_a_shared_parent() {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let directory = Directory::new();
    let shared = directory.0.join("shared");
    std::fs::create_dir(&shared).unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
    let private = shared.join("private");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&private)
        .unwrap();
    let uid = unsafe { libc::geteuid() };
    assert!(validate_unix_directory(&private, uid).is_err());
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o1777)).unwrap();
    validate_unix_directory(&private, uid).unwrap();
    assert!(validate_unix_directory(&shared, uid).is_err());
}
