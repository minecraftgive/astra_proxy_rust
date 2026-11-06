//! Privilege handling for standalone TUN launches.
use anyhow::Result;
use std::path::Path;

pub fn bootstrap() -> Result<Option<String>> {
    #[cfg(windows)]
    return astra_platform::elevation::bootstrap()?
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
                .and_then(|hex| u64::from_str_radix(hex.trim(), 16).ok())
        })
        .is_some_and(|caps| caps & (1 << 12) != 0)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn has_net_admin() -> bool {
    false
}

#[cfg(unix)]
fn has_tun_privilege() -> bool {
    (unsafe { libc::geteuid() } == 0) || (cfg!(target_os = "linux") && has_net_admin())
}

#[cfg(windows)]
fn elevated_arguments(
    path: &Path,
    socket: &Path,
    log: &Path,
    level: &str,
    runtime_root: &Path,
) -> Result<Vec<std::ffi::OsString>> {
    let mut arguments = astra_platform::elevation::core_arguments_in(
        path,
        Some(socket),
        log,
        level,
        None,
        runtime_root,
    )?;
    // The ordinary launcher remains alive while supervising the elevated core.
    // Hosted workers can use this exact process identity to recover its token.
    let owner = astra_platform::lifecycle::capture(std::process::id())?;
    arguments.push(serde_json::to_string(&owner)?.into());
    Ok(arguments)
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
        if tun_enabled && !astra_platform::elevation::is_elevated()? {
            let path = std::fs::canonicalize(path)?;
            // A separate elevated core keeps the calling shell/launcher unprivileged.
            let runtime_root = std::path::absolute(astra_platform::storage::runtime_root()?)?;
            let log =
                astra_platform::storage::runtime_path_in(&runtime_root, &path, "elevated-logs")?;
            astra_platform::storage::prepare_parent(&log)?;
            let socket = std::env::var_os("ASTRA_CONTROL_SOCKET")
                .map(std::path::absolute)
                .transpose()?
                .unwrap_or(astra_platform::storage::runtime_path_in(
                    &runtime_root,
                    &path,
                    "elevated-control",
                )?);
            astra_platform::storage::prepare_parent(&socket)?;
            let level = std::env::var("ASTRA_LOG_LEVEL")
                .or_else(|_| std::env::var("RUST_LOG"))
                .unwrap_or_else(|_| "info".into());
            let arguments = elevated_arguments(&path, &socket, &log, &level, &runtime_root)?;
            let launch_log = log.clone();
            let mut child = tokio::task::spawn_blocking(move || {
                use astra_platform::elevation::LaunchMethod;
                let method = LaunchMethod::preferred(true)?;
                match method {
                    LaunchMethod::Service => tracing::info!(log=%launch_log.display(), "Windows TUN starts a separate proxy core through the registered privilege service"),
                    LaunchMethod::Uac => tracing::info!(log=%launch_log.display(), "Windows TUN requests administrator permission for a separate proxy core"),
                }
                method.spawn(&std::env::current_exe()?, &arguments)
            })
            .await??;
            let status = tokio::select! {
                status = child.wait() => status?,
                signal = tokio::signal::ctrl_c() => {
                    signal?;
                    let status = astra_ipc::control::request(&socket, astra_model::control::Command::Status).await;
                    if status.is_ok_and(|status| Some(status.pid) == child.id()) {
                        let _ = astra_ipc::control::request(&socket, astra_model::control::Command::Stop).await;
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

#[cfg(all(test, windows))]
#[path = "../test/platform_privilege_test.rs"]
mod tests;
