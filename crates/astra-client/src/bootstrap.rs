//! Shared launch parsing and manager discovery. All I/O runs on the backend executor.
use crate::{i18n, protocol, windows};
use anyhow::{Context, Result};
use astra_platform::platform::OpenOptionsExt;
use std::{
    fs::OpenOptions,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

pub const UI_REFRESH_INTERVAL: Duration = Duration::from_millis(500);
pub fn is_host_binary(executable: &Path) -> bool {
    executable
        .file_stem()
        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("astra-host"))
}

pub fn manager_executable(executable: &Path) -> PathBuf {
    let host = executable.with_file_name(if cfg!(windows) {
        "astra-host.exe"
    } else {
        "astra-host"
    });
    if host.is_file() {
        host
    } else {
        executable.to_owned()
    }
}

pub fn selected_startup_release(
    home: &Path,
    explicit_bin: bool,
) -> Result<Option<(PathBuf, PathBuf)>> {
    if explicit_bin {
        return Ok(None);
    }
    let Some(release) = astra_update::ReleaseStore::new(home).resolve_current()? else {
        return Ok(None);
    };
    let host = release
        .directory
        .join(astra_platform::platform::executable("astra-host"));
    anyhow::ensure!(release.manifest.components.contains_key("astra-host") && host.is_file(),
        "The selected release has no versioned astra-host; choose a complete client package or use an explicit --bin-dir for maintenance");
    Ok(Some((release.directory, host)))
}

pub fn redirect_host(
    executable: &Path,
    manager_binary: &Path,
    explicit_bin: bool,
    socket: &Path,
) -> Result<bool> {
    if explicit_bin || std::fs::canonicalize(manager_binary)? == std::fs::canonicalize(executable)?
    {
        return Ok(false);
    }
    anyhow::ensure!(
        !socket.exists(),
        "A manager endpoint already exists; a running host is never replaced during startup"
    );
    Ok(true)
}

pub fn selected_frontend(
    executable: &Path,
    bin: &Path,
    selected: bool,
    explicit_bin: bool,
) -> Result<Option<PathBuf>> {
    if !selected || explicit_bin {
        return Ok(None);
    }
    let frontend = bin.join(astra_platform::platform::executable("astra-tui"));
    if std::fs::canonicalize(&frontend)? == std::fs::canonicalize(executable)? {
        Ok(None)
    } else {
        Ok(Some(frontend))
    }
}

pub fn set_operation(
    operation: &mut Option<protocol::Action>,
    next: protocol::Action,
) -> Result<()> {
    anyhow::ensure!(operation.is_none(), "Specify only one update command");
    *operation = Some(next);
    Ok(())
}

pub fn finish_launch(
    launch: &mut windows::Launch,
    default_kind: Option<protocol::Kind>,
    prepared: bool,
) {
    if prepared {
        return;
    }
    if let (Some(view), None) = (launch.view, &launch.window_id) {
        launch.window_id = windows::Launch::detached(view, None, protocol::Kind::Proxy).window_id;
    }
    // The server's default page is still a primary launch. Only an explicitly
    // requested --view creates an independent window session above.
    if launch.view.is_none() && default_kind == Some(protocol::Kind::Server) {
        launch.view = Some(windows::View::Profiles);
    }
}

pub fn headless_options(args: &[std::ffi::OsString]) -> (bool, bool) {
    let mut silent = false;
    let mut daemon = false;
    let mut args = args.iter();
    while let Some(argument) = args.next() {
        match argument.to_str() {
            Some("--silent") => silent = true,
            Some("--manager") => daemon = true,
            // Values that happen to look like flags are still values, matching
            // the real parser. In particular a directory can be "--silent".
            Some(
                "--endpoint" | "--agent" | "--lang" | "--state-dir" | "--config-dir" | "--bin-dir"
                | "--view" | "--instance" | "--core" | "--window-id" | "--restart-ready" | "update",
            ) => {
                args.next();
            }
            _ => {}
        }
    }
    (silent, daemon)
}

pub fn validate_silent_launch(
    silent: bool,
    daemon: bool,
    shutdown: bool,
    operation: bool,
    native_console: bool,
    launch: &windows::Launch,
) -> Result<()> {
    if silent {
        anyhow::ensure!(
            cfg!(any(windows, target_os = "macos", target_os = "linux")),
            "Silent startup is supported only on Windows, macOS and Linux"
        );
        anyhow::ensure!(!daemon && !shutdown && !operation && !native_console && launch.view.is_none() && launch.window_id.is_none() && launch.instance.is_none(),
            "--silent cannot be combined with manager, shutdown, update, native-console or window selection options");
    }
    Ok(())
}

