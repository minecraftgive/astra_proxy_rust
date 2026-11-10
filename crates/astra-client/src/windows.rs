//! Independent terminal windows sharing the existing manager, never the UI checkpoint.
use crate::{i18n::Language, protocol::Kind};
use anyhow::Result;
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};
#[cfg(windows)]
use {anyhow::Context as _, std::process::Command};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Dashboard,
    Proxies,
    Connections,
    Dns,
    Rules,
    Logs,
    Profiles,
    Settings,
    TuiLog,
    Diagnostics,
}
impl View {
    pub fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "overview" | "dashboard" => Self::Dashboard,
            "proxies" => Self::Proxies,
            "connections" => Self::Connections,
            "dns" => Self::Dns,
            // Tools were split into Rules (route queries) and Proxies (diagnostics).
            "rules" | "tools" => Self::Rules,
            "logs" => Self::Logs,
            "profiles" => Self::Profiles,
            "settings" => Self::Settings,
            "tui-log" => Self::TuiLog,
            "diagnostics" => Self::Diagnostics,
            _ => anyhow::bail!("Unknown view: {value}"),
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Dashboard => "overview",
            Self::Proxies => "proxies",
            Self::Connections => "connections",
            Self::Dns => "dns",
            Self::Rules => "rules",
            Self::Logs => "logs",
            Self::Profiles => "profiles",
            Self::Settings => "settings",
            Self::TuiLog => "tui-log",
            Self::Diagnostics => "diagnostics",
        }
    }
    /// Page index shared by the page bar, 1–7 and Alt+1–7.
    pub fn page(self) -> Option<usize> {
        match self {
            Self::Dashboard => Some(0),
            Self::Proxies => Some(1),
            Self::Connections => Some(2),
            Self::Rules => Some(3),
            Self::Logs => Some(4),
            Self::Profiles => Some(5),
            Self::Settings => Some(6),
            Self::Dns => Some(crate::navigation::DNS),
            _ => None,
        }
    }
    pub fn from_page(index: usize) -> Self {
        [
            Self::Dashboard,
            Self::Proxies,
            Self::Connections,
            Self::Rules,
            Self::Logs,
            Self::Profiles,
            Self::Settings,
            Self::Dns,
        ][index.min(7)]
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
            astra_platform::storage::private_dir(&directory)?;
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
    pub fn graphical(&self) -> bool {
        self.executable
            .file_stem()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                if cfg!(windows) {
                    name.eq_ignore_ascii_case("astra-gui")
                        || name.eq_ignore_ascii_case("astra-server-gui")
                } else {
                    name == "astra-gui" || name == "astra-server-gui"
                }
            })
    }
    /// Preserve storage and manager identity while selecting the companion frontend.
    pub fn for_frontend(&self, frontend: crate::preferences::StartupFrontend) -> Result<Self> {
        use crate::preferences::StartupFrontend;
        let stem = self
            .executable
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        let folded = if cfg!(windows) {
            stem.to_ascii_lowercase()
        } else {
            stem.to_owned()
        };
        let server = match folded.as_str() {
            "astra-server-tui" | "astra-server-gui" => true,
            "astra-tui" | "astra-gui" => false,
            _ => anyhow::bail!("Cannot identify the current interface executable"),
        };
        let name = match (server, frontend) {
            (false, StartupFrontend::Tui) => "astra-tui",
            (false, StartupFrontend::Gui) => "astra-gui",
            (true, StartupFrontend::Tui) => "astra-server-tui",
            (true, StartupFrontend::Gui) => "astra-server-gui",
        };
        let sibling = self
            .executable
            .with_file_name(astra_platform::platform::executable(name));
        let mut context = self.clone();
        context.executable =
            if sibling.is_file() || self.executable.parent() == Some(self.bin.as_path()) {
                sibling
            } else {
                self.bin.join(astra_platform::platform::executable(name))
            };
        Ok(context)
    }
    pub fn arguments(&self, launch: &Launch) -> Vec<OsString> {
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
        if let Some((endpoint, agent)) = crate::protocol::target() {
            args.extend(["--endpoint".into(), endpoint.into_os_string()]);
            if let Some(agent) = agent {
                args.extend(["--agent".into(), agent.into()]);
            }
        }
        if let Some(view) = launch.view {
            args.extend(["--view".into(), view.name().into()]);
        }
        if let Some(instance) = &launch.instance {
            args.extend(["--instance".into(), instance.into()]);
        }
        if let Some(kind) = launch.kind {
            args.extend([
                "--core".into(),
                match kind {
                    Kind::Proxy => "proxy",
                    Kind::Rule => "rule",
                    Kind::Server => "server",
                }
                .into(),
            ]);
        }
        if let Some(id) = &launch.window_id {
            args.extend(["--window-id".into(), id.into()]);
        }
        args
    }
    pub fn open(&self, launch: &Launch, terminal: &str) -> Result<()> {
        let args = self.arguments(launch);
        let title = format!("Astra · {}", launch.view.unwrap_or(View::Dashboard).name());
        if matches!(
            crate::terminals::launch(terminal, &self.executable, &args)?,
            crate::terminals::LaunchOutcome::Native
        ) {
            #[cfg(windows)]
            {
                // A missing/disabled external host still has the native console.
                // This internal flag prevents the fallback from scanning again.
                let mut child = Command::new(&self.executable)
                    .args(&args)
                    .arg("--native-console")
                    .arg("--terminal-native")
                    .spawn()
                    .context("Cannot open a new terminal window")?;
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
            }
            #[cfg(not(windows))]
            anyhow::bail!(
                "No supported terminal is installed; install a terminal and rescan in Settings"
            );
        }
        if let Ok(log) = crate::tui_log::Log::frontend(&self.home) {
            log.write("window", &format!("opened {title}"));
        }
        Ok(())
    }
}
#[cfg(windows)]
pub mod installation;
#[cfg(test)]
use crate::shell::shell_quote;
#[cfg(test)]
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
#[path = "test/windows_test.rs"]
mod tests;
