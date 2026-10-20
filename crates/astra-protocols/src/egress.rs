//! Shared outbound dial path; TUN owners install meow's socket binding.
use std::io;
use tokio::net::{TcpStream, ToSocketAddrs};

pub async fn connect(address: impl ToSocketAddrs) -> io::Result<TcpStream> {
    let addresses = tokio::net::lookup_host(address).await?;
    let mut last = io::Error::new(io::ErrorKind::AddrNotAvailable, "no outbound addresses");
    for address in addresses {
        match meow_common::connect_tcp(address).await {
            Ok(stream) => return Ok(stream),
            Err(error) => last = error,
        }
    }
    Err(last)
}
