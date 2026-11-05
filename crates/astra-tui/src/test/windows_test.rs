use super::*;
#[cfg(windows)]
#[test]
#[ignore = "opens a separate Windows console briefly"]
fn native_window_console_smoke() {
    use std::{io::IsTerminal, os::windows::process::CommandExt};
    const MARKER: &str = "ASTRA_WINDOW_TEST_MARKER";
    if let Some(path) = std::env::var_os(MARKER) {
        attach_console_io().unwrap();
        let size = crossterm::terminal::size().unwrap();
        std::fs::write(path,serde_json::to_vec(&serde_json::json!({"stdin":std::io::stdin().is_terminal(),"stdout":std::io::stdout().is_terminal(),"width":size.0,"height":size.1})).unwrap()).unwrap();
        return;
    }
    let marker = std::env::temp_dir().join(format!(
        "astra-window-console-{}-{}.json",
        std::process::id(),
        astra_ipc::progress::now_ms()
    ));
    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "windows::tests::native_window_console_smoke",
            "--ignored",
            "--nocapture",
        ])
        .env(MARKER, &marker)
        .creation_flags(0x00000010)
        .status()
        .unwrap();
    assert!(status.success());
    let result: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&marker).unwrap()).unwrap();
    std::fs::remove_file(marker).unwrap();
    assert_eq!(result["stdin"], true);
    assert_eq!(result["stdout"], true);
    assert!(result["width"].as_u64().unwrap() > 0);
    assert!(result["height"].as_u64().unwrap() > 0);
}
#[test]
fn arguments_keep_custom_storage_language_instance_and_literal_paths() {
    let context = Context {
        executable: "C:/Astra space/astra-tui.exe".into(),
        home: "C:/state space".into(),
        config: "C:/配置&config".into(),
        bin: "C:/core;bin".into(),
        language: Language::Traditional,
    };
    let launch = Launch::detached(View::Connections, Some("edge".into()), Kind::Proxy);
    let args = context.arguments(&launch);
    assert!(args.windows(2).any(|a| a
        == [
            OsString::from("--state-dir"),
            context.home.clone().into_os_string()
        ]));
    for pair in [
        ["--lang", "zh-TW"],
        ["--view", "connections"],
        ["--instance", "edge"],
        ["--core", "proxy"],
    ] {
        assert!(args.windows(2).any(|a| a == pair.map(OsString::from)));
    }
    assert_ne!(
        launch.window_id,
        Launch::detached(View::Connections, None, Kind::Proxy).window_id
    );
    assert!(View::parse("bad").is_err());
    assert_eq!(View::parse("tools").unwrap(), View::Rules);
    assert_eq!(View::parse("dashboard").unwrap(), View::Dashboard);
    for view in [
        View::Dashboard,
        View::Proxies,
        View::Connections,
        View::Rules,
        View::Logs,
        View::Profiles,
        View::Settings,
        View::TuiLog,
        View::Diagnostics,
    ] {
        assert_eq!(View::parse(view.name()).unwrap(), view);
    }
}
#[test]
fn window_sessions_cannot_escape_state_directory_or_overwrite_primary() {
    let home = std::env::temp_dir().join(format!("astra-windows-{}", std::process::id()));
    for id in ["../ui-session", "a/b", "a\\b", "", "."] {
        assert!(Launch {
            window_id: Some(id.into()),
            ..Default::default()
        }
        .session_path(&home)
        .is_err());
    }
    let first = Launch::detached(View::Logs, None, Kind::Proxy)
        .session_path(&home)
        .unwrap();
    let second = Launch::detached(View::Logs, None, Kind::Proxy)
        .session_path(&home)
        .unwrap();
    assert_ne!(first, second);
    assert_ne!(first, Launch::default().session_path(&home).unwrap());
    assert!(first.starts_with(home.join("window-sessions")));
    std::fs::remove_dir_all(home).unwrap();
}
#[test]
fn terminal_script_quoting_keeps_metacharacters_literal() {
    assert_eq!(shell_quote("a'b;$()"), "'a'\"'\"'b;$()'");
    assert_eq!(apple_quote("a\"b\\c\nd"), "\"a\\\"b\\\\c\\nd\"");
}
