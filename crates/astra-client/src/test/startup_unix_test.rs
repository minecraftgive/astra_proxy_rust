use super::*;

struct Temporary(PathBuf);
impl Temporary {
    fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "astra-startup-unix-{}-{}-{}",
            std::process::id(),
            astra_ipc::progress::now_ms(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(std::fs::canonicalize(path).unwrap())
    }
}
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn command() -> Command {
    Command {
        executable: "/opt/Astra $build%/astra-tui".into(),
        arguments: vec![
            "--state-dir".into(),
            "/home/test/状态 & 'state'".into(),
            "--config-dir".into(),
            "/home/test/<config>\"".into(),
            "--bin-dir".into(),
            "/opt/core\\bin".into(),
            "--endpoint".into(),
            "/home/test/controller.sock".into(),
            "--agent".into(),
            "node;$HOME%h".into(),
        ],
    }
}
fn context(root: &Path) -> Context {
    Context {
        executable: root.join("bin/astra-tui"),
        home: root.join("state"),
        config: root.join("config"),
        bin: root.join("bin"),
        language: crate::i18n::Language::Simplified,
    }
}

#[test]
fn startup_identity_is_stable_across_releases_but_separates_users_and_targets() {
    let temp = Temporary::new();
    let original = context(&temp.0);
    let id = identity(&original, &None);
    let mut updated = original.clone();
    updated.executable = temp.0.join("release2/astra-tui");
    updated.bin = temp.0.join("release2");
    assert_eq!(identity(&updated, &None), id);
    updated.executable = temp.0.join("release2/astra-server-tui");
    assert_ne!(identity(&updated, &None), id);
    updated = original.clone();
    updated.home = temp.0.join("second-state");
    assert_ne!(identity(&updated, &None), id);
    let first = Some((temp.0.join("controller.sock"), Some("first".into())));
    let second = Some((temp.0.join("controller.sock"), Some("second".into())));
    assert_ne!(identity(&original, &first), id);
    assert_ne!(identity(&original, &first), identity(&original, &second));
}

#[test]
fn paths_use_only_absolute_xdg_config_and_keep_mac_files_in_user_library() {
    let temp = Temporary::new();
    let linux = Paths::new(
        Platform::Linux,
        &temp.0,
        Some(Path::new("relative-config")),
        "abc".into(),
    );
    assert_eq!(
        linux.file,
        temp.0.join(".config/systemd/user/astra-tui-abc.service")
    );
    let xdg = temp.0.join("custom config");
    let custom = Paths::new(Platform::Linux, &temp.0, Some(&xdg), "abc".into());
    assert_eq!(custom.file, xdg.join("systemd/user/astra-tui-abc.service"));
    let mac = Paths::new(Platform::Mac, &temp.0, Some(&xdg), "abc".into());
    assert_eq!(
        mac.file,
        temp.0.join("Library/LaunchAgents/com.astra.tui.abc.plist")
    );
    assert_eq!(
        mac.script.unwrap(),
        temp.0
            .join("Library/Application Support/Astra/startup/com.astra.tui.abc.command")
    );
}

#[test]
fn missing_config_roots_are_resolved_without_creating_directories() {
    let temp = Temporary::new();
    let custom = temp.0.join("missing/config with spaces");
    let paths = resolved_paths(Platform::Linux, &temp.0, Some(&custom), "abc".into()).unwrap();
    assert_eq!(
        paths.file,
        custom.join("systemd/user/astra-tui-abc.service")
    );
    assert!(!custom.exists());
    let fallback = resolved_paths(
        Platform::Linux,
        &temp.0,
        Some(Path::new("relative")),
        "abc".into(),
    )
    .unwrap();
    assert_eq!(
        fallback.file,
        temp.0.join(".config/systemd/user/astra-tui-abc.service")
    );
    assert!(!temp.0.join(".config").exists());
    let file = temp.0.join("not-a-directory");
    std::fs::write(&file, b"unchanged").unwrap();
    assert!(resolve_config_root(&file).is_err());
}