pub fn configure_manager_background(command: &mut Command, silent: bool) {
    #[cfg(unix)]
    if silent {
        use std::os::unix::process::CommandExt;
        // Do not also call background_command/process_group(0): making the
        // child a group leader before setsid would make setsid fail with EPERM.
        // SAFETY: setsid is async-signal-safe and this callback does no allocation.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        return;
    }
    #[cfg(not(unix))]
    let _ = silent;
    astra_platform::platform::background_command(command, true);
}

pub fn validate_restart_ready(home: &Path, path: &Path) -> Result<()> {
    anyhow::ensure!(
        path.is_absolute() && !path.try_exists()?,
        "Invalid restart readiness path"
    );
    let parent = path
        .parent()
        .context("Missing restart readiness directory")?;
    anyhow::ensure!(
        std::fs::canonicalize(parent)? == std::fs::canonicalize(home.join("restart-ready"))?,
        "Restart readiness path must be inside the state restart-ready directory"
    );
    Ok(())
}

pub struct Options {
    pub executable: PathBuf,
    pub home: PathBuf,
    pub config_home: PathBuf,
    pub bin: PathBuf,
    pub explicit_bin: bool,
    pub daemon: bool,
    pub shutdown: bool,
    pub endpoint: Option<PathBuf>,
    pub agent_id: Option<String>,
    pub operation: Option<protocol::Action>,
    pub launch: windows::Launch,
    pub native_console: bool,
    pub terminal_child: bool,
    pub terminal_native: bool,
    /// Run the TUI in the invoking terminal instead of a selected terminal window.
    pub here: bool,
    pub resume_ui: bool,
    pub restart_ready: Option<PathBuf>,
    pub silent: bool,
    pub help: bool,
}
impl Options {
    pub fn parse(
        default_kind: Option<protocol::Kind>,
        arguments: impl IntoIterator<Item = String>,
    ) -> Result<Self> {
        let executable = std::env::current_exe()?;
        let mut home = astra_platform::platform::home_dir()?.join(".astra/tui");
        let mut config_home = astra_platform::platform::home_dir()?.join(".astra/config");
        let mut bin = executable
            .parent()
            .context("Missing executable directory")?
            .to_path_buf();
        let mut explicit_bin = false;
        let mut daemon = is_host_binary(&executable);
        let mut shutdown = false;
        let mut endpoint = None;
        let mut agent_id = None;
        let mut operation = None;
        let mut launch = windows::Launch {
            kind: default_kind,
            ..Default::default()
        };
        let mut native_console = false;
        let mut terminal_child = false;
        let mut terminal_native = false;
        let mut here = false;
        let mut resume_ui = false;
        let mut restart_ready = None;
        let mut args = arguments.into_iter();
        let mut help = false;
        let mut silent = false;
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--endpoint" => {
                    endpoint = Some(PathBuf::from(
                        args.next()
                            .context("--endpoint requires a trusted local IPC path")?,
                    ))
                }
                "--agent" => {
                    agent_id = Some(
                        args.next()
                            .context("--agent requires a registered agent ID")?,
                    )
                }
                "--lang" => {
                    args.next().context("--lang requires en, zh-CN or zh-TW")?;
                }
                "--state-dir" => {
                    home = PathBuf::from(args.next().context("--state-dir requires a path")?)
                }
                "--config-dir" => {
                    config_home =
                        PathBuf::from(args.next().context("--config-dir requires a path")?)
                }
                "--bin-dir" => {
                    bin = PathBuf::from(args.next().context("--bin-dir requires a path")?);
                    explicit_bin = true;
                }
                "--view" => {
                    launch.view = Some(windows::View::parse(
                        &args.next().context("--view requires a page")?,
                    )?)
                }
                "--instance" => {
                    launch.instance = Some(args.next().context("--instance requires a name")?)
                }
                "--core" => {
                    launch.kind = Some(
                        match args
                            .next()
                            .context("--core requires proxy or rule")?
                            .as_str()
                        {
                            "proxy" => protocol::Kind::Proxy,
                            "rule" => protocol::Kind::Rule,
                            "server" => protocol::Kind::Server,
                            _ => anyhow::bail!("--core requires proxy, rule or server"),
                        },
                    )
                }
                "--window-id" => {
                    launch.window_id = Some(args.next().context("--window-id requires an ID")?)
                }
                "--native-console" => native_console = true,
                "--terminal-child" => terminal_child = true,
                "--terminal-native" => terminal_native = true,
                "--here" => here = true,
                "--resume-ui" => resume_ui = true,
                "--restart-ready" => {
                    restart_ready = Some(PathBuf::from(
                        args.next().context("--restart-ready requires a path")?,
                    ));
                }
                "--silent" => silent = true,
                "--manager" => daemon = true,
                "--shutdown" => shutdown = true,
                "update" => {
                    let package = args
                        .next()
                        .context("update requires a local package path")?;
                    set_operation(
                        &mut operation,
                        protocol::Action::UpdatePackage(PathBuf::from(package)),
                    )?;
                }
                "update-status" => set_operation(&mut operation, protocol::Action::UpdateStatus)?,
                "rollback" => set_operation(&mut operation, protocol::Action::Rollback)?,
                "--help" | "-h" => {
                    help = true;
                    break;
                }
                _ => anyhow::bail!("Unknown argument: {arg}"),
            }
        }
        anyhow::ensure!(
            !(terminal_child && terminal_native)
                && (!(terminal_child || terminal_native)
                    || (!silent && !daemon && !shutdown && operation.is_none())),
            "Terminal host flags require an interactive frontend and cannot be combined"
        );
        anyhow::ensure!(
            !here || (!silent && !daemon && !shutdown && operation.is_none()),
            "--here requires an interactive frontend"
        );
        anyhow::ensure!(
            !here || cfg!(not(windows)),
            "--here is not supported on Windows; use --native-console"
        );
        anyhow::ensure!(
            restart_ready.is_none() || resume_ui,
            "--restart-ready requires --resume-ui"
        );
        anyhow::ensure!(
            !resume_ui || (!silent && !daemon && !shutdown && operation.is_none()),
            "--resume-ui requires an interactive frontend"
        );
        validate_silent_launch(
            silent,
            daemon,
            shutdown,
            operation.is_some(),
            native_console,
            &launch,
        )?;
        anyhow::ensure!(
            !shutdown || operation.is_none(),
            "--shutdown cannot be combined with an update command"
        );
        if let Some(protocol::Action::UpdatePackage(path)) = &mut operation {
            *path = std::fs::canonicalize(&*path)
                .with_context(|| format!("Cannot open update package {}", path.display()))?;
        }
        // Terminal handoffs already carry the parsed view and window identity.
        // In particular the server's default Profiles view remains a primary
        // window so it still runs configured instance startup exactly once.
        finish_launch(
            &mut launch,
            default_kind,
            resume_ui || terminal_child || terminal_native,
        );
        #[cfg(not(windows))]
        let _ = native_console;
        Ok(Self {
            executable,
            home,
            config_home,
            bin,
            explicit_bin,
            daemon,
            shutdown,
            endpoint,
            agent_id,
            operation,
            launch,
            native_console,
            terminal_child,
            terminal_native,
            here,
            resume_ui,
            restart_ready,
            silent,
            help,
        })
    }
}

