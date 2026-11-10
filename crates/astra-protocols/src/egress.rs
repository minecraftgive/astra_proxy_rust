//! Shared outbound dial path; TUN owners install meow's socket binding.
//! Hostnames resolve through meow's installed `HostResolver` (the proxy core
//! installs its encrypted DNS resolver), never directly via getaddrinfo.
use std::{borrow::Cow, io, net::SocketAddr};
use tokio::net::TcpStream;

/// A dial endpoint: `SocketAddr`, `(host, port)` or `"host:port"` /
/// `"[v6]:port"`.
pub trait Endpoint {
    fn host_port(&self) -> io::Result<(Cow<'_, str>, u16)>;
}
impl Endpoint for SocketAddr {
    fn host_port(&self) -> io::Result<(Cow<'_, str>, u16)> {
        Ok((self.ip().to_string().into(), self.port()))
    }
}
impl Endpoint for (&str, u16) {
    fn host_port(&self) -> io::Result<(Cow<'_, str>, u16)> {
        Ok((self.0.into(), self.1))
    }
}
impl Endpoint for &str {
    fn host_port(&self) -> io::Result<(Cow<'_, str>, u16)> {
        let invalid = || {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid endpoint {self}"),
            )
        };
        let (host, port) = self.rsplit_once(':').ok_or_else(invalid)?;
        let port = port.parse().map_err(|_| invalid())?;
        Ok((
            host.trim_start_matches('[').trim_end_matches(']').into(),
            port,
        ))
    }
}

pub async fn connect(address: impl Endpoint) -> io::Result<TcpStream> {
    let (host, port) = address.host_port()?;
    meow_common::connect_tcp_host(&host, port).await
}
