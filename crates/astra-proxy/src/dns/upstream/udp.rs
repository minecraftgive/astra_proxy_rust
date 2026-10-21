use super::interface;
use crate::{egress, App};
use anyhow::Result;
use std::net::SocketAddr;
use tokio::net::UdpSocket;
pub(crate) async fn direct(app: &App, address: SocketAddr, query: &[u8]) -> Result<Vec<u8>> {
    let address = egress::scoped_destination(address, interface(app))?;
    let socket = UdpSocket::bind(if address.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    })
    .await?;
    egress::bind_interface(
        &socket,
        interface(app).filter(|_| !address.ip().is_loopback()),
        address.is_ipv4(),
    )?;
    socket.connect(address).await?;
    socket.send(query).await?;
    let mut bytes = vec![0; 65535];
    let size = socket.recv(&mut bytes).await?;
    bytes.truncate(size);
    Ok(bytes)
}
