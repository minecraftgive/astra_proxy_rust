//! Independent terminal windows sharing the existing manager, never the UI checkpoint.
use crate::{i18n::Language, protocol::Kind};
use anyhow::{Context as _, Result};
#[cfg(not(windows))]
use std::process::Stdio;
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Dashboard,
    Proxies,
    Profiles,
    Connections,
    Tools,
    Logs,
    TuiLog,
    Diagnostics,
}
impl View {
    pub fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "dashboard" => Self::Dashboard,
            "proxies" => Self::Proxies,
            "profiles" => Self::Profiles,
            "connections" => Self::Connections,
            "tools" => Self::Tools,
            "logs" => Self::Logs,
            "tui-log" => Self::TuiLog,
            "diagnostics" => Self::Diagnostics,
            _ => anyhow::bail!("Unknown view: {value}"),
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Dashboard => "dashboard",
            Self::Proxies => "proxies",
            Self::Profiles => "profiles",
            Self::Connections => "connections",
            Self::Tools => "tools",
            Self::Logs => "logs",
            Self::TuiLog => "tui-log",
            Self::Diagnostics => "diagnostics",
        }
    }
    pub fn page(self) -> Option<usize> {
        match self {
            Self::Dashboard => Some(0),
            Self::Proxies => Some(1),
            Self::Profiles => Some(2),
            Self::Connections => Some(3),
            Self::Tools => Some(4),
            _ => None,
        }
    }
    pub fn from_page(index: usize) -> Self {
        [
            Self::Dashboard,
            Self::Proxies,
            Self::Profiles,
            Self::Connections,
            Self::Tools,
        ][index.min(4)]
    }
}
#[derive(Debug, Clone, Default)]
pub struct Launch {
    pub view: Option<View>,
    pub instance: Option<String>,
    pub kind: Option<Kind>,
    pub window_id: Option<String>,
}
impl Launch {
    pub fn session_path(&self, home: &Path) -> Result<PathBuf> {
        if let Some(id) = &self.window_id {
            anyhow::ensure!(
                !id.is_empty()
                    && id.len() <= 80
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
                "Invalid window ID"
            );
            let directory = home.join("window-sessions");
            crate::manager::private_dir(&directory)?;
            Ok(directory.join(format!("{id}.json")))
        } else {
            Ok(home.join("ui-session.json"))
        }
    }
    pub fn detached(view: View, instance: Option<String>, kind: Kind) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Self {
            view: Some(view),
            instance,
            kind: Some(kind),
            window_id: Some(format!(
                "{}-{}-{}",
                std::process::id(),
                astra_ipc::progress::now_ms(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            )),
        }
    }
}
#[derive(Clone)]
pub struct Context {
    pub executable: PathBuf,
    pub home: PathBuf,
    pub config: PathBuf,
    pub bin: PathBuf,
    pub language: Language,
}
impl Context {
    fn arguments(&self, launch: &Launch) -> Vec<OsString> {
        let mut args = vec![
            "--state-dir".into(),
            self.home.clone().into_os_string(),
            "--config-dir".into(),
            self.config.clone().into_os_string(),
            "--bin-dir".into(),
            self.bin.clone().into_os_string(),
            "--lang".into(),
            match self.language {
                Language::English => "en",
                Language::Simplified => "zh-CN",
                Language::Traditional => "zh-TW",
            }
            .into(),
        ];
        if let Some(view) = launch.view {
            args.extend(["--view".into(), view.name().into()]);
        }
        if let Some(instance) = &launch.instance {
            args.extend(["--instance".into(), instance.into()]);
        }
        if let Some(kind) = launch.kind {
            args.extend([
                "--core".into(),
                if kind == Kind::Proxy { "proxy" } else { "rule" }.into(),
            ]);
        }
        if let Some(id) = &launch.window_id {
            args.extend(["--window-id".into(), id.into()]);
        }
        args
    }
    pub fn open(&self, launch: &Launch) -> Result<()> {
        let args = self.arguments(launch);
        let title = format!("Astra · {}", launch.view.unwrap_or(View::Dashboard).name());
        #[cfg(windows)]
        let child = {
            use std::os::windows::process::CommandExt;
            // An explicit user request for a separate interactive console.
            Command::new(&self.executable)
                .args(&args)
                .arg("--native-console")
                .creation_flags(0x00000010)
                .spawn()
                .context("Cannot open a new terminal window")?
        };
        #[cfg(target_os = "macos")]
        let child = {
            let command = std::iter::once(self.executable.as_os_str())
                .chain(args.iter().map(|a| a.as_os_str()))
                .map(|s| shell_quote(&s.to_string_lossy()))
                .collect::<Vec<_>>()
                .join(" ");
            let script = format!(
                "tell application \"Terminal\"\nactivate\ndo script {}\nend tell",
                apple_quote(&format!("exec {command}"))
            );
            Command::new("osascript")
                .arg("-e")
                .arg(script)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .context("Cannot open Terminal")?
        };
        #[cfg(not(any(windows, target_os = "macos")))]
        let child = {
            let candidates: [(&str, &[&str]); 4] = [
                ("x-terminal-emulator", &["-e"]),
                ("gnome-terminal", &["--window", "--"]),
                ("konsole", &["--separate", "-e"]),
                ("xterm", &["-e"]),
            ];
            let mut launched = None;
            for (program, prefix) in candidates {
                match Command::new(program)
                    .args(prefix)
                    .arg(&self.executable)
                    .args(&args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                {
                    Ok(child) => {
                        launched = Some(child);
                        break;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(e).context("Cannot open terminal"),
                }
            }
            launched.context("Install x-terminal-emulator, gnome-terminal, konsole or xterm to open a new window")?
        };
        // Reap launcher/terminal processes without holding up the TUI.
        std::thread::spawn(move || {
            let mut child = child;
            let _ = child.wait();
        });
        if let Ok(log) = crate::tui_log::Log::open(&self.home.join("tui.log")) {
            log.write("window", &format!("opened {title}"));
        }
        Ok(())
    }
}
// A new Windows console must use its own input/output, even if the parent had
// inherited handles to another terminal or redirected output.
#[cfg(windows)]
pub fn attach_console_io() -> Result<()> {
    use std::{fs::OpenOptions, os::windows::io::AsRawHandle};
    #[link(name = "kernel32")]
    extern "system" {
        fn SetStdHandle(which: u32, handle: *mut std::ffi::c_void) -> i32;
    }
    for (which, path) in [(-10i32, "CONIN$"), (-11, "CONOUT$"), (-12, "CONOUT$")] {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .context("Open new console input/output")?;
        // The OS standard-handle table owns these handles until process exit.
        if unsafe { SetStdHandle(which as u32, file.as_raw_handle()) } == 0 {
            return Err(std::io::Error::last_os_error()).context("Attach new console input/output");
        }
        std::mem::forget(file);
    }
    Ok(())
}
#[cfg(any(target_os = "macos", test))]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}
#[cfg(any(target_os = "macos", test))]
fn apple_quote(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
    )
}

#[cfg(test)]
mod tests {
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
        for view in [
            View::Dashboard,
            View::Proxies,
            View::Profiles,
            View::Connections,
            View::Tools,
            View::Logs,
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
}
