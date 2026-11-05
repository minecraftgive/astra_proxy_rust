use super::*;
#[test]
fn editor_arguments_preserve_windows_paths_unicode_and_metacharacters_without_a_shell() {
    assert_eq!(
        arguments(r#""C:\Program Files\Editor\editor.exe" --wait "中文 空格""#).unwrap(),
        [r"C:\Program Files\Editor\editor.exe", "--wait", "中文 空格"]
    );
    assert_eq!(
        arguments(r#"["C:\\Program Files\\Editor\\editor.exe","--wait","$value;literal"]"#)
            .unwrap(),
        [
            r"C:\Program Files\Editor\editor.exe",
            "--wait",
            "$value;literal"
        ]
    );
    for text in ["", "[]", "['invalid']", "\"unclosed", "[\"\"]"] {
        assert!(arguments(text).is_err(), "{text}");
    }
}
#[test]
fn editor_process_receives_literal_draft_path_and_failure_retains_the_file() {
    let home =
        std::env::temp_dir().join(format!("astra editor process 中文 {}", std::process::id()));
    astra_platform::storage::private_dir(&home).unwrap();
    let draft = Draft::create(&home, "proxy".into(), "standalone=true\n".into()).unwrap();
    #[cfg(windows)]
    let (script, command) = {
        let script = home.join("editor helper.ps1");
        fs::write(&script,"param([string]$target)\n[System.IO.File]::WriteAllText($target, \"standalone=false`n\", [System.Text.UTF8Encoding]::new($false))\n").unwrap();
        let args = vec![
            "powershell.exe".into(),
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-File".into(),
            script.to_string_lossy().into_owned(),
        ];
        (script, args)
    };
    #[cfg(not(windows))]
    let (script, command) = {
        let script = home.join("editor helper.sh");
        fs::write(&script, "printf 'standalone=false\\n' > \"$1\"\n").unwrap();
        let args = vec!["/bin/sh".into(), script.to_string_lossy().into_owned()];
        (script, args)
    };
    launch(&command, &draft, &home).unwrap();
    assert_eq!(draft.read().unwrap(), "standalone=false\n");
    fs::write(&script, "exit 7\n").unwrap();
    assert!(launch(&command, &draft, &home).is_err());
    assert!(launch(&["astra-editor-that-does-not-exist".into()], &draft, &home).is_err());
    assert_eq!(draft.read().unwrap(), "standalone=false\n");
    fs::remove_file(script).unwrap();
    fs::remove_file(draft.path).unwrap();
    fs::remove_dir(home.join("external-drafts")).unwrap();
    fs::remove_dir(home).unwrap();
}
#[test]
fn draft_io_is_bounded_and_does_not_change_the_saved_configuration() {
    let home = std::env::temp_dir().join(format!("astra-external-{}", std::process::id()));
    astra_platform::storage::private_dir(&home).unwrap();
    let source = home.join("saved.toml");
    fs::write(&source, "original").unwrap();
    let draft = Draft::create(&home, "proxy".into(), "standalone=true\n".into()).unwrap();
    fs::write(&draft.path, "\u{feff}standalone=false\n").unwrap();
    assert_eq!(draft.read().unwrap(), "standalone=false\n");
    assert_eq!(fs::read_to_string(&source).unwrap(), "original");
    fs::write(&draft.path, vec![b'a'; 480 * 1024 + 1]).unwrap();
    assert!(draft.read().is_err());
    fs::remove_file(&draft.path).unwrap();
    fs::remove_file(source).unwrap();
    fs::remove_dir(home.join("external-drafts")).unwrap();
    fs::remove_dir(home).unwrap();
}
