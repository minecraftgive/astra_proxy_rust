#[tokio::main]
async fn main() -> anyhow::Result<()> {
    astra_proxy::run().await
}
