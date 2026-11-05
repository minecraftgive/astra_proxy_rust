//! User-owned startup artifacts. Writing these files never invokes launchctl,
//! systemctl, sudo, or a newly generated command.
#![cfg_attr(not(unix), allow(dead_code))]
use crate::{shell::shell_quote, windows::Context};
use anyhow::{Context as _, Result};
use sha2::{Digest, Sha256};
use std::{
    io,
    path::{Path, PathBuf},
};

type Target = Option<(PathBuf, Option<String>)>;
const MAX_FILE: u64 = 128 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Platform {
    Mac,
    Linux,
}
impl Platform {
    fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Mac
        } else {
            Self::Linux
        }
    }
}

struct Paths {
    platform: Platform,
    id: String,
    file: PathBuf,
    script: Option<PathBuf>,
}
impl Paths {
    fn new(platform: Platform, home: &Path, xdg: Option<&Path>, id: String) -> Self {
        let (file, script) = match platform {
            Platform::Mac => (
                home.join("Library/LaunchAgents")
                    .join(format!("com.astra.tui.{id}.plist")),
                Some(
                    home.join("Library/Application Support/Astra/startup")
                        .join(format!("com.astra.tui.{id}.command")),
                ),
            ),
            Platform::Linux => (
                xdg.filter(|path| path.is_absolute())
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| home.join(".config"))
                    .join("systemd/user")
                    .join(format!("astra-tui-{id}.service")),
                None,
            ),
        };
        Self {
            platform,
            id,
            file,
            script,
        }
    }
    fn marker(&self) -> String {
        format!("Astra managed startup: {}", self.id)
    }
    fn prefix(&self) -> String {
        match self.platform {
            Platform::Mac => format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!-- {} -->\n",
                self.marker()
            ),
            Platform::Linux => format!("# {}\n", self.marker()),
        }
    }
    fn script_prefix(&self) -> String {
        format!("#!/bin/sh\n# {}\n", self.marker())
    }
}

fn identity(context: &Context, target: &Target) -> String {
    let mut hash = Sha256::new();
    let mut add = |value: &std::ffi::OsStr| {
        hash.update(value.as_encoded_bytes());
        hash.update([0]);
    };
    add(context.home.as_os_str());
    add(context.executable.file_name().unwrap_or_default());
    if let Some((endpoint, agent)) = target {
        add(endpoint.as_os_str());
        add(std::ffi::OsStr::new(agent.as_deref().unwrap_or_default()));
    }
    format!("{:x}", hash.finalize())[..32].to_owned()
}
fn paths(context: &Context, target: &Target) -> Result<Paths> {
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is unavailable")?);
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    resolved_paths(
        Platform::current(),
        &home,
        xdg.as_deref(),
        identity(context, target),
    )
}

fn resolved_paths(
    platform: Platform,
    home: &Path,
    xdg: Option<&Path>,
    id: String,
) -> Result<Paths> {
    anyhow::ensure!(home.is_absolute(), "HOME must be an absolute path");
    // HOME and the selected configuration root are user-selected aliases.
    // Resolve those roots before checking the managed descendants below them.
    let home = std::fs::canonicalize(home).context("Resolve user home directory")?;
    let config = if platform == Platform::Linux {
        let requested = xdg
            .filter(|path| path.is_absolute())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| home.join(".config"));
        Some(resolve_config_root(&requested)?)
    } else {
        None
    };
    Ok(Paths::new(platform, &home, config.as_deref(), id))
}

/// Resolve an existing root (including /home aliases) without creating it.
/// XDG_CONFIG_HOME may name an as-yet missing directory, so resolve its nearest
/// existing ancestor and append only the missing ordinary path components.
fn resolve_config_root(path: &Path) -> Result<PathBuf> {
    anyhow::ensure!(path.is_absolute(), "Configuration root must be absolute");
    let mut existing = path;
    let mut missing = Vec::new();
    loop {
        match std::fs::canonicalize(existing) {
            Ok(mut resolved) => {
                anyhow::ensure!(
                    resolved.is_dir(),
                    "Configuration root is not a directory: {}",
                    existing.display()
                );
                for component in missing.into_iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // An existing dangling alias is a broken root, not a missing
                // directory that this feature should replace or repair.
                match std::fs::symlink_metadata(existing) {
                    Ok(_) => return Err(error).context("Resolve configuration root"),
                    Err(inspect) if inspect.kind() == io::ErrorKind::NotFound => {}
                    Err(inspect) => return Err(inspect).context("Inspect configuration root"),
                }
                missing.push(
                    existing
                        .file_name()
                        .context("Cannot resolve configuration root")?
                        .to_owned(),
                );
                existing = existing
                    .parent()
                    .context("Configuration root has no existing ancestor")?;
            }
            Err(error) => return Err(error).context("Resolve configuration root"),
        }
    }
}

