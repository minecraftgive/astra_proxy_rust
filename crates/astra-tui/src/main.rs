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
mod protocol;
mod subscription_view;
mod ui;
use anyhow::{Context, Result};
use std::{
    fs::OpenOptions,
    path::PathBuf,
    process::{Command, Stdio},
    time::Duration,
};

#[tokio::main]
async fn main() -> Result<()> {
    i18n::init()?;
    let mut home = astra_ipc::platform::home_dir()?.join(".astra/tui");
    let mut bin = std::env::current_exe()?
        .parent()
        .context("Missing executable directory")?
        .to_path_buf();
    let mut daemon = false;
    let mut shutdown = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--lang" => {
                args.next().context("--lang requires en, zh-CN or zh-TW")?;
            }
            "--state-dir" => {
                home = PathBuf::from(args.next().context("--state-dir requires a path")?)
            }
            "--bin-dir" => bin = PathBuf::from(args.next().context("--bin-dir requires a path")?),
            "--manager" => daemon = true,
            "--shutdown" => shutdown = true,
            "--help" | "-h" => {
                println!("astra-tui [--lang en|zh-CN|zh-TW] [--state-dir DIR] [--bin-dir DIR]\n{}", i18n::t("--manager: run manager · --shutdown: stop all\nq: close UI; instances keep running"));
                return Ok(());
            }
            _ => anyhow::bail!("Unknown argument: {arg}"),
        }
    }
    manager::private_dir(&home)?;
    home = std::fs::canonicalize(home)?;
    bin = std::fs::canonicalize(bin)?;
    let socket = home.join("manager.sock");
    astra_ipc::platform::validate_state_directory(&home)?;
    if daemon {
        return manager::serve(home, bin).await;
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
    ui::run(socket).await
}
