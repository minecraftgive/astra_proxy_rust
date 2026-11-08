//! The optional gateway is supervised independently of every worker process.
use anyhow::{Context, Result};
use astra_management::config::ManagerConfig;
use std::{path::PathBuf, process::Stdio, time::Duration};

pub struct Supervisor(tokio::task::JoinHandle<()>);
impl Drop for Supervisor {
    fn drop(&mut self) {
        self.0.abort();
    }
}
pub fn supervise(config: &ManagerConfig) -> Result<Option<Supervisor>> {
    if !config.web.enabled {
        return Ok(None);
    }
    let binary = config.web.binary.clone().unwrap_or_else(|| {
        config
            .bin_dir
            .join(astra_platform::platform::executable("astra-web"))
    });
    anyhow::ensure!(
        binary.is_file(),
        "web.enabled requires the independent astra-web component: {}",
        binary.display()
    );
    let state = config
        .web
        .state_dir
        .clone()
        .unwrap_or_else(|| config.state_dir.join("web"));
    crate::private_dir(&state)?;
    let state = std::fs::canonicalize(state)?;
    let file = config.state_dir.join("web-gateway.toml");
    let mut gateway = toml::Table::new();
    gateway.insert(
        "manager_socket".into(),
        config.endpoint().to_string_lossy().to_string().into(),
    );
    gateway.insert(
        "state_dir".into(),
        state.to_string_lossy().to_string().into(),
    );
    gateway.insert("listen".into(), config.web.listen.to_string().into());
    gateway.insert(
        "public_origin".into(),
        config
            .web
            .public_origin
            .clone()
            .context("web.public_origin")?
            .into(),
    );
    gateway.insert("dev_http".into(), config.web.dev_http.into());
    if let Some(path) = &config.web.credentials_file {
        gateway.insert(
            "credentials_file".into(),
            path.to_string_lossy().to_string().into(),
        );
    }
    if let Some(tls) = &config.web.tls {
        gateway.insert("tls".into(), toml::Value::try_from(tls)?);
    }
    crate::atomic_write(&file, toml::to_string(&gateway)?.as_bytes())?;
    Ok(Some(Supervisor(tokio::spawn(async move {
        let mut delay = 1;
        loop {
            let started = std::time::Instant::now();
            let result = run_child(&binary, &file, &state).await;
            eprintln!("Independent web gateway stopped: {result:?}; retry in {delay}s (workers remain running)");
            tokio::time::sleep(Duration::from_secs(delay)).await;
            delay = if started.elapsed() > Duration::from_secs(60) {
                1
            } else {
                (delay * 2).min(30)
            };
        }
    }))))
}
async fn run_child(
    binary: &PathBuf,
    config: &PathBuf,
    state: &std::path::Path,
) -> Result<std::process::ExitStatus> {
    use astra_platform::platform::OpenOptionsExt;
    let path = state.join("gateway.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 4 * 1024 * 1024) {
        let old = state.join("gateway.previous.log");
        if old.is_file() {
            std::fs::remove_file(&old)?;
        }
        std::fs::rename(&path, old)?;
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&path)?;
    astra_platform::platform::set_private(&path, 0o600)?;
    let mut command = tokio::process::Command::new(binary);
    command
        .args(["--config"])
        .arg(config)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .kill_on_drop(true);
    astra_platform::platform::background_command(command.as_std_mut(), false);
    let mut child = command.spawn().context("start independent web gateway")?;
    let process = astra_platform::lifecycle::capture(child.id().context("web gateway PID")?)?;
    crate::atomic_write(&state.join("process.json"), &serde_json::to_vec(&process)?)?;
    Ok(child.wait().await?)
}