pub(crate) fn managed_path(context: &Context) -> Result<PathBuf> {
    Ok(paths(context, &crate::protocol::target())?.file)
}
pub(crate) fn instructions(context: &Context) -> Result<String> {
    let paths = paths(context, &crate::protocol::target())?;
    activation_instructions(&paths)
}
fn activation_instructions(paths: &Paths) -> Result<String> {
    Ok(match paths.platform {
        Platform::Mac => crate::i18n::t("The LaunchAgent takes effect at the next login.").into(),
        Platform::Linux => format!(
            "systemctl --user daemon-reload\nunit='astra-tui-{}.service'\nfile={}\nif [ \"$file\" -ef \"$(systemctl --user show --property=FragmentPath --value \"$unit\")\" ]; then\n  systemctl --user enable \"$unit\"\nelse\n  systemctl --user enable \"$file\"\nfi",
            paths.id,
            shell_quote(path_text(&paths.file)?)
        ),
    })
}
pub(crate) fn enabled(context: &Context) -> Result<bool> {
    let paths = paths(context, &crate::protocol::target())?;
    managed_file(&paths.file, &paths.prefix())
}
pub(crate) fn set_enabled(context: &Context, enabled: bool, silent: bool) -> Result<()> {
    let target = crate::protocol::target();
    let paths = paths(context, &target)?;
    let command = Command::from_context(context, &target)?;
    apply(&paths, &command, enabled, silent)
}

struct Command {
    executable: String,
    arguments: Vec<String>,
}
impl Command {
    fn from_context(context: &Context, target: &Target) -> Result<Self> {
        let mut arguments = vec![
            "--state-dir".into(),
            absolute(&context.home)?,
            "--config-dir".into(),
            absolute(&context.config)?,
            "--bin-dir".into(),
            absolute(&context.bin)?,
        ];
        if let Some((endpoint, agent)) = target {
            arguments.extend(["--endpoint".into(), absolute(endpoint)?]);
            if let Some(agent) = agent {
                arguments.extend(["--agent".into(), valid_text(agent)?.into()]);
            }
        }
        Ok(Self {
            executable: absolute(&context.executable)?,
            arguments,
        })
    }
    fn argv(&self, silent: bool) -> Vec<String> {
        let mut args = vec![self.executable.clone()];
        args.extend(self.arguments.clone());
        if silent {
            args.push("--silent".into());
        }
        args
    }
}
fn valid_text(value: &str) -> Result<&str> {
    anyhow::ensure!(
        !value.chars().any(char::is_control),
        "Startup paths and arguments cannot contain control characters"
    );
    Ok(value)
}
fn path_text(path: &Path) -> Result<&str> {
    valid_text(path.to_str().context("Startup paths must be valid UTF-8")?)
}
fn absolute(path: &Path) -> Result<String> {
    Ok(path_text(&std::path::absolute(path)?)?.into())
}