#[test]
fn activation_links_the_absolute_unit_path_independent_of_manager_xdg_environment() {
    let temp = Temporary::new();
    let custom = temp.0.join("custom config '$HOME%'");
    let paths = resolved_paths(Platform::Linux, &temp.0, Some(&custom), "abc".into()).unwrap();
    let instructions = activation_instructions(&paths).unwrap();
    assert_eq!(instructions.lines().count(), 8);
    assert_eq!(
        instructions.lines().next(),
        Some("systemctl --user daemon-reload")
    );
    assert_eq!(
        instructions.lines().nth(1),
        Some("unit='astra-tui-abc.service'")
    );
    let file = instructions.lines().nth(2).unwrap();
    assert_eq!(
        file,
        format!("file={}", shell_quote(paths.file.to_str().unwrap()))
    );
    assert!(file.contains("custom config '\"'\"'$HOME%'\"'\"'"));
    assert!(file.ends_with("astra-tui-abc.service'"));
    // Only use a unit name if the manager resolves it to this very file,
    // including aliases. Otherwise link the full path into its search path.
    assert_eq!(instructions.lines().nth(3), Some("if [ \"$file\" -ef \"$(systemctl --user show --property=FragmentPath --value \"$unit\")\" ]; then"));
    assert_eq!(
        instructions.lines().nth(4),
        Some("  systemctl --user enable \"$unit\"")
    );
    assert_eq!(
        instructions.lines().nth(6),
        Some("  systemctl --user enable \"$file\"")
    );
    assert!(!custom.exists());
}

#[cfg(unix)]
#[test]
fn activation_commands_use_the_managers_file_identity_and_preserve_literal_paths() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let temp = Temporary::new();
    let custom = temp.0.join("配置 '$HOME%\";back\\slash");
    let paths = Paths::new(Platform::Linux, &temp.0, Some(&custom), "abc".into());
    std::fs::create_dir_all(paths.file.parent().unwrap()).unwrap();
    std::fs::write(&paths.file, b"fixture only").unwrap();
    let alias = temp.0.join("loaded-alias.service");
    symlink(&paths.file, &alias).unwrap();
    let other = temp.0.join("other.service");
    std::fs::write(&other, b"another unit").unwrap();
    let bin = temp.0.join("bin");
    std::fs::create_dir(&bin).unwrap();
    let mock = bin.join("systemctl");
    // PATH contains only this stub. No real manager is contacted or changed.
    std::fs::write(
        &mock,
        br##"#!/bin/sh
[ "$1" = --user ] || exit 10
case "$2" in
  daemon-reload) [ "$#" = 2 ] || exit 11 ;;
  show) [ "$#" = 5 ] || exit 12
        [ "$3" = --property=FragmentPath ] || exit 13
        [ "$4" = --value ] || exit 14
        [ "$5" = astra-tui-abc.service ] || exit 15
        printf '%s\n' "$ASTRA_TEST_FRAGMENT" ;;
  enable) [ "$#" = 3 ] || exit 16
          printf '%s\n' "$3" > "$ASTRA_TEST_ENABLED" ;;
  *) exit 17 ;;
esac
"##,
    )
    .unwrap();
    std::fs::set_permissions(&mock, std::fs::Permissions::from_mode(0o700)).unwrap();
    let instructions = activation_instructions(&paths).unwrap();
    let enabled = temp.0.join("enabled-argument");
    for fragment in [Some(alias.as_path()), None, Some(other.as_path())] {
        let output = std::process::Command::new("/bin/sh")
            .args(["-c", &instructions])
            .env("PATH", &bin)
            .env("ASTRA_TEST_FRAGMENT", fragment.unwrap_or(Path::new("")))
            .env("ASTRA_TEST_ENABLED", &enabled)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let expected = if fragment == Some(alias.as_path()) {
            "astra-tui-abc.service"
        } else {
            paths.file.to_str().unwrap()
        };
        assert_eq!(
            std::fs::read_to_string(&enabled).unwrap(),
            format!("{expected}\n")
        );
    }
}

