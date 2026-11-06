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
