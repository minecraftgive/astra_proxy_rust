//! Windows login helper: starts the manager and configured instances without
//! allocating a console or leaving an invisible interactive TUI running.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    astra_tui::run_silent().await
}

#[cfg(not(windows))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("Silent startup is only supported on Windows")
}
