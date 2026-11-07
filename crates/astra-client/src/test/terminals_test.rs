use super::*;

#[test]
fn discovery_scans_known_paths_without_cross_platform_or_duplicate_entries() {
    let locations = Locations {
        path: vec![PathBuf::from("tools"), PathBuf::from("other")],
        local: Some("local".into()),
        programs: vec!["programs".into()],
        home: Some("home".into()),
    };
    let installed = [
        PathBuf::from("local/Microsoft/WindowsApps/wt.exe"),
        PathBuf::from("programs/WezTerm/wezterm-gui.exe"),
        PathBuf::from("tools/wezterm.exe"),
        PathBuf::from("other/wezterm.exe"),
        PathBuf::from("tools/kitty.exe"),
    ];
    let windows = discover_in(Platform::Windows, &locations, |path, _| {
        installed.contains(&path.to_owned())
    });
    assert_eq!(
        windows.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
        ["auto", "native", "windows-terminal", "wezterm"]
    );
    assert_eq!(
        windows[2].executable.as_deref(),
        Some(Path::new("local/Microsoft/WindowsApps/wt.exe"))
    );
    assert_eq!(
        windows[3].executable.as_deref(),
        Some(Path::new("tools/wezterm.exe"))
    );
    let linux = discover_in(Platform::Linux, &locations, |path, bundle| {
        !bundle && path == Path::new("tools/kitty")
    });
    assert_eq!(
        linux.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
        ["auto", "kitty"]
    );
    let mac = discover_in(Platform::Mac, &locations, |path, bundle| {
        (bundle && path == Path::new("/System/Applications/Utilities/Terminal.app"))
            || path == Path::new("home/Applications/Alacritty.app/Contents/MacOS/alacritty")
    });
    assert_eq!(
        mac.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
        ["auto", "terminal", "alacritty"]
    );
}

#[test]
fn selection_falls_back_to_auto_without_interpreting_ids_as_commands() {
    let choices = discover_in(Platform::Windows, &Locations::default(), |_, _| false);
    assert!(ordered("native", &choices).is_empty());
    let choices = discover_in(
        Platform::Linux,
        &Locations {
            path: vec!["tools".into()],
            ..Default::default()
        },
        |_, _| true,
    );
    let ids = |id| {
        ordered(id, &choices)
            .into_iter()
            .map(|c| c.id.as_str())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids("missing; launch.exe"), ids("auto"));
    assert_eq!(ids("native"), ids("auto"));
    let selected = ids("kitty");
    assert_eq!(selected[0], "kitty");
    assert_eq!(selected.iter().filter(|id| **id == "kitty").count(), 1);
}

#[test]
fn terminal_templates_keep_literal_arguments_and_use_known_command_boundaries() {
    let command = [
        "/app/TUI ' $;",
        "--state-dir",
        "/用户/state\"\\",
        "--terminal-child",
    ]
    .map(OsString::from);
    for (id, prefix) in [
        ("gnome-terminal", vec!["--window", "--"]),
        ("wezterm", vec!["start", "--always-new-process", "--"]),
        ("alacritty", vec!["-e"]),
        ("konsole", vec!["--separate", "-e"]),
        ("xfce4-terminal", vec!["--disable-server", "--execute"]),
        ("kitty", vec!["--"]),
    ] {
        let candidate = Candidate {
            id: id.into(),
            label: id.into(),
            executable: Some(PathBuf::from(id)),
        };
        let invocation = invocation(&candidate, Platform::Linux, &command).unwrap();
        assert_eq!(
            &invocation.args[..prefix.len()],
            prefix
                .iter()
                .map(|value| OsString::from(*value))
                .collect::<Vec<_>>()
        );
        assert_eq!(&invocation.args[prefix.len()..], &command);
    }
    let unknown = Candidate {
        id: "custom;cmd".into(),
        label: "unknown".into(),
        executable: Some("evil".into()),
    };
    assert!(invocation(&unknown, Platform::Linux, &command).is_err());
}

#[test]
fn windows_bridge_encodes_payload_and_quotes_crt_arguments() {
    assert_eq!(windows_argument(""), "\"\"");
    assert_eq!(windows_argument("tail\\"), "\"tail\\\\\"");
    assert_eq!(windows_argument("a\"b"), "\"a\\\"b\"");
    assert_eq!(base64(b""), "");
    assert_eq!(base64(b"f"), "Zg==");
    assert_eq!(base64(b"fo"), "Zm8=");
    assert_eq!(base64(b"foo"), "Zm9v");
    let command = powershell_bridge(
        Path::new("C:/app ' $;/tui.exe"),
        &["a; $() ' \" tail\\".into()],
        Path::new("C:/Windows/powershell.exe"),
    )
    .unwrap();
    assert_eq!(command[4], "-EncodedCommand");
    let payload = command[5].to_str().unwrap();
    assert!(payload
        .bytes()
        .all(|c| c.is_ascii_alphanumeric() || b"+/=".contains(&c)));
    let candidate = Candidate {
        id: "windows-terminal".into(),
        label: "WT".into(),
        executable: Some("wt.exe".into()),
    };
    let invocation = invocation(&candidate, Platform::Windows, &command).unwrap();
    assert_eq!(
        &invocation.args[..3],
        ["-w", "new", "new-tab"].map(OsString::from)
    );
    assert!(!invocation
        .args
        .iter()
        .any(|arg| arg.to_string_lossy().contains(';')));
}

