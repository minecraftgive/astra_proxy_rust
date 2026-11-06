use crate::{i18n, protocol, ui, windows};
use anyhow::{Context, Result};
use astra_platform::platform::OpenOptionsExt;
use std::{
    fs::OpenOptions,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

// Shared cadence for passive UI data, core subscriptions and log tailing.
pub(crate) const UI_REFRESH_INTERVAL: Duration = Duration::from_millis(500);

fn is_host_binary(executable: &Path) -> bool {
    executable
        .file_stem()
        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("astra-host"))
}

fn manager_executable(executable: &Path) -> PathBuf {
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

fn selected_startup_release(home: &Path, explicit_bin: bool) -> Result<Option<(PathBuf, PathBuf)>> {
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

fn redirect_host(
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

fn selected_frontend(
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

fn set_operation(operation: &mut Option<protocol::Action>, next: protocol::Action) -> Result<()> {
    anyhow::ensure!(operation.is_none(), "Specify only one update command");
    *operation = Some(next);
    Ok(())
}

fn finish_launch(
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

pub(crate) async fn run(default_kind: Option<protocol::Kind>) -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    // Decide before localization/argument validation can fail. Silent launches
    // must never allocate a console and must log early failures as well.
    let (mut silent, _daemon) = headless_options(&args);
    #[cfg(windows)]
    let daemon = _daemon || is_host_binary(&std::env::current_exe()?);
    #[cfg(windows)]
    let console = if !silent && !daemon {
        // CLI help/errors can use the invoking shell, preserving redirected
        // output. An interactive UI gets its own console after validation.
        windows::prepare_console(windows::ConsoleMode::CommandLine)
    } else {
        Ok(())
    };
    #[cfg(not(windows))]
    let console: Result<()> = Ok(());
    let result = match console {
        Ok(()) => run_inner(default_kind, &mut silent).await,
        Err(error) => Err(error),
    };
    if silent {
        if let Err(error) = &result {
            let home = args
                .windows(2)
                .rev()
                .find(|pair| pair[0] == "--state-dir")
                .map(|pair| PathBuf::from(&pair[1]))
                .or_else(|| {
                    astra_platform::platform::home_dir()
                        .ok()
                        .map(|p| p.join(".astra/tui"))
                });
            if let Some(home) = home {
                let _ = ui::log_startup(&home, &format!("Startup failed: {error:#}"));
            }
        }
    }
    #[cfg(windows)]
    if !silent && !daemon {
        if let Err(error) = &result {
            // A double-clicked GUI frontend may fail before it opens the TUI.
            // Keep such errors visible without changing redirected CLI errors.
            windows::report_startup_error(&format!("{error:#}"));
        }
    }
    result
}

fn headless_options(args: &[std::ffi::OsString]) -> (bool, bool) {
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

#[cfg(windows)]
fn prepare_interactive_console(
    native_console: bool,
    terminal_child: bool,
    launch: &windows::Launch,
) -> Result<()> {
    windows::prepare_console(if terminal_child {
        windows::ConsoleMode::Hosted
    } else if native_console {
        windows::ConsoleMode::NewWindow
    } else {
        windows::ConsoleMode::Interactive
    })?;
    crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::SetTitle(format!(
            "Astra · {}",
            launch.view.unwrap_or(windows::View::Dashboard).name()
        ))
    )?;
    Ok(())
}

fn should_launch_terminal(
    hosted: bool,
    native: bool,
    resume: bool,
    preference: &str,
    tty: bool,
) -> bool {
    !hosted && !native && !resume && (cfg!(windows) || preference != "auto" || !tty)
}

fn terminal_arguments(
    context: &windows::Context,
    launch: &windows::Launch,
    explicit_language: bool,
) -> Result<Vec<std::ffi::OsString>> {
    let mut context = context.clone();
    // Terminal profiles can change the working directory of the new process.
    context.config = std::path::absolute(&context.config)?;
    context.bin = std::path::absolute(&context.bin)?;
    let mut args = context.arguments(launch);
    if !explicit_language {
        // The main window has not applied its saved language yet. Let the
        // hosted UI load it, instead of turning the system locale into a CLI
        // override. New windows already know the active UI language.
        if let Some(index) = args.iter().position(|arg| arg == "--lang") {
            args.drain(index..index + 2);
        }
    }
    Ok(args)
}

async fn launch_terminal(
    context: &windows::Context,
    launch: &windows::Launch,
    terminal_child: bool,
    terminal_native: bool,
    resume_ui: bool,
) -> Result<bool> {
    use std::io::IsTerminal;
    if terminal_child || terminal_native || resume_ui {
        return Ok(false);
    }
    let tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let args = terminal_arguments(context, launch, i18n::command_line_language())?;
    let context = context.clone();
    tokio::task::spawn_blocking(move || {
        let preference = ui::preferred_terminal(&context.home);
        if !should_launch_terminal(terminal_child, terminal_native, resume_ui, &preference, tty) {
            return Ok(false);
        }
        match crate::terminals::launch(&preference, &context.executable, &args)? {
            crate::terminals::LaunchOutcome::Native => Ok(false),
            crate::terminals::LaunchOutcome::Launched { terminal } => {
                if let Ok(log) = astra_platform::tui_log::Log::open(&context.home.join("tui.log")) {
                    log.write("terminal", &format!("opened {terminal}"));
                }
                Ok(true)
            }
        }
    })
    .await
    .context("Choose terminal")?
}

fn validate_silent_launch(
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

/// A headless launcher may exit as soon as the manager is ready. On Unix the
/// manager needs its own session, without the launcher's controlling terminal;
/// its standard handles are already null/log files and std::process::Child
/// deliberately does not kill it when this frontend drops the handle.
fn configure_manager_background(command: &mut Command, silent: bool) {
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

async fn run_inner(default_kind: Option<protocol::Kind>, silent: &mut bool) -> Result<()> {
    i18n::init()?;
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
    let mut resume_ui = false;
    let mut restart_ready = None;
    let mut args = std::env::args().skip(1);
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
                config_home = PathBuf::from(args.next().context("--config-dir requires a path")?)
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
            "--resume-ui" => resume_ui = true,
            "--restart-ready" => {
                restart_ready = Some(PathBuf::from(
                    args.next().context("--restart-ready requires a path")?,
                ));
            }
            "--silent" => *silent = true,
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
                let program = if is_host_binary(&executable) {
                    "astra-host"
                } else {
                    "astra-tui"
                };
                println!("{program} [--lang en|zh-CN|zh-TW] [--state-dir DIR] [--config-dir DIR] [--bin-dir DIR]\n{}", i18n::t("--manager: run manager · --shutdown: stop all\nq: close UI; instances keep running"));
                println!("update PATH: apply a local update package\nupdate-status: show release and drain status\nrollback: return new sessions to the previous release\nastra-host runs the background manager by default; host updates take effect after an intentional restart");
                println!("--endpoint PATH [--agent ID]: connect to a trusted local manager/controller IPC endpoint; remote nodes are selected by ID");
                #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
                println!("--silent: start the manager and configured auto-start instances without opening the TUI; errors are saved to STATE_DIR/startup.log");
                println!("--view overview|proxies|connections|rules|logs|profiles|settings|tui-log|diagnostics [--instance NAME] [--core proxy|rule] [--window-id ID]\nF8: current page in a new window · Alt+1–7: page in a new window");
                return Ok(());
            }
            _ => anyhow::bail!("Unknown argument: {arg}"),
        }
    }
    let silent = *silent;
    anyhow::ensure!(
        !(terminal_child && terminal_native)
            && (!(terminal_child || terminal_native)
                || (!silent && !daemon && !shutdown && operation.is_none())),
        "Terminal host flags require an interactive frontend and cannot be combined"
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
    astra_platform::storage::private_dir(&home)?;
    home = std::fs::canonicalize(home)?;
    if let Some(path) = &restart_ready {
        validate_restart_ready(&home, path)?;
    }
    anyhow::ensure!(
        agent_id.is_none() || endpoint.is_some(),
        "--agent requires --endpoint"
    );
    if let Some(endpoint) = endpoint {
        anyhow::ensure!(!daemon, "--endpoint cannot start a manager");
        let endpoint = std::fs::canonicalize(endpoint).context(
            "Manager/controller endpoint does not exist; start its independent service first",
        )?;
        protocol::configure(endpoint.clone(), agent_id);
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
        if shutdown {
            println!(
                "{}",
                protocol::call(&endpoint, protocol::Action::Shutdown)
                    .await?
                    .text
            );
            return Ok(());
        }
        if let Some(action) = operation {
            println!("{}", protocol::call(&endpoint, action).await?.text);
            return Ok(());
        }
        let context = windows::Context {
            executable,
            home,
            config: std::path::absolute(config_home)?,
            bin: std::path::absolute(bin)?,
            language: i18n::language(),
        };
        return if silent {
            ui::run_silent(endpoint, context).await
        } else {
            if launch_terminal(
                &context,
                &launch,
                terminal_child,
                terminal_native,
                resume_ui,
            )
            .await?
            {
                return Ok(());
            }
            #[cfg(windows)]
            prepare_interactive_console(native_console, terminal_child, &launch)?;
            ui::run(endpoint, context, launch, resume_ui, restart_ready).await
        };
    }
    astra_platform::storage::private_dir(&config_home)?;
    config_home = std::fs::canonicalize(config_home)?;
    anyhow::ensure!(
        config_home != home && !config_home.starts_with(&home) && !home.starts_with(&config_home),
        "Configuration and state directories must be separate, non-nested directories"
    );
    let selected = if shutdown {
        None
    } else {
        let home = home.clone();
        tokio::task::spawn_blocking(move || selected_startup_release(&home, explicit_bin)).await??
    };
    let has_selected_release = selected.is_some();
    let manager_binary = if let Some((directory, host)) = selected {
        bin = directory;
        host
    } else {
        manager_executable(&executable)
    };
    bin = std::fs::canonicalize(bin)?;
    let socket = home.join("management.sock");
    astra_platform::platform::validate_state_directory(&home)?;
    if !silent && !daemon && !shutdown && operation.is_none() {
        if let Some(frontend) =
            selected_frontend(&executable, &bin, has_selected_release, explicit_bin)?
        {
            // A frontend can reconnect to an existing manager without replacing it.
            let status = tokio::process::Command::new(frontend)
                .args(std::env::args_os().skip(1))
                .arg("--bin-dir")
                .arg(&bin)
                .status()
                .await
                .context("Start the selected interface")?;
            anyhow::ensure!(
                status.success(),
                "The selected interface exited with {status}"
            );
            return Ok(());
        }
    }
    if daemon && !shutdown && operation.is_none() {
        if redirect_host(&executable, &manager_binary, explicit_bin, &socket)? {
            let status = tokio::process::Command::new(&manager_binary)
                .arg("--manager")
                .arg("--state-dir")
                .arg(&home)
                .arg("--config-dir")
                .arg(&config_home)
                .arg("--bin-dir")
                .arg(&bin)
                .arg("--lang")
                .arg(match i18n::language() {
                    i18n::Language::English => "en",
                    i18n::Language::Simplified => "zh-CN",
                    i18n::Language::Traditional => "zh-TW",
                })
                .status()
                .await
                .context("Start the selected resident host")?;
            anyhow::ensure!(
                status.success(),
                "The selected resident host exited with {status}"
            );
            return Ok(());
        }
        anyhow::ensure!(
            manager_binary != executable,
            "The separate astra-host executable is required; install a complete package"
        );
        let status = tokio::process::Command::new(&manager_binary)
            .arg("--manager")
            .arg("--state-dir")
            .arg(&home)
            .arg("--config-dir")
            .arg(&config_home)
            .arg("--bin-dir")
            .arg(&bin)
            .status()
            .await
            .context("Start the independent manager")?;
        anyhow::ensure!(status.success(), "Manager exited with {status}");
        return Ok(());
    }
    if shutdown {
        let reply = protocol::call(&socket, protocol::Action::Shutdown).await?;
        println!("{}", i18n::t(&reply.text));
        return Ok(());
    }
    if protocol::call(&socket, protocol::Action::List)
        .await
        .is_err()
    {
        anyhow::ensure!(
            manager_binary != executable,
            "The separate astra-host executable is required; install a complete package"
        );
        if socket.exists() {
            astra_platform::platform::recover_stale_manager_endpoint(&socket)
                .with_context(|| format!("Manager endpoint is unavailable and could not be safely recovered: {}. Inspect {} and running cores", socket.display(), home.join("manager.log").display()))?;
        }
        anyhow::ensure!(!socket.exists(), "Manager socket is present but unavailable. Inspect manager.log and running cores before removing a stale socket.");
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(home.join("manager.log"))?;
        let mut command = Command::new(&manager_binary);
        command
            .arg("--manager")
            .arg("--state-dir")
            .arg(&home)
            .arg("--config-dir")
            .arg(&config_home)
            .arg("--bin-dir")
            .arg(&bin)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        configure_manager_background(&mut command, silent);
        let mut child = command.spawn()?;
        let mut ready = false;
        // Poll quickly at first: the manager normally binds within a few ms.
        for attempt in 0..120 {
            if protocol::call(&socket, protocol::Action::List)
                .await
                .is_ok()
            {
                ready = true;
                break;
            }
            if let Some(status) = child.try_wait()? {
                anyhow::bail!(
                    "Manager exited ({status}); see {}",
                    home.join("manager.log").display()
                );
            }
            tokio::time::sleep(Duration::from_millis(if attempt < 20 { 10 } else { 50 })).await;
        }
        anyhow::ensure!(ready, "Manager did not become ready; inspect manager.log");
    }
    let storage = protocol::call(&socket, protocol::Action::StoragePaths).await
        .context("The running manager does not support separate configuration storage. Exit it and restart the updated astra-tui")?;
    let storage: protocol::StoragePaths = serde_json::from_str(&storage.text)?;
    anyhow::ensure!(storage.config == config_home && storage.state == home,
        "The manager for this state directory is using configuration directory {}. Use that --config-dir or a separate --state-dir", storage.config.display());
    if let Some(action) = operation {
        let reply = protocol::call(&socket, action).await?;
        println!("{}", reply.text);
        return Ok(());
    }
    let context = windows::Context {
        executable,
        home,
        config: config_home,
        bin,
        language: i18n::language(),
    };
    if silent {
        ui::run_silent(socket, context).await
    } else {
        if launch_terminal(
            &context,
            &launch,
            terminal_child,
            terminal_native,
            resume_ui,
        )
        .await?
        {
            return Ok(());
        }
        // Allocate only in the frontend that actually displays the UI. A
        // versioned frontend handoff above must not leave an extra empty window.
        #[cfg(windows)]
        prepare_interactive_console(native_console, terminal_child, &launch)?;
        ui::run(socket, context, launch, resume_ui, restart_ready).await
    }
}

fn validate_restart_ready(home: &Path, path: &Path) -> Result<()> {
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

#[cfg(test)]
#[path = "test/entry_test.rs"]
mod tests;
