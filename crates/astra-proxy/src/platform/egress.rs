//! Explicit physical-interface binding prevents direct TUN routes recapturing themselves.
#[cfg(unix)]
use anyhow::ensure;
use anyhow::{Context, Result};
#[cfg(unix)]
use std::{ffi::CString, os::fd::AsRawFd};
#[cfg(unix)]
pub fn bind_interface(socket: &impl AsRawFd, interface: Option<&str>, ipv4: bool) -> Result<()> {
    let Some(interface) = interface else {
        return Ok(());
    };
    let name = CString::new(interface)?;
    #[cfg(target_os = "macos")]
    {
        let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
        ensure!(index != 0, "unknown egress interface {interface}");
        let result = unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                if ipv4 {
                    libc::IPPROTO_IP
                } else {
                    libc::IPPROTO_IPV6
                },
                if ipv4 {
                    libc::IP_BOUND_IF
                } else {
                    libc::IPV6_BOUND_IF
                },
                &index as *const _ as *const _,
                std::mem::size_of_val(&index) as _,
            )
        };
        ensure!(
            result == 0,
            "bind egress interface: {}",
            std::io::Error::last_os_error()
        );
    }
    #[cfg(target_os = "linux")]
    {
        let _ = ipv4;
        let result = unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_BINDTODEVICE,
                name.as_ptr() as *const _,
                name.as_bytes_with_nul().len() as _,
            )
        };
        ensure!(
            result == 0,
            "bind egress interface: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}
#[cfg(windows)]
pub fn bind_interface(
    socket: &impl std::os::windows::io::AsRawSocket,
    interface: Option<&str>,
    ipv4: bool,
) -> Result<()> {
    if let Some(interface) = interface {
        super::windows_network::bind_socket(socket, interface, ipv4)?;
    }
    Ok(())
}
pub async fn connect(target: &astra_ipc::Target, interface: &str) -> Result<tokio::net::TcpStream> {
    let addresses = tokio::net::lookup_host((target.host.as_str(), target.port)).await?;
    let mut last = None;
    for address in addresses {
        let socket = if address.is_ipv4() {
            tokio::net::TcpSocket::new_v4()?
        } else {
            tokio::net::TcpSocket::new_v6()?
        };
        bind_interface(
            &socket,
            (!address.ip().is_loopback()).then_some(interface),
            address.is_ipv4(),
        )?;
        match socket.connect(address).await {
            Ok(stream) => {
                stream.set_nodelay(true)?;
                return Ok(stream);
            }
            Err(error) => last = Some(error),
        }
    }
    Err(last.context("no egress addresses")?.into())
}