#[cfg(unix)]
#[test]
fn configured_roots_resolve_home_aliases_and_missing_tail_but_not_managed_links() {
    use std::os::unix::fs::symlink;
    let temp = Temporary::new();
    let actual_home = temp.0.join("var-home/user");
    std::fs::create_dir_all(&actual_home).unwrap();
    let alias = temp.0.join("home");
    symlink(actual_home.parent().unwrap(), &alias).unwrap();
    let user_home = alias.join("user");
    let configured = user_home.join(".config");
    let paths =
        resolved_paths(Platform::Linux, &user_home, Some(&configured), "abc".into()).unwrap();
    assert_eq!(
        paths.file,
        actual_home.join(".config/systemd/user/astra-tui-abc.service")
    );
    assert!(!actual_home.join(".config").exists());
    apply(&paths, &command(), true, true).unwrap();
    assert!(managed_file(&paths.file, &paths.prefix()).unwrap());

    let custom_alias = temp.0.join("config-alias");
    symlink(actual_home.join(".config"), &custom_alias).unwrap();
    let aliased = resolved_paths(
        Platform::Linux,
        &user_home,
        Some(&custom_alias),
        "abc".into(),
    )
    .unwrap();
    assert_eq!(aliased.file, paths.file);
    let dangling = temp.0.join("dangling-config");
    symlink(temp.0.join("missing-destination"), &dangling).unwrap();
    assert!(resolve_config_root(&dangling).is_err());

    let linked_user = actual_home.join(".config/systemd/linked-user");
    symlink(paths.file.parent().unwrap(), &linked_user).unwrap();
    let linked_file = linked_user.join("astra-tui-abc.service");
    assert!(managed_file(&linked_file, &paths.prefix()).is_err());
}

#[test]
fn mac_startup_runs_the_frontend_directly_and_escapes_xml_in_both_modes() {
    let temp = Temporary::new();
    let paths = Paths::new(Platform::Mac, &temp.0, None, "abc".into());
    let silent = mac_plist(&paths, &command(), true).unwrap();
    assert!(silent.contains("<string>/home/test/状态 &amp; &apos;state&apos;</string>"));
    assert!(silent.contains("&lt;config&gt;&quot;"));
    assert!(silent.contains("<string>--silent</string>"));
    assert!(silent.contains("<key>RunAtLoad</key>\n  <true/>"));
    assert!(!silent.contains("KeepAlive"));
    let interactive = mac_plist(&paths, &command(), false).unwrap();
    assert!(interactive.contains("<string>/opt/Astra $build%/astra-tui</string>"));
    assert!(interactive.contains("<string>/home/test/状态 &amp; &apos;state&apos;</string>"));
    assert!(interactive.contains("<string>node;$HOME%h</string>"));
    assert!(!interactive.contains("<string>/usr/bin/open</string>"));
    assert!(!interactive.contains("<string>Terminal</string>"));
    assert!(!interactive.contains("--silent"));
    assert!(!interactive.contains(".command"));
    assert!(!interactive.contains("--terminal-child"));
    assert!(!interactive.contains("KeepAlive"));
    assert!(!interactive.contains("osascript"));
}

#[test]
fn systemd_unit_disables_environment_expansion_and_preserves_specifier_literals() {
    let temp = Temporary::new();
    let paths = Paths::new(Platform::Linux, &temp.0, None, "abc".into());
    let unit = linux_unit(&paths, &command()).unwrap();
    assert!(unit.contains(
        "ExecStart=\":/bin/sh\" \"-c\" \"exec \\\"$@\\\"\" \"astra-startup\" \"/opt/Astra $build%%/astra-tui\""
    ));
    assert!(unit.contains("\"node;$HOME%%h\""));
    assert!(unit.contains("\"/opt/core\\\\bin\""));
    assert!(unit.contains("<config>\\\""));
    assert!(unit.contains("\"--silent\""));
    for line in [
        "Type=exec",
        "RemainAfterExit=yes",
        "KillMode=process",
        "TimeoutStartSec=30s",
        "WantedBy=default.target",
    ] {
        assert!(unit.lines().any(|value| value == line));
    }
    assert!(!unit.contains("ExecStop="));
    assert!(!unit.contains("Type=oneshot"));
    assert!(!unit.contains("TimeoutStartSec=0"));
    assert!(!unit.contains("sudo"));
    assert!(systemd_word("bad\nExecStart=/bin/evil").is_err());
    assert!(xml("bad\0value").is_err());
    assert!(xml("bad\u{ffff}value").is_err());
    let mut quoted = command();
    quoted.executable = "/opt/用户 'quoted' \"tui\";\\$HOME%h".into();
    let unit = linux_unit(&paths, &quoted).unwrap();
    assert!(unit.contains("\"astra-startup\" \"/opt/用户 'quoted' \\\"tui\\\";\\\\$HOME%%h\""));
}

