use super::*;

#[test]
fn runtime_root_respects_override_without_a_home_directory() {
    const CASE: &str = "ASTRA_TEST_RUNTIME_ROOT_CASE";
    let home = std::env::temp_dir().join("astra home 中文");
    let runtime = std::env::temp_dir().join("astra runtime 中文");
    if let Ok(case) = std::env::var(CASE) {
        let expected = match case.as_str() {
            "override-only" | "override-and-home" => runtime,
            "home-only" | "profile-only" => home.join(".astra/runtime"),
            "neither" => {
                #[cfg(windows)]
                {
                    crate::platform::home_dir().unwrap().join(".astra/runtime")
                }
                #[cfg(not(windows))]
                {
                    assert_eq!(runtime_root().unwrap_err().kind(), io::ErrorKind::NotFound);
                    return;
                }
            }
            _ => panic!("unknown runtime root test case: {case}"),
        };
        assert_eq!(runtime_root().unwrap(), expected);
        let source = Path::new("profiles/proxy.toml");
        assert_eq!(
            runtime_path(source, "overrides").unwrap(),
            runtime_path_in(&expected, source, "overrides").unwrap()
        );
        return;
    }

    // Isolate environment changes from other tests in this process.
    for case in [
        "override-only",
        "override-and-home",
        "home-only",
        "neither",
        #[cfg(windows)]
        "profile-only",
    ] {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        crate::platform::background_command(&mut command, false);
        command
            .args([
                "--exact",
                "storage::tests::runtime_root_respects_override_without_a_home_directory",
                "--nocapture",
            ])
            .env(CASE, case);
        for key in ["ASTRA_RUNTIME_DIR", "HOME", "LOCALAPPDATA", "USERPROFILE"] {
            command.env_remove(key);
        }
        if matches!(case, "override-only" | "override-and-home") {
            command.env("ASTRA_RUNTIME_DIR", &runtime);
        }
        if matches!(case, "home-only" | "override-and-home") {
            command.env(
                if cfg!(windows) {
                    "LOCALAPPDATA"
                } else {
                    "HOME"
                },
                &home,
            );
        }
        if case == "profile-only" {
            command.env("USERPROFILE", &home);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{case}:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

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
