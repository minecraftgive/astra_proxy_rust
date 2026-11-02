mod availability;
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
mod manager;
mod nettools;
mod network_interfaces;
mod protocol;
mod subscription_view;
mod theme;
mod tui_log;
mod ui;
mod windows;
use entry::UI_REFRESH_INTERVAL;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    entry::run().await
}
