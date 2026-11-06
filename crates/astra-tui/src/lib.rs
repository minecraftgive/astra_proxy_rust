mod availability;
mod clipboard;
mod diagnostics;
mod editor;
mod entry;
mod explanation;
mod export;
mod external_editor;
mod help;
mod highlight;
mod i18n;
mod import;
mod log_stream;
mod logs;
#[cfg(test)]
#[path = "test/lib_manager_test.rs"]
mod manager;
mod nettools;
mod network_interfaces;
mod protocol;
mod self_update;
mod shell;
#[cfg(windows)]
mod startup;
#[cfg(any(target_os = "macos", target_os = "linux", test))]
mod startup_unix;
use astra_management::subscription_view;
mod terminals;
mod theme;
use astra_platform::tui_log;
mod ui;
mod windows;
use entry::UI_REFRESH_INTERVAL;

/// Shared terminal application; executable entry points only select an initial view.
pub async fn run() -> anyhow::Result<()> {
    entry::run(None).await
}
pub async fn run_server() -> anyhow::Result<()> {
    entry::run(Some(protocol::Kind::Server)).await
}
