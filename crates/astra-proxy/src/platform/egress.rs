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
    let cache = astra_ipc::storage::runtime_path(path, "egress")?;
    astra_ipc::storage::prepare_parent(&cache)?;
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
    target: &astra_ipc::Target,
    interface: Option<&str>,
) -> Result<tokio::net::TcpStream> {
    let addresses = tokio::net::lookup_host((target.host.as_str(), target.port)).await?;
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
mod tests {
    use super::*;

    #[cfg(windows)]
    #[tokio::test]
    async fn concurrent_group_scopes_bind_real_tcp_and_udp_sockets_independently() {
        use socket2_05 as socket2;
        use std::os::windows::io::AsRawSocket;
        use windows_sys::Win32::{NetworkManagement::IpHelper::*, Networking::WinSock::*};
        let mut pointer = std::ptr::null_mut();
        assert_eq!(unsafe { GetIfTable2(&mut pointer) }, 0);
        let rows = unsafe {
            std::slice::from_raw_parts((*pointer).Table.as_ptr(), (*pointer).NumEntries as usize)
        };
        let adapters: Vec<_> = rows
            .iter()
            .filter_map(|row| {
                let mut ip = MIB_IPINTERFACE_ROW::default();
                unsafe { InitializeIpInterfaceEntry(&mut ip) };
                ip.Family = AF_INET;
                ip.InterfaceIndex = row.InterfaceIndex;
                if unsafe { GetIpInterfaceEntry(&mut ip) } != 0 {
                    return None;
                }
                let end = row
                    .Alias
                    .iter()
                    .position(|c| *c == 0)
                    .unwrap_or(row.Alias.len());
                Some((
                    String::from_utf16_lossy(&row.Alias[..end]),
                    row.InterfaceIndex,
                ))
            })
            .take(2)
            .collect();
        unsafe { FreeMibTable(pointer.cast()) };
        assert_eq!(
            adapters.len(),
            2,
            "two local IP adapters are required for this isolation check"
        );
        let check = |(name, index): (String, u32)| async move {
            meow_common::outbound_iface::with_outbound_interface(Some(&name), async {
                for _ in 0..8 {
                    tokio::task::yield_now().await;
                    assert_eq!(
                        meow_common::outbound_iface::outbound_interface().as_deref(),
                        Some(name.as_str())
                    );
                    for (kind, protocol) in [
                        (socket2::Type::STREAM, socket2::Protocol::TCP),
                        (socket2::Type::DGRAM, socket2::Protocol::UDP),
                    ] {
                        let socket =
                            socket2::Socket::new(socket2::Domain::IPV4, kind, Some(protocol))
                                .unwrap();
                        meow_common::outbound_iface::apply_outbound_interface(
                            &socket,
                            socket2::Domain::IPV4,
                        )
                        .unwrap();
                        let mut value: u32 = 0;
                        let mut size = std::mem::size_of_val(&value) as i32;
                        assert_eq!(
                            unsafe {
                                getsockopt(
                                    socket.as_raw_socket() as SOCKET,
                                    IPPROTO_IP,
                                    IP_UNICAST_IF,
                                    (&mut value as *mut u32).cast(),
                                    &mut size,
                                )
                            },
                            0
                        );
                        // Windows returns this option in host order on reads.
                        assert_eq!(value, index, "wrong interface for {name}");
                    }
                }
            })
            .await
            .unwrap();
        };
        let mut adapters = adapters.into_iter();
        tokio::join!(
            check(adapters.next().unwrap()),
            check(adapters.next().unwrap())
        );
    }

    #[test]
    fn automatic_mode_stays_automatic_and_cache_is_only_diagnostic() {
        let directory =
            std::env::temp_dir().join(format!("astra-egress-auto-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("client.toml");
        let source = "# preserve\n[tun]\nauto_interface=true\n";
        std::fs::write(&path, source).unwrap();
        let mut tun: crate::mode_config::Tun = toml::from_str("auto_interface=true").unwrap();
        tun.interface = Some("Ethernet".into());
        tun.gateway = Some("192.0.2.1".parse().unwrap());

        cache_interface(&path, &tun).unwrap();
        tun.interface = Some("Wi-Fi".into());
        tun.gateway = Some("192.0.2.254".parse().unwrap());
        cache_interface(&path, &tun).unwrap();
        let cache: serde_json::Value = serde_json::from_slice(
            &std::fs::read(astra_ipc::storage::runtime_path(&path, "egress").unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(cache["interface"], "Wi-Fi");
        assert_eq!(cache["gateway"], "192.0.2.254");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
        std::fs::remove_file(astra_ipc::storage::runtime_path(&path, "egress").unwrap()).unwrap();
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
    #[test]
    fn automatic_discovery_is_debounced_and_manual_selection_is_explicit() {
        let mut previous: crate::mode_config::Tun =
            toml::from_str("auto_interface=true\ninterface='Ethernet'").unwrap();
        assert!(previous.detects_interface());
        let mut detected = previous.clone();
        detected.interface = Some("Wi-Fi".into());
        detected.gateway = Some("192.0.2.254".parse().unwrap());
        let mut pending = None;
        assert!(!settled_update(&previous, detected.clone(), &mut pending));
        assert!(settled_update(&previous, detected.clone(), &mut pending));
        assert!(!settled_update(&previous, previous.clone(), &mut pending));
        assert!(pending.is_none());
        assert!(!settled_update(&previous, detected, &mut pending));
        previous.auto_interface = Some(false);
        assert!(!previous.detects_interface());
        let explicit: crate::mode_config::Tun = toml::from_str("interface='Wi-Fi'").unwrap();
        assert!(!explicit.detects_interface());
        let automatic: crate::mode_config::Tun = toml::from_str("").unwrap();
        assert!(automatic.detects_interface());
    }

    #[test]
    fn runtime_egress_does_not_rewrite_source_or_subscription_cache() {
        let directory = std::env::temp_dir().join(format!("astra-egress-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("config.toml");
        let source = "# user comment\n[tun]\nname = 'astra-test' # keep\n";
        std::fs::write(&path, source).unwrap();
        let mut tun: crate::mode_config::Tun = toml::from_str("name='astra-test'").unwrap();
        tun.interface = Some("Ethernet 2".into());
        cache_interface(&path, &tun).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
        std::fs::write(&path, "# edited meanwhile\n").unwrap();
        tun.interface = Some("Wi-Fi".into());
        cache_interface(&path, &tun).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# edited meanwhile\n"
        );
        std::fs::remove_file(astra_ipc::storage::runtime_path(&path, "egress").unwrap()).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn tun_link_local_requires_a_real_egress_and_uses_its_scope() {
        let link_local = "[fe80::123]:53".parse().unwrap();
        assert!(scoped_destination(link_local, None).is_err());
        assert!(scoped_destination(link_local, Some("astra-nonexistent-interface")).is_err());
        #[cfg(windows)]
        let interface = "1"; // Windows loopback interface, no external traffic.
        #[cfg(target_os = "linux")]
        let interface = "lo";
        #[cfg(target_os = "macos")]
        let interface = "lo0";
        let scoped = scoped_destination(link_local, Some(interface)).unwrap();
        let std::net::SocketAddr::V6(scoped) = scoped else {
            panic!("IPv6 required")
        };
        assert_ne!(scoped.scope_id(), 0);
        assert_eq!(
            scoped.ip(),
            &"fe80::123".parse::<std::net::Ipv6Addr>().unwrap()
        );
        assert_eq!(scoped.port(), 53);
        for address in [
            "[2001:db8::1]:53",
            "[fd00::1]:53",
            "127.0.0.1:53",
            "[::1]:53",
        ] {
            let address = address.parse().unwrap();
            assert_eq!(scoped_destination(address, None).unwrap(), address);
        }
    }
}
