//! Discover a small set of known terminal applications and launch fixed argv
//! templates. Persist IDs, never executable paths or arbitrary shell commands.
use anyhow::{Context as _, Result};
use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub id: String,
    pub label: String,
    pub executable: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LaunchOutcome {
    Launched {
        terminal: String,
    },
    /// No external terminal was launched. The caller can use the native
    /// Windows console or retain its existing terminal on Unix.
    Native,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Platform {
    Windows,
    Mac,
    Linux,
}
impl Platform {
    fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::Mac
        } else {
            Self::Linux
        }
    }
}

struct Spec {
    id: &'static str,
    label: &'static str,
    names: &'static [&'static str],
}
fn specs(platform: Platform) -> &'static [Spec] {
    match platform {
        Platform::Windows => &[
            Spec {
                id: "windows-terminal",
                label: "Windows Terminal",
                names: &["wt.exe"],
            },
            Spec {
                id: "wezterm",
                label: "WezTerm",
                names: &["wezterm-gui.exe", "wezterm.exe"],
            },
            Spec {
                id: "alacritty",
                label: "Alacritty",
                names: &["alacritty.exe"],
            },
        ],
        Platform::Mac => &[
            Spec {
                id: "terminal",
                label: "Terminal",
                names: &[],
            },
            Spec {
                id: "wezterm",
                label: "WezTerm",
                names: &["wezterm"],
            },
            Spec {
                id: "alacritty",
                label: "Alacritty",
                names: &["alacritty"],
            },
        ],
        Platform::Linux => &[
            Spec {
                id: "x-terminal-emulator",
                label: "System terminal",
                names: &["x-terminal-emulator"],
            },
            Spec {
                id: "gnome-terminal",
                label: "GNOME Terminal",
                names: &["gnome-terminal"],
            },
            Spec {
                id: "konsole",
                label: "Konsole",
                names: &["konsole"],
            },
            Spec {
                id: "xfce4-terminal",
                label: "Xfce Terminal",
                names: &["xfce4-terminal"],
            },
            Spec {
                id: "wezterm",
                label: "WezTerm",
                names: &["wezterm"],
            },
            Spec {
                id: "alacritty",
                label: "Alacritty",
                names: &["alacritty"],
            },
            Spec {
                id: "kitty",
                label: "Kitty",
                names: &["kitty"],
            },
            Spec {
                id: "foot",
                label: "Foot",
                names: &["foot"],
            },
            Spec {
                id: "xterm",
                label: "XTerm",
                names: &["xterm"],
            },
        ],
    }
}

#[derive(Default)]
struct Locations {
    path: Vec<PathBuf>,
    home: Option<PathBuf>,
    local: Option<PathBuf>,
    programs: Vec<PathBuf>,
}
impl Locations {
    fn environment(platform: Platform) -> Self {
        let mut path: Vec<_> = std::env::var_os("PATH")
            .map(|value| {
                std::env::split_paths(&value)
                    .filter(|p| p.is_absolute())
                    .collect()
            })
            .unwrap_or_default();
        if platform != Platform::Windows {
            path.extend(
                ["/usr/local/bin", "/usr/bin", "/bin", "/opt/homebrew/bin"].map(PathBuf::from),
            );
        }
        Self {
            path,
            home: std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute()),
            local: std::env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute()),
            programs: ["ProgramFiles", "ProgramFiles(x86)"]
                .into_iter()
                .filter_map(std::env::var_os)
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .collect(),
        }
    }
    fn candidates(&self, platform: Platform, spec: &Spec) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        for name in spec.names {
            paths.extend(self.path.iter().map(|directory| directory.join(name)));
        }
        match platform {
            Platform::Windows => {
                if spec.id == "windows-terminal" {
                    if let Some(local) = &self.local {
                        paths.push(local.join("Microsoft/WindowsApps/wt.exe"));
                    }
                } else {
                    let folder = if spec.id == "wezterm" {
                        "WezTerm"
                    } else {
                        "Alacritty"
                    };
                    for base in &self.programs {
                        paths.extend(spec.names.iter().map(|name| base.join(folder).join(name)));
                    }
                    if let Some(local) = &self.local {
                        paths.extend(
                            spec.names
                                .iter()
                                .map(|name| local.join("Programs").join(folder).join(name)),
                        );
                    }
                }
            }
            Platform::Mac => {
                if spec.id == "terminal" {
                    paths.extend(
                        [
                            "/System/Applications/Utilities/Terminal.app",
                            "/Applications/Utilities/Terminal.app",
                        ]
                        .map(PathBuf::from),
                    );
                } else {
                    let relative = if spec.id == "wezterm" {
                        "WezTerm.app/Contents/MacOS/wezterm"
                    } else {
                        "Alacritty.app/Contents/MacOS/alacritty"
                    };
                    paths.push(Path::new("/Applications").join(relative));
                    if let Some(home) = &self.home {
                        paths.push(home.join("Applications").join(relative));
                    }
                }
            }
            Platform::Linux => {}
        }
        paths
    }
}

