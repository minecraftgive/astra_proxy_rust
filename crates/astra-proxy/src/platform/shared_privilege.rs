//! Privilege handling for standalone TUN launches.
use anyhow::Result;
use std::path::Path;

pub fn bootstrap() -> Result<Option<String>> {
    #[cfg(windows)]
    return astra_ipc::elevation::bootstrap()?
        .map(|path| {
            path.into_string()
                .map_err(|_| anyhow::anyhow!("configuration path must be UTF-8"))
        })
        .transpose();
    #[cfg(not(windows))]
    Ok(None)
}

#[cfg(target_os = "linux")]
fn has_net_admin() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find_map(|line| line.strip_prefix("CapEff:\t"))
        })
        .and_then(|hex| u64::from_str_radix(hex.trim(), 16).ok())
        .is_some_and(|caps| caps & (1 << 12) != 0)
}

#[cfg(not(target_os = "linux"))]
fn has_net_admin() -> bool {
    false
}

#[cfg(unix)]
fn has_tun_privilege() -> bool {
    (unsafe { libc::geteuid() } == 0) || (cfg!(target_os = "linux") && has_net_admin())
}

pub async fn delegate(path: &Path, tun_enabled: bool) -> Result<bool> {
    #[cfg(unix)]
    if tun_enabled && !has_tun_privilege() {
        anyhow::bail!(
            "TUN requires root or CAP_NET_ADMIN; restart Astra with sudo in your terminal. Astra never reads the sudo password."
        );
    }
    #[cfg(not(windows))]
    let _ = (path, tun_enabled);
    #[cfg(windows)]
    {
        use std::time::Duration;
        if tun_enabled && !astra_ipc::elevation::is_elevated()? {
            let path = std::fs::canonicalize(path)?;
            // A separate elevated core keeps the calling shell/launcher unprivileged.
            let log = astra_ipc::storage::runtime_path(&path, "elevated-logs")?;
            astra_ipc::storage::prepare_parent(&log)?;
            let socket = std::env::var_os("ASTRA_CONTROL_SOCKET")
                .map(std::path::absolute)
                .transpose()?
                .unwrap_or(astra_ipc::storage::runtime_path(&path, "elevated-control")?);
            astra_ipc::storage::prepare_parent(&socket)?;
            let level = std::env::var("ASTRA_LOG_LEVEL")
                .or_else(|_| std::env::var("RUST_LOG"))
                .unwrap_or_else(|_| "info".into());
            let arguments =
                astra_ipc::elevation::core_arguments(&path, Some(&socket), &log, &level, None)?;
            tracing::info!(log=%log.display(), "Windows TUN requests administrator permission for a separate proxy core");
            let mut child = tokio::task::spawn_blocking(move || {
                astra_ipc::elevation::spawn(&std::env::current_exe()?, &arguments)
            })
            .await??;
            let status = tokio::select! {
                status = child.wait() => status?,
                signal = tokio::signal::ctrl_c() => {
                    signal?;
                    let status = astra_ipc::control::request(&socket, astra_ipc::control::Command::Status).await;
                    if status.is_ok_and(|status| Some(status.pid) == child.id()) {
                        let _ = astra_ipc::control::request(&socket, astra_ipc::control::Command::Stop).await;
                    }
                    if let Ok(status) = tokio::time::timeout(Duration::from_secs(30), child.wait()).await {
                        status?;
                        return Ok(true);
                    }
                    child.kill().await?;
                    return Ok(true);
                }
            };
            anyhow::ensure!(
                status.success(),
                "Elevated TUN proxy exited ({status}); see {}",
                log.display()
            );
            return Ok(true);
        }
    }
    Ok(false)
}
