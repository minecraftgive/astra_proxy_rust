use super::*;

fn directory(owner: u32, group: u32, mode: u32, destination: bool) -> Result<()> {
    validate_directory_permissions(
        Path::new("configuration"),
        destination,
        DirectoryPermissions { owner, group, mode },
        1000,
        1000,
    )
}

#[test]
fn inherited_user_directories_accept_the_primary_group_write_convention() {
    for mode in [0o700, 0o755, 0o775, 0o2775] {
        for destination in [false, true] {
            assert!(directory(1000, 1000, mode, destination).is_ok());
        }
    }
    // A non-writable group does not need to be the user's primary group.
    assert!(directory(1000, 2000, 0o755, true).is_ok());
    assert!(directory(0, 0, 0o755, false).is_ok());
}

#[test]
fn unsupported_ownership_world_write_and_foreign_group_write_remain_rejected() {
    assert!(directory(2000, 1000, 0o755, false)
        .unwrap_err()
        .to_string()
        .contains("owned by another user"));
    assert!(directory(0, 0, 0o755, true)
        .unwrap_err()
        .to_string()
        .contains("not owned by this user"));
    for destination in [false, true] {
        assert!(directory(1000, 1000, 0o777, destination)
            .unwrap_err()
            .to_string()
            .contains("writable by everyone"));
        assert!(directory(1000, 2000, 0o775, destination)
            .unwrap_err()
            .to_string()
            .contains("unsupported group-write permissions"));
    }
    assert!(directory(0, 1000, 0o775, false).is_err());
}

#[test]
fn only_trusted_sticky_ancestors_receive_the_temporary_directory_exception() {
    assert!(directory(0, 0, 0o1777, false).is_ok());
    assert!(directory(1000, 1000, 0o1777, false).is_ok());
    assert!(directory(2000, 2000, 0o1777, false).is_err());
    assert!(directory(1000, 1000, 0o1777, true).is_err());
}

#[cfg(unix)]
mod filesystem {
    use super::*;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    struct Temporary(PathBuf);
    impl Temporary {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "astra-startup-permissions-{}-{}-{}",
                std::process::id(),
                astra_ipc::progress::now_ms(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&path)
                .unwrap();
            Self(std::fs::canonicalize(path).unwrap())
        }
    }
    impl Drop for Temporary {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn mode(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn inherited_775_directories_are_preserved_while_new_artifacts_stay_private() {
        let temporary = Temporary::new();
        let config = temporary.0.join("config");
        let systemd = config.join("systemd");
        let user = systemd.join("user");
        std::fs::create_dir_all(&user).unwrap();
        for directory in [&config, &systemd, &user] {
            mode(directory, 0o775);
        }
        let file = user.join("test.service");
        write_managed(&file, b"# managed\n", 0o600).unwrap();
        assert!(managed_file(&file, "# managed\n").unwrap());
        for directory in [&config, &systemd, &user] {
            assert_eq!(
                std::fs::metadata(directory).unwrap().permissions().mode() & 0o777,
                0o775
            );
        }
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        mode(&file, 0o660);
        assert!(managed_file(&file, "# managed\n").is_err());
        let private = config.join("new/private");
        check_directories(&private, true).unwrap();
        assert_eq!(
            std::fs::metadata(private).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn an_absent_artifact_does_not_require_a_directory_write_policy_decision() {
        let temporary = Temporary::new();
        let directory = temporary.0.join("inherited");
        std::fs::create_dir(&directory).unwrap();
        mode(&directory, 0o777);
        let file = directory.join("missing.service");
        assert!(!managed_file(&file, "# managed\n").unwrap());
        std::fs::write(&file, b"# managed\n").unwrap();
        mode(&file, 0o600);
        assert!(managed_file(&file, "# managed\n")
            .unwrap_err()
            .to_string()
            .contains("writable by everyone"));
    }

    #[test]
    fn existing_artifacts_under_substituted_directories_are_not_read() {
        let temporary = Temporary::new();
        let real = temporary.0.join("real");
        let linked = temporary.0.join("linked");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("existing.service"), b"not the managed prefix").unwrap();
        std::os::unix::fs::symlink(&real, &linked).unwrap();
        let error = managed_file(&linked.join("existing.service"), "# managed\n")
            .unwrap_err()
            .to_string();
        assert!(error.contains("not a real directory"), "{error}");
        assert!(!managed_file(&linked.join("missing.service"), "# managed\n").unwrap());
        std::os::unix::fs::symlink(real.join("missing"), real.join("dangling.service")).unwrap();
        assert!(managed_file(&real.join("dangling.service"), "# managed\n").is_err());
    }
}
