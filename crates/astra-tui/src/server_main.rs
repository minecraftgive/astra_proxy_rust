#[tokio::main]
async fn main() -> anyhow::Result<()> {
    astra_tui::run_server().await
}
