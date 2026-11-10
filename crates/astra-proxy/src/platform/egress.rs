//! Explicit physical-interface binding prevents direct TUN routes recapturing themselves.
#[cfg(unix)]
use anyhow::ensure;
use anyhow::{Context, Result};
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub(super) struct PhysicalEgress {
    pub interface: String,
    pub gateway: Option<std::net::Ipv4Addr>,
    pub gateway6: Option<std::net::Ipv6Addr>,
}
pub(crate) fn settled_update(
    previous: &crate::mode_config::Tun,
    observed: crate::mode_config::Tun,
    pending: &mut Option<crate::mode_config::Tun>,
) -> bool {
    if &observed == previous {
        *pending = None;
        return false;
    }
    if pending.as_ref() != Some(&observed) {
        *pending = Some(observed);
        return false;
    }
    true
}
/// Resolve a generation before installing routes. Automatic selection remains
/// automatic across reloads; discovery results never turn into a user override.
pub fn prepare_interface(tun: &mut crate::mode_config::Tun) -> Result<()> {
    let automatic = tun.detects_interface();
    tun.auto_interface = Some(automatic);
    if automatic || tun.auto_gateway {
        let selected = if automatic {
            None
        } else {
            tun.interface.as_deref()
        };
        #[cfg(windows)]
        let detected = super::windows_network::physical_egress(selected, tun.name.as_deref())?;
        #[cfg(not(windows))]
        let detected = super::unix_network::physical_egress(selected, tun.name.as_deref())?;
        anyhow::ensure!(
            tun.name.as_deref() != Some(&detected.interface),
            "TUN cannot be its own egress"
        );
        tun.interface = Some(detected.interface);
        if tun.auto_gateway {
            tun.gateway = detected.gateway.map(std::net::IpAddr::V4);
            tun.gateway6 = detected.gateway6;
        }
    } else {
        let interface = tun
            .interface
            .as_deref()
            .context("manual egress requires tun.interface")?;
        #[cfg(windows)]
        {
            tun.interface = Some(super::windows_network::interface_alias(interface)?);
        }
        #[cfg(not(windows))]
        {
            anyhow::ensure!(
                tun.name.as_deref() != Some(interface),
                "TUN cannot be its own egress"
            );
        }
    }
    Ok(())
}

pub(crate) fn cache_interface(path: &std::path::Path, tun: &crate::mode_config::Tun) -> Result<()> {
    use std::io::Write;
    let cache = astra_platform::storage::runtime_path(path, "egress")?;
    astra_platform::storage::prepare_parent(&cache)?;
    let temporary = cache.with_file_name(format!(".astra-egress-{}.tmp", uuid::Uuid::new_v4()));
    let text = serde_json::to_vec_pretty(
        &serde_json::json!({"version":1,"interface":tun.interface,"gateway":tun.gateway,"gateway6":tun.gateway6,"auto_interface":tun.detects_interface(),"auto_gateway":tun.auto_gateway}),
    )?;
    let result = (|| -> Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        astra_platform::platform::adopt_owner(&temporary)?;
        file.write_all(&text)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, &cache)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}
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

/// TUN loses the original IPv6 zone. Reconstruct it from the pinned egress,
/// never from the OS's default link-local route (which can be the TUN itself).
pub fn scoped_destination(
    mut address: std::net::SocketAddr,
    interface: Option<&str>,
) -> Result<std::net::SocketAddr> {
    if let std::net::SocketAddr::V6(ref mut address) = address {
        if address.ip().is_unicast_link_local() {
            let interface = interface.context(
                "TUN IPv6 link-local destination requires tun.interface with a physical egress",
            )?;
            #[cfg(windows)]
            let index = super::windows_network::interface_index(interface)?;
            #[cfg(unix)]
            let index = {
                let name = CString::new(interface)?;
                let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
                ensure!(index != 0, "unknown egress interface {interface}");
                index
            };
            address.set_scope_id(index);
        }
    }
    Ok(address)
}

pub async fn connect(
    target: &astra_model::Target,
    interface: Option<&str>,
) -> Result<tokio::net::TcpStream> {
    // Literals short-circuit; hostnames use the installed resolver hook.
    let addresses = meow_common::resolve_host_all(&target.host, target.port).await?;
    let mut last = None;
    for address in addresses {
        let address = scoped_destination(address, interface)?;
        let socket = if address.is_ipv4() {
            tokio::net::TcpSocket::new_v4()?
        } else {
            tokio::net::TcpSocket::new_v6()?
        };
        bind_interface(
            &socket,
            interface.filter(|_| !address.ip().is_loopback()),
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

#[cfg(test)]
#[path = "../test/platform_egress_test.rs"]
mod tests;
