#[tokio::main]
async fn main() -> anyhow::Result<()> {
    astra_server_core::run().await
}
