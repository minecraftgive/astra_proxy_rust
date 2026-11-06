//! Switch only the frontend after a completed local release transaction.
use crate::{protocol::updates, windows};
use anyhow::{Context as _, Result};
use std::{path::Path, process::Command};
#[cfg(any(windows, test))]
use std::{
    path::PathBuf,
    process::{Child, Stdio},
    time::{Duration, Instant},
};

const READY_CONTENT: &[u8] = b"astra-tui-ready-v1\n";

/// Publish readiness only after the replacement interface draws its first frame.
pub(crate) fn signal_ready(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("Inspect interface readiness marker"),
        Ok(_) => anyhow::bail!("The interface readiness marker already exists"),
    }
    astra_platform::storage::atomic_write_default(path, READY_CONTENT)
        .context("Signal updated interface readiness")
}

#[cfg(any(windows, test))]
fn readiness_path(home: &Path) -> Result<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let directory = home.join("restart-ready");
    astra_platform::storage::private_dir(&directory)?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let path = directory.join(format!(
        "{}-{nonce}-{}.ready",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(path),
        Err(error) => Err(error).context("Inspect interface readiness marker"),
        Ok(_) => anyhow::bail!("The interface readiness marker already exists"),
    }
}

#[cfg(any(windows, test))]
fn wait_for_ready(child: &mut Child, path: &Path, timeout: Duration) -> Result<()> {
    let started = Instant::now();
    let outcome = (|| -> Result<()> {
        loop {
            // Read the durable acknowledgement first: a ready UI may already
            // have handed off to a subsequent update (or the user closed it).
            match astra_platform::storage::read_bounded(path, READY_CONTENT.len() as u64) {
                Ok(content) => {
                    anyhow::ensure!(
                        content == READY_CONTENT,
                        "Invalid interface readiness marker"
                    );
                    return Ok(());
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("Read interface readiness marker"),
            }
            if let Some(status) = child
                .try_wait()
                .context("Inspect updated interface process")?
            {
                anyhow::bail!("The updated interface exited before becoming ready ({status})");
            }
            anyhow::ensure!(
                started.elapsed() < timeout,
                "The updated interface did not become ready before the restart timed out"
            );
            std::thread::sleep(
                Duration::from_millis(25).min(timeout.saturating_sub(started.elapsed())),
            );
        }
    })();
    let cleanup = match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("Remove interface readiness marker"),
    };
    let outcome = outcome.and(cleanup);
    if outcome.is_err() {
        // Ensure the failed candidate cannot appear later beside the recovered
        // old interface, and wait before cleaning up a possible late marker.
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_file(path);
    }
    outcome
}

/// Importing or partially applying a release must never restart the interface.
pub(crate) fn completed_release(status: &updates::UpdateStatus) -> Option<&str> {
    let operation = status.operation.as_ref()?;
    if operation.phase != updates::UpdatePhase::Complete
        || !operation.runtime_committed
        || !operation.next_launch_saved
        || operation.error.is_some()
        || matches!(
            operation.request.intent,
            updates::UpdateIntent::Stage { .. }
        )
        || status.capabilities.operation_running
        || status.capabilities.recovery_operation_id.is_some()
    {
        return None;
    }
    let release_id = operation.release_id.as_deref()?;
    let selected = status.next_launch.as_ref()?;
    (selected.release_id == release_id).then_some(release_id)
}

/// Resolve from the local store, never from a path supplied by a status reply.
/// resolve_current rechecks the manifest, target, protocol and every file hash.
pub(crate) fn resolve(
    context: &windows::Context,
    release_id: &str,
) -> Result<Option<windows::Context>> {
    let Some(release) = astra_update::ReleaseStore::new(&context.home).resolve_current()? else {
        anyhow::bail!("The completed update has no selected local release");
    };
    anyhow::ensure!(
        release.manifest.release_id == release_id,
        "The selected release changed before the interface could restart"
    );
    let name = context
        .executable
        .file_name()
        .and_then(|name| name.to_str())
        .context("Cannot identify the current interface executable")?;
    let component = ["astra-tui", "astra-server-tui"]
        .into_iter()
        .find(|component| {
            let expected = astra_platform::platform::executable(component);
            if cfg!(windows) {
                name.eq_ignore_ascii_case(&expected)
            } else {
                name == expected
            }
        })
        .context("Only Astra interface executables can restart after an update")?;
    anyhow::ensure!(
        release.manifest.components.contains_key(component),
        "The selected release does not contain {component}"
    );
    let frontend = release
        .directory
        .join(astra_platform::platform::executable(component));
    let frontend = std::fs::canonicalize(&frontend)
        .with_context(|| format!("Cannot open updated interface {}", frontend.display()))?;
    if frontend == std::fs::canonicalize(&context.executable)? {
        return Ok(None);
    }
    let mut next = context.clone();
    next.executable = frontend;
    next.bin = std::fs::canonicalize(release.directory)?;
    Ok(Some(next))
}

fn restart_command(context: &windows::Context, launch: &windows::Launch) -> Command {
    let mut command = Command::new(&context.executable);
    command.args(context.arguments(launch)).arg("--resume-ui");
    command
}

/// Called only after checkpointing drafts and restoring the terminal. No host
/// command is sent: the replacement frontend reconnects to the existing host.
pub(crate) fn restart(context: &windows::Context, launch: &windows::Launch) -> Result<()> {
    let mut command = restart_command(context, launch);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(command.exec()).context("Restart the updated interface")
    }
    #[cfg(windows)]
    {
        // Windows cannot replace its running image. Releases are immutable,
        // and the GUI frontend arranges its own interactive console at startup.
        let ready = readiness_path(&context.home)?;
        command
            .arg("--restart-ready")
            .arg(&ready)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = command.spawn().context("Restart the updated interface")?;
        wait_for_ready(&mut child, &ready, Duration::from_secs(10))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = command;
        anyhow::bail!("Interface restart is unavailable on this platform")
    }
}

#[cfg(test)]
#[path = "test/self_update_test.rs"]
mod tests;