pub async fn ensure_manager(
    socket: &Path,
    manager_binary: &Path,
    context: &windows::Context,
    silent: bool,
) -> Result<()> {
    if protocol::call(socket, protocol::Action::List)
        .await
        .is_err()
    {
        anyhow::ensure!(
            manager_binary != context.executable,
            "The separate astra-host executable is required; install a complete package"
        );
        if socket.exists() {
            astra_platform::platform::recover_stale_manager_endpoint(socket)
                .with_context(|| format!("Manager endpoint is unavailable and could not be safely recovered: {}. Inspect {} and running cores", socket.display(), context.home.join("manager.log").display()))?;
        }
        anyhow::ensure!(!socket.exists(), "Manager socket is present but unavailable. Inspect manager.log and running cores before removing a stale socket.");
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(context.home.join("manager.log"))?;
        let mut command = Command::new(manager_binary);
        command
            .arg("--manager")
            .arg("--state-dir")
            .arg(&context.home)
            .arg("--config-dir")
            .arg(&context.config)
            .arg("--bin-dir")
            .arg(&context.bin)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        configure_manager_background(&mut command, silent);
        let mut child = command.spawn()?;
        let mut ready = false;
        // Poll quickly at first: the manager normally binds within a few ms.
        for attempt in 0..120 {
            if protocol::call(socket, protocol::Action::List).await.is_ok() {
                ready = true;
                break;
            }
            if let Some(status) = child.try_wait()? {
                anyhow::bail!(
                    "Manager exited ({status}); see {}",
                    context.home.join("manager.log").display()
                );
            }
            tokio::time::sleep(Duration::from_millis(if attempt < 20 { 10 } else { 50 })).await;
        }
        anyhow::ensure!(ready, "Manager did not become ready; inspect manager.log");
    }
    let storage = protocol::call(socket, protocol::Action::StoragePaths).await
        .context("The running manager does not support separate configuration storage. Exit it and restart the updated astra-tui")?;
    let storage: protocol::StoragePaths = serde_json::from_str(&storage.text)?;
    anyhow::ensure!(storage.config == context.config && storage.state == context.home,
        "The manager for this state directory is using configuration directory {}. Use that --config-dir or a separate --state-dir", storage.config.display());
    Ok(())
}