fn executable_file(path: &Path, bundle: bool) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if bundle {
        return metadata.is_dir();
    }
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn discover_in(
    platform: Platform,
    locations: &Locations,
    exists: impl Fn(&Path, bool) -> bool,
) -> Vec<Candidate> {
    let mut found = vec![Candidate {
        id: "auto".into(),
        label: "Auto".into(),
        executable: None,
    }];
    if platform == Platform::Windows {
        found.push(Candidate {
            id: "native".into(),
            label: "Native console".into(),
            executable: None,
        });
    }
    for spec in specs(platform) {
        if let Some(executable) = locations
            .candidates(platform, spec)
            .into_iter()
            .find(|path| exists(path, spec.id == "terminal"))
        {
            found.push(Candidate {
                id: spec.id.into(),
                label: spec.label.into(),
                executable: Some(executable),
            });
        }
    }
    found
}

/// Filesystem-only discovery: never launches terminals or queries their shells.
pub(crate) fn discover() -> Vec<Candidate> {
    let platform = Platform::current();
    discover_in(platform, &Locations::environment(platform), executable_file)
}

fn ordered<'a>(id: &str, candidates: &'a [Candidate]) -> Vec<&'a Candidate> {
    if id == "native" && candidates.iter().any(|candidate| candidate.id == "native") {
        return Vec::new();
    }
    let selected = candidates
        .iter()
        .find(|candidate| candidate.id == id && candidate.executable.is_some());
    selected
        .into_iter()
        .chain(candidates.iter().filter(|candidate| {
            candidate.executable.is_some()
                && Some(candidate.id.as_str()) != selected.map(|c| c.id.as_str())
        }))
        .collect()
}

struct Invocation {
    program: PathBuf,
    args: Vec<OsString>,
}

fn invocation(
    candidate: &Candidate,
    platform: Platform,
    command: &[OsString],
) -> Result<Invocation> {
    let program = candidate
        .executable
        .clone()
        .context("Terminal has no executable")?;
    anyhow::ensure!(
        specs(platform).iter().any(|spec| spec.id == candidate.id),
        "Unknown terminal ID"
    );
    let prefix: &[&str] = match candidate.id.as_str() {
        "windows-terminal" => &["-w", "new", "new-tab"],
        "wezterm" => &["start", "--always-new-process", "--"],
        "alacritty" => &["-e"],
        "gnome-terminal" => &["--window", "--"],
        "konsole" => &["--separate", "-e"],
        "xfce4-terminal" => &["--disable-server", "--execute"],
        "kitty" | "foot" => &["--"],
        "x-terminal-emulator" | "xterm" => &["-e"],
        _ => anyhow::bail!("Terminal requires a platform-specific launcher"),
    };
    Ok(Invocation {
        program,
        args: prefix
            .iter()
            .map(|value| OsString::from(*value))
            .chain(command.iter().cloned())
            .collect(),
    })
}

fn text(value: &OsStr) -> Result<&str> {
    let value = value
        .to_str()
        .context("Terminal arguments must be valid Unicode")?;
    anyhow::ensure!(
        !value.contains('\0'),
        "Terminal arguments cannot contain NUL"
    );
    Ok(value)
}

// The child is a Windows GUI-subsystem program, so the terminal needs a real
// console process that waits for that child (not its background descendants).
fn powershell_bridge(
    executable: &Path,
    args: &[OsString],
    powershell: &Path,
) -> Result<Vec<OsString>> {
    let quote = |value: &str| format!("'{}'", value.replace('\'', "''"));
    let arguments = args
        .iter()
        .map(|arg| Ok(windows_argument(text(arg)?)))
        .collect::<Result<Vec<_>>>()?
        .join(" ");
    let script = format!(
        "$ErrorActionPreference='Stop';try{{$p=Start-Process -FilePath {} -ArgumentList {} -NoNewWindow -PassThru;$null=$p.Handle;$p.WaitForExit();exit $p.ExitCode}}catch{{[Console]::Error.WriteLine($_.Exception.Message);exit 1}}",
        quote(text(executable.as_os_str())?), quote(&arguments)
    );
    let bytes: Vec<_> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    Ok(vec![
        powershell.as_os_str().into(),
        "-NoLogo".into(),
        "-NoProfile".into(),
        "-NonInteractive".into(),
        "-EncodedCommand".into(),
        base64(&bytes).into(),
    ])
}