#[test]
fn mac_command_file_quotes_values_and_only_removes_its_own_private_directory() {
    let script = script_body(
        Path::new("/Applications/Astra ' $()/tui"),
        &[
            "--state-dir".into(),
            "/tmp/a' b; $HOME".into(),
            "--terminal-child".into(),
        ],
        Path::new("/tmp/private ' dir"),
    )
    .unwrap();
    assert!(script.contains("/bin/rm -f -- \"$0\""));
    assert!(script.contains("/bin/rmdir '/tmp/private '\"'\"' dir'"));
    assert!(script.contains("exec '/Applications/Astra '\"'\"' $()/tui'"));
    assert!(script.contains("'/tmp/a'\"'\"' b; $HOME'"));
    assert!(script.ends_with("'--terminal-child'\n"));
}

#[cfg(windows)]
#[test]
fn powershell_bridge_preserves_real_argv_waits_for_child_and_returns_its_exit_code() {
    use std::{
        io,
        os::windows::process::CommandExt,
        time::{Duration, Instant},
    };
    let root = std::env::temp_dir().join(format!(
        "astra-terminal-bridge-{}-{}",
        std::process::id(),
        astra_ipc::progress::now_ms()
    ));
    std::fs::create_dir(&root).unwrap();
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            if let (Ok(parent), Ok(target)) = (
                std::fs::canonicalize(std::env::temp_dir()),
                std::fs::canonicalize(&self.0),
            ) {
                assert_eq!(target, parent.join(self.0.file_name().unwrap()));
                let _ = std::fs::remove_dir_all(target);
            }
        }
    }
    let _cleanup = Cleanup(root.clone());
    let powershell = PathBuf::from(std::env::var_os("SystemRoot").unwrap())
        .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let literals = [
        "Unicode 用户",
        "apostrophe ' quote \" ; $() & %",
        "trailing\\",
        "",
    ];
    let args: Vec<OsString> = [
        "--exact",
        "terminals::tests::bridge_child",
        "--ignored",
        "--nocapture",
    ]
    .into_iter()
    .chain(literals)
    .map(OsString::from)
    .collect();
    let command = powershell_bridge(&std::env::current_exe().unwrap(), &args, &powershell).unwrap();
    let mut child = Command::new(&command[0])
        .args(&command[1..])
        .env("ASTRA_TERMINAL_BRIDGE_TEST", &root)
        .creation_flags(0x08000000)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while !root.join("ready.json").exists() {
        if let Some(status) = child.try_wait().unwrap() {
            let mut errors = String::new();
            use io::Read;
            child
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut errors)
                .unwrap();
            panic!("Bridge exited before the child became ready: {status}: {errors}");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("Bridge child did not become ready");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        child.try_wait().unwrap().is_none(),
        "Bridge must wait until the TUI child exits"
    );
    let actual: Vec<String> =
        serde_json::from_slice(&std::fs::read(root.join("ready.json")).unwrap()).unwrap();
    assert_eq!(&actual[actual.len() - literals.len()..], literals);
    std::fs::write(root.join("release"), b"go").unwrap();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert_eq!(status.code(), Some(23));
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("Bridge failed to exit after its child");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(windows)]
#[test]
#[ignore = "invoked only by the isolated Windows terminal bridge regression"]
fn bridge_child() {
    let Some(root) = std::env::var_os("ASTRA_TERMINAL_BRIDGE_TEST").map(PathBuf::from) else {
        return;
    };
    std::fs::write(
        root.join("ready.tmp"),
        serde_json::to_vec(&std::env::args().collect::<Vec<_>>()).unwrap(),
    )
    .unwrap();
    std::fs::rename(root.join("ready.tmp"), root.join("ready.json")).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while !root.join("release").exists() {
        if std::time::Instant::now() >= deadline {
            std::process::exit(24);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    std::process::exit(23);
}

#[test]
fn external_command_resolution_rejects_missing_programs_and_preserves_paths() {
    let executable = std::env::current_exe().unwrap();
    assert_eq!(resolve_command(&executable).unwrap(), executable);
    assert!(resolve_command(Path::new("/nonexistent/astra-terminal-command-test")).is_err());
}

#[test]
fn external_editor_terminal_templates_do_not_add_frontend_arguments() {
    let arguments = [
        OsString::from("--wait"),
        OsString::from("/tmp/config ' $();.toml"),
    ];
    let script = script_body(
        Path::new("/usr/bin/editor"),
        &arguments,
        Path::new("/tmp/private-editor"),
    )
    .unwrap();
    assert!(script.contains("exec '/usr/bin/editor' '--wait' '/tmp/config '\"'\"' $();.toml'"));
    assert!(!script.contains("--terminal-child"));
    let candidate = Candidate {
        id: "xterm".into(),
        label: "xterm".into(),
        executable: Some("/usr/bin/xterm".into()),
    };
    let command = std::iter::once(OsString::from("/usr/bin/editor"))
        .chain(arguments)
        .collect::<Vec<_>>();
    let launch = invocation(&candidate, Platform::Linux, &command).unwrap();
    assert_eq!(&launch.args[1..], command);
    assert!(!launch.args.iter().any(|arg| arg == "--terminal-child"));
}
