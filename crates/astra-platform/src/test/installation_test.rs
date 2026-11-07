use super::*;

fn paths(directory: &Path) -> InstallPaths {
    InstallPaths {
        scope: InstallScope::User,
        role: "client".into(),
        root: directory.join("installed/client"),
        entry: directory.join(if cfg!(windows) {
            "commands/Astra Client.lnk"
        } else {
            "commands/astra-client"
        }),
    }
}

#[test]
fn entry_probe_is_read_only_and_unrelated_entries_are_preserved() {
    let directory = tempfile::tempdir().unwrap();
    let paths = paths(directory.path());
    check_entry(&paths).unwrap();
    assert!(!paths.entry.parent().unwrap().exists());
    fs::create_dir(paths.entry.parent().unwrap()).unwrap();
    fs::write(&paths.entry, b"another application").unwrap();
    assert!(publish_entry(&paths, &directory.path().join("astra-tui")).is_err());
    assert_eq!(fs::read(&paths.entry).unwrap(), b"another application");
}

#[test]
fn managed_entries_can_be_replaced_without_changing_the_payload() {
    let directory = tempfile::tempdir().unwrap();
    let paths = paths(directory.path());
    let first = directory.path().join("Chinese 中文 space ' old.exe");
    let second = directory.path().join("Chinese 中文 space ' new.exe");
    fs::write(&first, "old").unwrap();
    fs::write(&second, "new").unwrap();
    publish_entry(&paths, &first).unwrap();
    assert!(entry_matches(&paths, &first).unwrap());
    publish_entry(&paths, &second).unwrap();
    assert!(entry_matches(&paths, &second).unwrap());
    assert!(!entry_matches(&paths, &first).unwrap());
    assert_eq!(fs::read_to_string(first).unwrap(), "old");
}

#[test]
fn source_open_refuses_hard_links() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("program");
    fs::write(&path, "payload").unwrap();
    open_package_file(&path).unwrap();
    fs::hard_link(&path, directory.path().join("alias")).unwrap();
    assert!(open_package_file(&path).is_err());
}

#[cfg(unix)]
#[test]
fn wrapper_forwards_arguments_without_shell_expansion() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let paths = paths(directory.path());
    let executable = directory.path().join("program with ' quote");
    fs::write(&executable, "#!/bin/sh\nprintf '%s\\n' \"$@\"\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    publish_entry(&paths, &executable).unwrap();
    let output = std::process::Command::new(&paths.entry)
        .args(["space value", "$(touch unexpected)", "quote'value"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "space value\n$(touch unexpected)\nquote'value\n"
    );
    assert!(!directory.path().join("unexpected").exists());
}

#[cfg(unix)]
#[test]
fn user_links_and_writable_system_ancestors_are_rejected() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let directory = tempfile::tempdir().unwrap();
    let outside = directory.path().join("outside");
    fs::create_dir(&outside).unwrap();
    let link = directory.path().join("link");
    symlink(&outside, &link).unwrap();
    assert!(prepare_directory(&link.join("child"), InstallScope::User).is_err());
    assert!(!outside.join("child").exists());
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o777)).unwrap();
    assert!(validate_destination(&directory.path().join("system"), InstallScope::System).is_err());
}

#[cfg(unix)]
#[test]
fn desktop_entries_keep_gui_execution_separate_from_the_shell() {
    let directory = tempfile::tempdir().unwrap();
    let mut paths = paths(directory.path());
    paths.entry = directory.path().join("applications/astra.desktop");
    let executable = directory.path().join("GUI space $d `x` %F \"quoted\"");
    publish_entry(&paths, &executable).unwrap();
    assert!(entry_matches(&paths, &executable).unwrap());
    let entry = fs::read_to_string(&paths.entry).unwrap();
    assert!(entry.starts_with("[Desktop Entry]\n# Astra managed installation v1: client\n"));
    assert!(entry.contains("Terminal=false"));
    assert!(entry.contains("%%F"));
    assert!(!entry.contains("sh -c"));
    assert!(paths.root.join("icon.svg").is_file());
    fs::write(paths.root.join("icon.svg"), "damaged").unwrap();
    assert!(!entry_matches(&paths, &executable).unwrap());
    publish_entry(&paths, &executable).unwrap();
    assert!(entry_matches(&paths, &executable).unwrap());
}
#[cfg(target_os = "macos")]
#[test]
fn native_app_collision_is_preserved_and_managed_bundle_is_repairable() {
    let directory = tempfile::tempdir().unwrap();
    let mut paths = paths(directory.path());
    paths.entry = directory
        .path()
        .join("Applications/Astra.app/Contents/MacOS/astra-launch");
    let contents = paths.entry.parent().unwrap().parent().unwrap();
    fs::create_dir_all(contents).unwrap();
    fs::write(contents.join("Info.plist"), "another app").unwrap();
    assert!(publish_entry(&paths, &directory.path().join("astra-gui")).is_err());
    assert_eq!(
        fs::read_to_string(contents.join("Info.plist")).unwrap(),
        "another app"
    );
    fs::remove_dir_all(contents.parent().unwrap()).unwrap();
    publish_entry(&paths, &directory.path().join("astra-gui")).unwrap();
    assert!(entry_matches(&paths, &directory.path().join("astra-gui")).unwrap());
    fs::remove_file(contents.join("Resources/Astra.icns")).unwrap();
    assert!(!entry_matches(&paths, &directory.path().join("astra-gui")).unwrap());
    publish_entry(&paths, &directory.path().join("astra-gui")).unwrap();
    assert!(entry_matches(&paths, &directory.path().join("astra-gui")).unwrap());
}

#[test]
fn graphical_layout_keeps_terminal_entries_and_uses_native_desktop_locations() {
    let terminal = layout(InstallScope::System, "client").unwrap();
    assert_eq!(
        layout_for_frontend(InstallScope::System, "client", false)
            .unwrap()
            .entry,
        terminal.entry
    );
    let gui = layout_for_frontend(InstallScope::System, "client", true).unwrap();
    assert_eq!(gui.root, terminal.root);
    #[cfg(target_os = "macos")]
    assert_eq!(
        gui.entry,
        PathBuf::from("/Applications/Astra.app/Contents/MacOS/astra-launch")
    );
    #[cfg(all(unix, not(target_os = "macos")))]
    assert_eq!(
        gui.entry,
        PathBuf::from("/usr/local/share/applications/astra-client.desktop")
    );
    #[cfg(windows)]
    assert_eq!(gui.entry, terminal.entry);
    assert!(layout_for_frontend(InstallScope::System, "controller", true).is_err());
}