pub struct Prepared {
    pub socket: PathBuf,
    pub context: windows::Context,
    pub launch: windows::Launch,
    pub silent: bool,
}

/// Resolve storage and connect or launch the independent manager. Call from a
/// Tokio backend thread, never from a native UI event callback.
pub async fn prepare(options: &Options) -> Result<Prepared> {
    astra_platform::storage::private_dir(&options.home)?;
    let home = std::fs::canonicalize(&options.home)?;
    if let Some(path) = &options.restart_ready {
        validate_restart_ready(&home, path)?;
    }
    anyhow::ensure!(
        options.agent_id.is_none() || options.endpoint.is_some(),
        "--agent requires --endpoint"
    );
    let mut context = windows::Context {
        executable: options.executable.clone(),
        home,
        config: std::path::absolute(&options.config_home)?,
        bin: std::path::absolute(&options.bin)?,
        language: i18n::language(),
    };
    let socket = if let Some(endpoint) = &options.endpoint {
        anyhow::ensure!(!options.daemon, "--endpoint cannot start a manager");
        let endpoint = std::fs::canonicalize(endpoint).context(
            "Manager/controller endpoint does not exist; start its independent service first",
        )?;
        protocol::configure(endpoint.clone(), options.agent_id.clone());
        let hello = protocol::request(
            &endpoint,
            astra_management::v2::RequestBody::Query(astra_management::v2::Query::Hello),
        )
        .await?;
        if let astra_management::v2::ResponseBody::Hello(hello) = hello {
            anyhow::ensure!(
                hello.role == "agent",
                "Select a registered node with --agent ID when connecting to a controller"
            );
        }
        endpoint
    } else {
        astra_platform::storage::private_dir(&context.config)?;
        context.config = std::fs::canonicalize(&context.config)?;
        anyhow::ensure!(
            context.config != context.home
                && !context.config.starts_with(&context.home)
                && !context.home.starts_with(&context.config),
            "Configuration and state directories must be separate, non-nested directories"
        );
        let mut manager = manager_executable(&options.executable);
        if !options.shutdown {
            if let Some((directory, host)) =
                selected_startup_release(&context.home, options.explicit_bin)?
            {
                context.bin = directory;
                manager = host;
            }
        }
        // Explicit binary directories also make development launches useful
        // when the UI and manager were built into different target folders.
        let supplied_host = context
            .bin
            .join(astra_platform::platform::executable("astra-host"));
        if options.explicit_bin && supplied_host.is_file() {
            manager = supplied_host;
        }
        context.bin = std::fs::canonicalize(&context.bin)?;
        let socket = context.home.join("management.sock");
        astra_platform::platform::validate_state_directory(&context.home)?;
        if !options.shutdown {
            ensure_manager(&socket, &manager, &context, options.silent).await?;
        }
        socket
    };
    Ok(Prepared {
        socket,
        context,
        launch: options.launch.clone(),
        silent: options.silent,
    })
}

/// Return output for a headless command, or None for an interactive launch.
pub async fn run_maintenance(prepared: &Prepared, options: &Options) -> Result<Option<String>> {
    let action = if options.shutdown {
        Some(protocol::Action::Shutdown)
    } else {
        options.operation.clone()
    };
    if let Some(action) = action {
        return Ok(Some(protocol::call(&prepared.socket, action).await?.text));
    }
    if options.silent {
        crate::startup_instances::run_silent(prepared.socket.clone(), prepared.context.clone())
            .await?;
        return Ok(Some(String::new()));
    }
    if options.daemon {
        return Ok(Some(String::new()));
    }
    Ok(None)
}

#[cfg(test)]
#[path = "test/bootstrap_test.rs"]
mod tests;
