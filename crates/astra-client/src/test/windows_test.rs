use super::*;
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
