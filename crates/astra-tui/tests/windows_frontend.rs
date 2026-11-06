#![cfg(windows)]

use std::{path::PathBuf, process::Command};

const FRONTENDS: [&str; 2] = [
    env!("CARGO_BIN_EXE_astra-tui"),
    env!("CARGO_BIN_EXE_astra-server-tui"),
];

struct Temporary(PathBuf);
impl Temporary {
    fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "astra-frontend-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Temporary {
    fn drop(&mut self) {
        // Only this freshly created fixture tree is ever removed.
        if let (Ok(parent), Ok(target)) = (
            std::fs::canonicalize(std::env::temp_dir()),
            std::fs::canonicalize(&self.0),
        ) {
            assert_eq!(target, parent.join(self.0.file_name().unwrap()));
            let _ = std::fs::remove_dir_all(target);
        }
    }
}

#[test]
fn both_frontends_use_the_gui_subsystem_and_keep_redirected_help() {
    for executable in FRONTENDS {
        let bytes = std::fs::read(executable).unwrap();
        let pe = u32::from_le_bytes(bytes[0x3c..0x40].try_into().unwrap()) as usize;
        assert_eq!(&bytes[pe..pe + 4], b"PE\0\0");
        // The subsystem field has the same offset in PE32 and PE32+ headers.
        let subsystem = u16::from_le_bytes(bytes[pe + 24 + 68..pe + 24 + 70].try_into().unwrap());
        assert_eq!(
            subsystem, 2,
            "{executable} must not allocate a console before main"
        );
        let output = Command::new(executable).arg("--help").output().unwrap();
        assert!(output.status.success(), "{:?}", output);
        assert!(String::from_utf8_lossy(&output.stdout).contains("--silent"));
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn silent_launch_logs_early_and_connection_errors_without_starting_a_manager() {
    for executable in FRONTENDS {
        for early_error in [false, true] {
            let temporary = Temporary::new();
            let state = temporary.0.join("state");
            let config = temporary.0.join("config");
            let missing = temporary.0.join("missing-manager.sock");
            let mut command = Command::new(executable);
            if early_error {
                // Silent detection must work even before the parser reaches
                // that flag, without losing its nonzero exit status.
                command.arg("--unknown-startup-option");
            }
            let output = command
                .arg("--silent")
                .arg("--state-dir")
                .arg(&state)
                .arg("--config-dir")
                .arg(&config)
                .arg("--endpoint")
                .arg(&missing)
                .output()
                .unwrap();
            assert!(!output.status.success());
            let log = std::fs::read_to_string(state.join("startup.log")).unwrap();
            assert!(
                log.contains(if early_error {
                    "Unknown argument: --unknown-startup-option"
                } else {
                    "Manager/controller endpoint does not exist"
                }),
                "{log}"
            );
            assert!(output.stdout.is_empty());
            assert!(!state.join("management.sock").exists());
            assert!(!state.join("ui-session.json").exists());
            assert!(!config.exists());
        }
    }
}

#[test]
fn ordinary_command_errors_keep_redirected_stderr() {
    for executable in FRONTENDS {
        let output = Command::new(executable)
            .arg("--unknown-startup-option")
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr)
            .contains("Unknown argument: --unknown-startup-option"));
    }
}

#[test]
fn internal_terminal_flags_reject_headless_and_conflicting_modes() {
    for executable in FRONTENDS {
        for args in [
            vec!["--terminal-child", "--terminal-native"],
            vec!["--terminal-child", "--manager"],
            vec!["--terminal-native", "--shutdown"],
            vec!["--terminal-child", "update-status"],
        ] {
            let output = Command::new(executable).args(args).output().unwrap();
            assert!(!output.status.success());
            assert!(String::from_utf8_lossy(&output.stderr)
                .contains("Terminal host flags require an interactive frontend"));
        }
    }
}
