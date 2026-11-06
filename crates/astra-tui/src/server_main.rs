#![cfg_attr(windows, windows_subsystem = "windows")]

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    astra_tui::run_server().await
}