#[test]
fn command_keeps_storage_and_remote_target_without_window_overrides() {
    let temp = Temporary::new();
    let context = context(&temp.0);
    let endpoint = temp.0.join("controller.sock");
    let target = Some((endpoint.clone(), Some("node with spaces".into())));
    let command = Command::from_context(&context, &target).unwrap();
    let arguments = command.argv(true);
    for (flag, value) in [
        ("--state-dir", context.home),
        ("--config-dir", context.config),
        ("--bin-dir", context.bin),
        ("--endpoint", endpoint),
    ] {
        assert!(arguments
            .windows(2)
            .any(|pair| pair == [flag, value.to_str().unwrap()]));
    }
    assert!(arguments
        .windows(2)
        .any(|pair| pair == ["--agent", "node with spaces"]));
    assert!(arguments.contains(&"--silent".into()));
    for absent in ["--lang", "--view", "--window-id", "--native-console"] {
        assert!(!arguments.iter().any(|arg| arg == absent));
    }
}

#[test]
fn managed_files_can_be_replaced_and_removed_without_touching_unrelated_files() {
    let temp = Temporary::new();
    let paths = Paths::new(Platform::Mac, &temp.0, None, "abc".into());
    assert!(!managed_file(&paths.file, &paths.prefix()).unwrap());
    apply(&paths, &command(), true, false).unwrap();
    assert!(managed_file(&paths.file, &paths.prefix()).unwrap());
    assert!(!paths.script.as_ref().unwrap().exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&paths.file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    // Older releases generated a helper alongside the plist. Re-registering
    // either mode preserves it; only an explicit disable cleans it up.
    let legacy_script = format!("{}exec /old/astra-tui\n", paths.script_prefix());
    write_managed(
        paths.script.as_ref().unwrap(),
        legacy_script.as_bytes(),
        0o700,
    )
    .unwrap();
    let unrelated = paths.file.with_file_name("unrelated.plist");
    std::fs::write(&unrelated, b"keep me").unwrap();
    apply(&paths, &command(), true, false).unwrap();
    apply(&paths, &command(), true, true).unwrap();
    assert_eq!(
        std::fs::read_to_string(paths.script.as_ref().unwrap()).unwrap(),
        legacy_script
    );
    apply(&paths, &command(), false, true).unwrap();
    assert!(!paths.file.exists());
    assert!(!paths.script.as_ref().unwrap().exists());
    assert_eq!(std::fs::read(unrelated).unwrap(), b"keep me");
    std::fs::write(&paths.file, b"user-owned unrelated file").unwrap();
    assert!(apply(&paths, &command(), true, true).is_err());
    assert!(apply(&paths, &command(), false, true).is_err());
    assert_eq!(
        std::fs::read(&paths.file).unwrap(),
        b"user-owned unrelated file"
    );
}

#[test]
fn failed_mac_plist_commit_preserves_registration_and_legacy_helper() {
    for previously_silent in [false, true] {
        let temp = Temporary::new();
        let paths = Paths::new(Platform::Mac, &temp.0, None, "abc".into());
        apply(&paths, &command(), true, previously_silent).unwrap();
        let previous_plist = std::fs::read(&paths.file).unwrap();
        let script = paths.script.as_ref().unwrap();
        if !previously_silent {
            write_managed(
                script,
                format!("{}exec /old/astra-tui\n", paths.script_prefix()).as_bytes(),
                0o700,
            )
            .unwrap();
        }
        let previous_script = std::fs::read(script).ok();
        let mut changed = command();
        changed.executable = "/opt/updated/astra-tui".into();
        let error = apply_with_writer(&paths, &changed, true, false, |path, bytes, mode| {
            anyhow::ensure!(path != paths.file, "simulated plist commit failure");
            write_managed(path, bytes, mode)
        })
        .unwrap_err();
        assert!(error.to_string().contains("simulated plist commit failure"));
        assert_eq!(std::fs::read(&paths.file).unwrap(), previous_plist);
        assert_eq!(std::fs::read(script).ok(), previous_script);
    }
}

#[test]
fn linux_generates_only_the_unit_even_if_interactive_preference_is_selected() {
    let temp = Temporary::new();
    let paths = Paths::new(Platform::Linux, &temp.0, None, "abc".into());
    apply(&paths, &command(), true, false).unwrap();
    let unit = std::fs::read_to_string(&paths.file).unwrap();
    assert!(unit.contains("\"--silent\""));
    assert!(!paths
        .file
        .parent()
        .unwrap()
        .join("default.target.wants")
        .exists());
    assert!(paths.script.is_none());
    apply(&paths, &command(), false, false).unwrap();
    assert!(!paths.file.exists());
}

#[cfg(unix)]
#[test]
fn symlinked_destinations_and_directories_are_not_overwritten_or_removed() {
    use std::os::unix::fs::symlink;
    let temp = Temporary::new();
    let paths = Paths::new(Platform::Linux, &temp.0, None, "abc".into());
    let victim = temp.0.join("victim");
    std::fs::write(&victim, "untouched").unwrap();
    check_directories(paths.file.parent().unwrap(), true).unwrap();
    symlink(&victim, &paths.file).unwrap();
    assert!(apply(&paths, &command(), true, true).is_err());
    assert!(apply(&paths, &command(), false, true).is_err());
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "untouched");
    let linked = temp.0.join("linked-config");
    symlink(temp.0.join(".config"), &linked).unwrap();
    let linked = Paths::new(Platform::Linux, &temp.0, Some(&linked), "def".into());
    assert!(apply(&linked, &command(), true, true).is_err());
}