fn xml(value: &str) -> Result<String> {
    valid_text(value)?;
    anyhow::ensure!(
        !value.contains(['\u{fffe}', '\u{ffff}']),
        "Invalid XML character in startup value"
    );
    Ok(value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;"))
}
fn mac_plist(paths: &Paths, command: &Command, silent: bool) -> Result<String> {
    let args = if silent {
        command.argv(true)
    } else {
        vec![
            "/usr/bin/open".into(),
            "-a".into(),
            "Terminal".into(),
            path_text(
                paths
                    .script
                    .as_deref()
                    .context("Missing Terminal startup script")?,
            )?
            .into(),
        ]
    };
    let args = args
        .iter()
        .map(|arg| Ok(format!("    <string>{}</string>\n", xml(arg)?)))
        .collect::<Result<String>>()?;
    Ok(format!("{}<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key>\n  <string>com.astra.tui.{}</string>\n  <key>ProgramArguments</key>\n  <array>\n{}  </array>\n  <key>RunAtLoad</key>\n  <true/>\n  <key>LimitLoadToSessionType</key>\n  <string>Aqua</string>\n</dict>\n</plist>\n", paths.prefix(), paths.id, args))
}
fn mac_script(paths: &Paths, command: &Command) -> Result<String> {
    let args = command
        .argv(false)
        .iter()
        .map(|arg| Ok(shell_quote(valid_text(arg)?)))
        .collect::<Result<Vec<_>>>()?
        .join(" ");
    Ok(format!("{}exec {args}\n", paths.script_prefix()))
}
/// ':' disables systemd's environment expansion for the entire command. This
/// also preserves '$' in the executable path (which is not an environment word).
/// '%' still expands unit specifiers and therefore must be doubled separately.
fn systemd_word(value: &str) -> Result<String> {
    valid_text(value)?;
    Ok(format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
    ))
}
fn linux_unit(paths: &Paths, command: &Command) -> Result<String> {
    // systemd rejects quotes and backslashes in the executable itself even
    // when escaped. A fixed shell program passes the actual path as argv,
    // without interpolating any user-controlled text into shell source.
    let mut args = vec![
        ":/bin/sh".into(),
        "-c".into(),
        "exec \"$@\"".into(),
        "astra-startup".into(),
    ];
    args.extend(command.argv(true));
    let args = args
        .iter()
        .map(|arg| systemd_word(arg))
        .collect::<Result<Vec<_>>>()?
        .join(" ");
    Ok(format!("{}# Requires systemd 242 or newer (Type=exec and the ':' command prefix).\n[Unit]\nDescription=Astra startup\n\n[Service]\n# Release default.target after exec; instance startup continues asynchronously.\nType=exec\nRemainAfterExit=yes\n# The manager has its own lifecycle; stopping this launcher leaves it running.\nKillMode=process\nTimeoutStartSec=30s\nUMask=0077\nExecStart={args}\n\n[Install]\nWantedBy=default.target\n", paths.prefix()))
}

fn apply(paths: &Paths, command: &Command, enabled: bool, silent: bool) -> Result<()> {
    apply_with_writer(paths, command, enabled, silent, write_managed)
}
fn apply_with_writer(
    paths: &Paths,
    command: &Command,
    enabled: bool,
    silent: bool,
    mut write: impl FnMut(&Path, &[u8], u32) -> Result<()>,
) -> Result<()> {
    // Check every existing destination before any write/remove. Managed markers
    // prevent coincidentally named files from being replaced or deleted.
    let had_file = managed_file(&paths.file, &paths.prefix())?;
    let had_script = match &paths.script {
        Some(path) => managed_file(path, &paths.script_prefix())?,
        None => false,
    };
    if !enabled {
        if had_file {
            std::fs::remove_file(&paths.file)?;
        }
        if had_script {
            // Removing the registration is the commit. A leftover unused
            // helper is harmless; cleanup must not report a failed disable.
            let _ = std::fs::remove_file(paths.script.as_ref().unwrap());
        }
        return Ok(());
    }
    match paths.platform {
        Platform::Mac => {
            let plist = mac_plist(paths, command, silent)?;
            if silent {
                // Keep any old helper until disable. Its cleanup cannot be
                // allowed to fail after committing the new startup mode.
                write(&paths.file, plist.as_bytes(), 0o600)?;
                return Ok(());
            }
            let script = paths.script.as_ref().unwrap();
            let previous = had_script
                .then(|| astra_platform::storage::read_bounded(script, MAX_FILE))
                .transpose()?;
            write(script, mac_script(paths, command)?.as_bytes(), 0o700)?;
            if let Err(error) = write(&paths.file, plist.as_bytes(), 0o600) {
                let rollback = match previous {
                    Some(bytes) => write(script, &bytes, 0o700),
                    None => std::fs::remove_file(script).map_err(Into::into),
                };
                return match rollback {
                    Ok(()) => Err(error),
                    Err(rollback) => Err(error.context(format!(
                        "Could not restore the previous startup script: {rollback}"
                    ))),
                };
            }
        }
        Platform::Linux => write(&paths.file, linux_unit(paths, command)?.as_bytes(), 0o600)?,
    }
    Ok(())
}

