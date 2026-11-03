#[tokio::main]
async fn main() -> anyhow::Result<()> {
    astra_rule::run().await
}
