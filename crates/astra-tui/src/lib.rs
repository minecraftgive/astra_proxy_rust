use astra_client::clipboard;
pub use astra_client::tr;
mod diagnostics;
mod editor;
mod entry;
mod explanation;
use astra_client::export;
use astra_client::external_editor;
mod help;
mod highlight;
use astra_client::i18n;
use astra_client::import;
mod logs;
#[cfg(test)]
#[path = "test/lib_manager_test.rs"]
mod manager;
mod nettools;
use astra_client::protocol;
use astra_client::self_update;
#[cfg(windows)]
use astra_client::startup;
#[cfg(any(target_os = "macos", target_os = "linux", test))]
use astra_client::startup_unix;
use astra_client::terminals;
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
mod input;