fn managed_file(path: &Path, prefix: &str) -> Result<bool> {
    // An absent artifact needs no directory permission decision. In
    // particular, merely opening settings must not reject an inherited
    // configuration directory when there is no registration to inspect.
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("Inspect startup file"),
    };
    anyhow::ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "Startup destination is not a regular file: {}",
        path.display()
    );
    let parent = path.parent().context("Startup file has no parent")?;
    // symlink_metadata does not read the artifact. Check every ancestor before
    // opening existing content, including links followed while finding it.
    if !check_directories(parent, false)? {
        return Ok(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        anyhow::ensure!(
            metadata.uid() == unsafe { libc::geteuid() }
                && metadata.nlink() == 1
                && metadata.mode() & 0o022 == 0,
            "Startup file is not privately owned: {}",
            path.display()
        );
    }
    let bytes = astra_platform::storage::read_bounded(path, MAX_FILE)?;
    anyhow::ensure!(
        bytes.starts_with(prefix.as_bytes()),
        "Refusing to change a startup file not generated by Astra: {}",
        path.display()
    );
    Ok(true)
}
fn write_managed(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_FILE,
        "Generated startup file exceeds size limit"
    );
    let parent = path.parent().context("Startup file has no parent")?;
    check_directories(parent, true)?;
    #[cfg(unix)]
    {
        use std::{
            io::Write,
            os::unix::fs::{OpenOptionsExt, PermissionsExt},
            sync::atomic::{AtomicU64, Ordering},
        };
        static NEXT: AtomicU64 = AtomicU64::new(0);
        struct Temporary(Option<PathBuf>);
        impl Drop for Temporary {
            fn drop(&mut self) {
                if let Some(path) = &self.0 {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
        let (mut temporary, mut file) = loop {
            let candidate = parent.join(format!(
                ".astra-startup-{}-{}.tmp",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .open(&candidate)
            {
                Ok(file) => break (Temporary(Some(candidate)), file),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error).context("Create startup temporary file"),
            }
        };
        // Set exact permissions before publication, regardless of the umask.
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()?;
        std::fs::rename(temporary.0.as_ref().unwrap(), path).context("Save startup file")?;
        temporary.0 = None;
        // Nothing fallible may follow the authoritative rename: callers use
        // success to commit the selected mode in their preferences.
        let _ = astra_platform::storage::sync_directory(parent);
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
        astra_platform::storage::atomic_write(
            path,
            bytes,
            astra_platform::storage::WritePolicy {
                sync_directory: false,
                ..Default::default()
            },
        )
        .context("Save startup file")?;
    }
    Ok(())
}
fn check_directories(path: &Path, create: bool) -> Result<bool> {
    anyhow::ensure!(path.is_absolute(), "Startup directory must be absolute");
    for directory in path.ancestors().collect::<Vec<_>>().into_iter().rev() {
        let metadata = match std::fs::symlink_metadata(directory) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound && !create => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let builder = std::fs::DirBuilder::new();
                #[cfg(unix)]
                let builder = {
                    use std::os::unix::fs::DirBuilderExt;
                    let mut builder = builder;
                    builder.mode(0o700);
                    builder
                };
                match builder.create(directory) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error.into()),
                }
                std::fs::symlink_metadata(directory)?
            }
            Err(error) => return Err(error.into()),
        };
        anyhow::ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "Startup path is not a real directory: {}",
            directory.display()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            validate_directory_permissions(
                directory,
                directory == path,
                DirectoryPermissions {
                    owner: metadata.uid(),
                    group: metadata.gid(),
                    mode: metadata.mode(),
                },
                unsafe { libc::geteuid() },
                unsafe { libc::getegid() },
            )?;
        }
    }
    Ok(true)
}

#[cfg(any(unix, test))]
struct DirectoryPermissions {
    owner: u32,
    group: u32,
    mode: u32,
}
#[cfg(any(unix, test))]
fn validate_directory_permissions(
    path: &Path,
    destination: bool,
    permissions: DirectoryPermissions,
    user: u32,
    primary_group: u32,
) -> Result<()> {
    anyhow::ensure!(
        permissions.owner == user || permissions.owner == 0,
        "Startup directory is owned by another user: {}",
        path.display()
    );
    anyhow::ensure!(
        !destination || permissions.owner == user,
        "Startup directory is not owned by this user: {}",
        path.display()
    );
    // Trusted sticky ancestors such as /tmp prevent another user from
    // replacing our entries. A writable destination itself is never exempt.
    if !destination && permissions.mode & 0o1000 != 0 {
        return Ok(());
    }
    anyhow::ensure!(
        permissions.mode & 0o002 == 0,
        "Startup directory is writable by everyone: {}",
        path.display()
    );
    // Existing user configuration directories commonly inherit 0775 under
    // the user's primary group. Accept that convention without enumerating
    // NSS accounts or claiming the group has no other members. New managed
    // directories remain 0700, and managed files retain strict permissions.
    anyhow::ensure!(
        permissions.mode & 0o020 == 0
            || (permissions.owner == user && permissions.group == primary_group),
        "Startup directory has unsupported group-write permissions: {}",
        path.display()
    );
    Ok(())
}

#[cfg(test)]
#[path = "test/startup_unix_test.rs"]
mod tests;

#[cfg(test)]
#[path = "test/startup_unix_permissions_test.rs"]
mod permissions_tests;