#[cfg(target_os = "linux")]
#[test]
fn generated_unit_passes_systemd_analyze_without_loading_or_starting_it() {
    let temp = Temporary::new();
    let paths = Paths::new(Platform::Linux, &temp.0, None, "verify".into());
    let command = Command {
        executable: std::env::current_exe().unwrap().to_str().unwrap().into(),
        arguments: vec!["--state-dir".into(), "/tmp/state $literal%".into()],
    };
    apply(&paths, &command, true, true).unwrap();
    let output = match std::process::Command::new("systemd-analyze")
        .args(["--user", "verify"])
        .arg(&paths.file)
        .env("XDG_RUNTIME_DIR", &temp.0)
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => panic!("systemd-analyze: {error}"),
    };
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(target_os = "macos")]
#[test]
fn generated_plist_passes_plutil_without_loading_the_agent() {
    let temp = Temporary::new();
    let paths = Paths::new(Platform::Mac, &temp.0, None, "verify".into());
    for silent in [false, true] {
        apply(&paths, &command(), true, silent).unwrap();
        let output = std::process::Command::new("/usr/bin/plutil")
            .args(["-lint", "--"])
            .arg(&paths.file)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn gui_startup_uses_a_separate_identity_and_keeps_silent_mode_explicit() {
    let temp = Temporary::new();
    let original = context(&temp.0);
    let gui = original
        .for_frontend(crate::preferences::StartupFrontend::Gui)
        .unwrap();
    assert_ne!(identity(&gui, &None), identity(&original, &None));
    let command = Command::from_context(&gui, &None).unwrap();
    let paths = Paths::new(Platform::Mac, &temp.0, None, identity(&gui, &None));
    let plist = mac_plist(&paths, &command, false).unwrap();
    assert!(plist.contains("astra-gui"));
    assert!(!plist.contains("--native-console"));
    assert!(!plist.contains("--silent"));
    assert!(mac_plist(&paths, &command, true)
        .unwrap()
        .contains("--silent"));
}
