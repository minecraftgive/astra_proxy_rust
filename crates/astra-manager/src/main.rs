#[tokio::main]
async fn main() -> anyhow::Result<()> {
    astra_manager::run().await
}
