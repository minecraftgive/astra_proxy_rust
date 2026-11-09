use crate::{i18n, protocol, ui, windows};
use anyhow::{Context, Result};
#[cfg(test)]
use astra_client::bootstrap::{
    configure_manager_background, finish_launch, validate_silent_launch,
};
use astra_client::bootstrap::{
    headless_options, is_host_binary, manager_executable, redirect_host, selected_frontend,
    selected_startup_release, validate_restart_ready,
};
#[cfg(test)]
use std::{
    path::Path,
    process::{Command, Stdio},
};
use std::{path::PathBuf, time::Duration};

// Shared cadence for passive UI data, core subscriptions and log tailing.
pub(crate) const UI_REFRESH_INTERVAL: Duration = Duration::from_millis(500);

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
    here: bool,
    preference: &str,
    tty: bool,
) -> bool {
    !hosted && !native && !resume && !here && (cfg!(windows) || preference != "auto" || !tty)
}

/// A remote shell cannot see windows opened on the host's desktop, so an
/// interactive SSH session always keeps the TUI in its own terminal.
fn remote_session(tty: bool) -> bool {
    !cfg!(windows)
        && tty
        && ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
            .iter()
            .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
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
    here: bool,
    resume_ui: bool,
) -> Result<bool> {
    use std::io::IsTerminal;
    if terminal_child || terminal_native || resume_ui {
        return Ok(false);
    }
    let tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    anyhow::ensure!(!here || tty, "--here requires an interactive terminal");
    let here = here || remote_session(tty);
    let args = terminal_arguments(context, launch, i18n::command_line_language())?;
    let context = context.clone();
    tokio::task::spawn_blocking(move || {
        let preference = ui::preferred_terminal(&context.home);
        if !should_launch_terminal(
            terminal_child,
            terminal_native,
            resume_ui,
            here,
            &preference,
            tty,
        ) {
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

async fn run_inner(default_kind: Option<protocol::Kind>, silent: &mut bool) -> Result<()> {
    i18n::init()?;
    let options = astra_client::bootstrap::Options::parse(default_kind, std::env::args().skip(1))?;
    *silent = options.silent;
    let astra_client::bootstrap::Options {
        executable,
        mut home,
        mut config_home,
        mut bin,
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
    } = options;
    #[cfg(not(windows))]
    let _ = native_console;
    if help {
        let program = if is_host_binary(&executable) {
            "astra-host"
        } else {
            "astra-tui"
        };
        println!("{program} [--lang en|zh-CN|zh-TW] [--state-dir DIR] [--config-dir DIR] [--bin-dir DIR]\n{}", i18n::t("--manager: run manager · --shutdown: stop all\nq: close UI; instances keep running"));
        println!("update PATH: apply a local update package\nupdate-status: show release and drain status\nrollback: return new sessions to the previous release\nastra-host runs the background manager by default; host updates take effect after an intentional restart");
        #[cfg(not(windows))]
        println!("--here: run the TUI in the current terminal instead of opening a terminal window (automatic over SSH)");
        println!("--endpoint PATH [--agent ID]: connect to a trusted local manager/controller IPC endpoint; remote nodes are selected by ID");
        #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
        println!("--silent: start the manager and configured auto-start instances without opening the TUI; errors are saved to STATE_DIR/startup.log");
        println!("--view overview|proxies|connections|rules|logs|profiles|settings|dns|tui-log|diagnostics [--instance NAME] [--core proxy|rule] [--window-id ID]\nF8: current page in a new window · Alt+1–8: page in a new window");

        return Ok(());
    }
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
                here,
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
    let socket = astra_client::maintenance::local_endpoint(&home)?;
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
    astra_client::bootstrap::ensure_manager(
        &socket,
        &manager_binary,
        &windows::Context {
            executable: executable.clone(),
            home: home.clone(),
            config: config_home.clone(),
            bin: bin.clone(),
            language: i18n::language(),
        },
        silent,
    )
    .await?;
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
            here,
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

#[cfg(test)]
#[path = "test/entry_test.rs"]
mod tests;
