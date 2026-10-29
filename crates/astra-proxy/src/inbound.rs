mod http;
mod socks5;
pub use http::handle_http;
pub use socks5::handle_socks5;

mod authentication;
pub(crate) use authentication::{authorize_connect, http_authorized};
pub async fn handle_mixed(
    client: tokio::net::TcpStream,
    app: std::sync::Arc<crate::App>,
) -> anyhow::Result<()> {
    let mut first = [0u8; 1];
    anyhow::ensure!(
        tokio::time::timeout(std::time::Duration::from_secs(10), client.peek(&mut first)).await??
            == 1,
        "mixed listener closed before handshake"
    );
    if first[0] == 5 {
        handle_socks5(client, app).await
    } else {
        handle_http(client, app).await
    }
}
