#[tokio::main]
async fn main() -> anyhow::Result<()> {
    astra_tui::run().await
}