fn windows_argument(value: &str) -> String {
    let mut result = String::from("\"");
    let mut slashes = 0;
    for character in value.chars() {
        match character {
            '\\' => slashes += 1,
            '"' => {
                result.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
                result.push('"');
                slashes = 0;
            }
            _ => {
                result.extend(std::iter::repeat_n('\\', slashes));
                slashes = 0;
                result.push(character);
            }
        }
    }
    result.extend(std::iter::repeat_n('\\', slashes * 2));
    result.push('"');
    result
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::new();
    for chunk in bytes.chunks(3) {
        let bits = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        output.push(ALPHABET[(bits >> 18) as usize] as char);
        output.push(ALPHABET[((bits >> 12) & 63) as usize] as char);
        output.push(if chunk.len() > 1 {
            ALPHABET[((bits >> 6) & 63) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            ALPHABET[(bits & 63) as usize] as char
        } else {
            '='
        });
    }
    output
}

fn spawn(invocation: &Invocation) -> std::io::Result<std::process::Child> {
    let mut command = Command::new(&invocation.program);
    command
        .args(&invocation.args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Hide a CLI launcher (wezterm.exe/wt.exe), not the requested GUI
        // terminal. GUI-subsystem executables ignore CREATE_NO_WINDOW.
        command.creation_flags(0x08000000);
    }
    command.spawn()
}

#[cfg(target_os = "macos")]
fn terminal_script(executable: &Path, args: &[OsString]) -> Result<PathBuf> {
    use std::{
        io::Write,
        os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
        sync::atomic::{AtomicU64, Ordering},
    };
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let parent = loop {
        let candidate = std::env::temp_dir().join(format!(
            "astra-terminal-{}-{}-{}",
            std::process::id(),
            astra_ipc::progress::now_ms(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        match std::fs::DirBuilder::new().mode(0o700).create(&candidate) {
            Ok(()) => break candidate,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    };
    let path = parent.join("launch.command");
    let result = (|| {
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700))?;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .open(&path)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o700))?;
        file.write_all(script_body(executable, args, &parent)?.as_bytes())?;
        file.sync_all()?;
        Ok::<_, anyhow::Error>(path.clone())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&parent);
    }
    result
}

#[cfg(any(target_os = "macos", test))]
fn script_body(executable: &Path, args: &[OsString], parent: &Path) -> Result<String> {
    let command = std::iter::once(executable.as_os_str())
        .chain(args.iter().map(OsString::as_os_str))
        .map(|value| Ok(crate::shell::shell_quote(text(value)?)))
        .collect::<Result<Vec<_>>>()?
        .join(" ");
    Ok(format!(
        "#!/bin/sh\n/bin/rm -f -- \"$0\"\n/bin/rmdir {} 2>/dev/null || :\nexec {command}\n",
        crate::shell::shell_quote(text(parent.as_os_str())?)
    ))
}

/// Missing selections and failed terminal spawns try the same ordered Auto
/// choices, then leave native/current-terminal fallback to the caller.
pub(crate) fn launch(id: &str, executable: &Path, args: &[OsString]) -> Result<LaunchOutcome> {
    let platform = Platform::current();
    if id == "native" && platform == Platform::Windows {
        return Ok(LaunchOutcome::Native);
    }
    let executable = std::path::absolute(executable)?;
    anyhow::ensure!(
        executable.is_file(),
        "TUI executable is unavailable: {}",
        executable.display()
    );
    let candidates = discover();
    let selected = ordered(id, &candidates);
    if selected.is_empty() {
        return Ok(LaunchOutcome::Native);
    }
    let mut args = args.to_vec();
    args.push("--terminal-child".into());
    let command = if platform == Platform::Windows {
        let Some(system) = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
        else {
            return Ok(LaunchOutcome::Native);
        };
        let powershell = system.join("System32/WindowsPowerShell/v1.0/powershell.exe");
        if !powershell.is_file() {
            return Ok(LaunchOutcome::Native);
        }
        powershell_bridge(&executable, &args, &powershell)?
    } else {
        std::iter::once(executable.as_os_str().to_owned())
            .chain(args.iter().cloned())
            .collect()
    };
    for candidate in selected {
        #[cfg(target_os = "macos")]
        let (invocation, script) = if candidate.id == "terminal" {
            let script = terminal_script(&executable, &args)?;
            (
                Invocation {
                    program: "/usr/bin/open".into(),
                    args: vec![
                        "-a".into(),
                        candidate.executable.as_ref().unwrap().as_os_str().into(),
                        script.as_os_str().into(),
                    ],
                },
                Some(script),
            )
        } else {
            (invocation(candidate, platform, &command)?, None)
        };
        #[cfg(not(target_os = "macos"))]
        let invocation = invocation(candidate, platform, &command)?;
        match spawn(&invocation) {
            Ok(mut child) => {
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                return Ok(LaunchOutcome::Launched {
                    terminal: candidate.id.clone(),
                });
            }
            Err(_) =>
            {
                #[cfg(target_os = "macos")]
                if let Some(path) = script {
                    let _ = std::fs::remove_file(&path);
                    if let Some(parent) = path.parent() {
                        let _ = std::fs::remove_dir(parent);
                    }
                }
            }
        }
    }
    Ok(LaunchOutcome::Native)
}

#[cfg(test)]
#[path = "test/terminals_test.rs"]
mod tests;
