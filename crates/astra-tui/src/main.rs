use astra_ipc::platform::OpenOptionsExt;
mod availability;
mod diagnostics;
mod editor;
mod explanation;
mod external_editor;
mod help;
mod i18n;
mod import;
mod log_stream;
mod logs;
mod manager;
mod network_interfaces;
mod protocol;
mod subscription_view;
mod tui_log;
mod ui;
mod windows;
use anyhow::{Context, Result};
use std::{
    fs::OpenOptions,
    path::PathBuf,
    process::{Command, Stdio},
    time::Duration,
};

// Shared cadence for passive UI data, core subscriptions and log tailing.
const UI_REFRESH_INTERVAL: Duration = Duration::from_millis(500);

#[tokio::main]
async fn main() -> Result<()> {
    i18n::init()?;
    let mut home = astra_ipc::platform::home_dir()?.join(".astra/tui");
    let mut config_home = astra_ipc::platform::home_dir()?.join(".astra/config");
    let mut bin = std::env::current_exe()?
        .parent()
        .context("Missing executable directory")?
        .to_path_buf();
    let mut daemon = false;
    let mut shutdown = false;
    let mut launch = windows::Launch::default();
    let mut native_console = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--lang" => {
                args.next().context("--lang requires en, zh-CN or zh-TW")?;
            }
            "--state-dir" => {
                home = PathBuf::from(args.next().context("--state-dir requires a path")?)
            }
            "--config-dir" => {
                config_home = PathBuf::from(args.next().context("--config-dir requires a path")?)
            }
            "--bin-dir" => bin = PathBuf::from(args.next().context("--bin-dir requires a path")?),
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
                        _ => anyhow::bail!("--core requires proxy or rule"),
                    },
                )
            }
            "--window-id" => {
                launch.window_id = Some(args.next().context("--window-id requires an ID")?)
            }
            "--native-console" => native_console = true,
            "--manager" => daemon = true,
            "--shutdown" => shutdown = true,
            "--help" | "-h" => {
                println!("astra-tui [--lang en|zh-CN|zh-TW] [--state-dir DIR] [--config-dir DIR] [--bin-dir DIR]\n{}", i18n::t("--manager: run manager · --shutdown: stop all\nq: close UI; instances keep running"));
                println!("--view dashboard|proxies|profiles|connections|tools|logs|tui-log|diagnostics [--instance NAME] [--core proxy|rule] [--window-id ID]\nF8: current page in a new window · Alt+1–5: page in a new window");
                return Ok(());
            }
            _ => anyhow::bail!("Unknown argument: {arg}"),
        }
    }
    #[cfg(windows)]
    if native_console {
        windows::attach_console_io()?;
        crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::SetTitle(format!(
                "Astra · {}",
                launch.view.unwrap_or(windows::View::Dashboard).name()
            ))
        )?;
    }
    #[cfg(not(windows))]
    let _ = native_console;
    if launch.view.is_some() && launch.window_id.is_none() {
        launch.window_id =
            windows::Launch::detached(launch.view.unwrap(), None, protocol::Kind::Proxy).window_id;
    }
    manager::private_dir(&home)?;
    home = std::fs::canonicalize(home)?;
    manager::private_dir(&config_home)?;
    config_home = std::fs::canonicalize(config_home)?;
    anyhow::ensure!(
        config_home != home && !config_home.starts_with(&home) && !home.starts_with(&config_home),
        "Configuration and state directories must be separate, non-nested directories"
    );
    bin = std::fs::canonicalize(bin)?;
    let socket = home.join("manager.sock");
    astra_ipc::platform::validate_state_directory(&home)?;
    if daemon {
        return manager::serve_with_config(home, config_home, bin).await;
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
        if socket.exists() {
            astra_ipc::platform::recover_stale_manager_endpoint(&socket)
                .with_context(|| format!("Manager endpoint is unavailable and could not be safely recovered: {}. Inspect {} and running cores", socket.display(), home.join("manager.log").display()))?;
        }
        anyhow::ensure!(!socket.exists(), "Manager socket is present but unavailable. Inspect manager.log and running cores before removing a stale socket.");
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(home.join("manager.log"))?;
        let mut command = Command::new(std::env::current_exe()?);
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
        astra_ipc::platform::background_command(&mut command, true);
        let mut child = command.spawn()?;
        let mut ready = false;
        for _ in 0..50 {
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
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        anyhow::ensure!(ready, "Manager did not become ready; inspect manager.log");
    }
    let storage = protocol::call(&socket, protocol::Action::StoragePaths).await
        .context("The running manager does not support separate configuration storage. Exit it and restart the updated astra-tui")?;
    let storage: protocol::StoragePaths = serde_json::from_str(&storage.text)?;
    anyhow::ensure!(storage.config == config_home && storage.state == home,
        "The manager for this state directory is using configuration directory {}. Use that --config-dir or a separate --state-dir", storage.config.display());
    let context = windows::Context {
        executable: std::env::current_exe()?,
        home,
        config: config_home,
        bin,
        language: i18n::language(),
    };
    ui::run(socket, context, launch).await
}
