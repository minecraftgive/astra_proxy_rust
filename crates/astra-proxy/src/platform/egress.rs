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

/// Fill only a missing interface. Never rewrite a user's explicit selection.
pub fn persist_interface(
    path: &std::path::Path,
    source: &str,
    tun: &crate::mode_config::Tun,
) -> Result<String> {
    if tun.detects_interface() {
        return Ok(source.to_owned());
    }
    use std::io::Write;
    let mut document: toml_edit::DocumentMut = source.parse()?;
    let table = document
        .get_mut("tun")
        .and_then(toml_edit::Item::as_table_like_mut)
        .context("configuration has no TUN table")?;
    if table.contains_key("interface") {
        return Ok(source.to_owned());
    }
    let interface = tun
        .interface
        .as_deref()
        .context("physical egress was not resolved")?;
    table.insert("interface", toml_edit::value(interface));
    let text = document.to_string();
    let cache = crate::subscription::read(path, source);
    anyhow::ensure!(
        std::fs::read_to_string(path)? == source,
        "configuration changed during egress detection; refusing to overwrite it"
    );
    let temporary = path.with_file_name(format!(
        ".astra-egress-{}-{}.tmp",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    let result = (|| -> Result<()> {
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        anyhow::ensure!(
            std::fs::read_to_string(path)? == source,
            "configuration changed before egress save; refusing to overwrite it"
        );
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result?;
    if let Some(mut cache) = cache {
        let migrate = (|| -> Result<()> {
            let mut effective: toml_edit::DocumentMut = cache.effective.parse()?;
            if let Some(table) = effective
                .get_mut("tun")
                .and_then(toml_edit::Item::as_table_like_mut)
            {
                if !table.contains_key("interface") {
                    table.insert("interface", toml_edit::value(interface));
                }
            }
            cache.base = text.clone();
            cache.effective = effective.to_string();
            crate::subscription::save(path, &cache)
        })();
        if let Err(error) = migrate {
            tracing::warn!(%error, "egress saved but subscription cache could not be migrated");
        }
    }
    tracing::info!(%interface, config=%path.display(), "physical egress saved to configuration");
    Ok(text)
}

pub(crate) fn cache_interface(path: &std::path::Path, tun: &crate::mode_config::Tun) -> Result<()> {
    use std::io::Write;
    let cache = path.with_extension("egress-cache.json");
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
        assert_eq!(persist_interface(&path, source, &tun).unwrap(), source);
        cache_interface(&path, &tun).unwrap();
        tun.interface = Some("Wi-Fi".into());
        tun.gateway = Some("192.0.2.254".parse().unwrap());
        cache_interface(&path, &tun).unwrap();
        let cache: serde_json::Value = serde_json::from_slice(
            &std::fs::read(path.with_extension("egress-cache.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(cache["interface"], "Wi-Fi");
        assert_eq!(cache["gateway"], "192.0.2.254");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
        std::fs::remove_file(path.with_extension("egress-cache.json")).unwrap();
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
    fn saves_missing_interface_preserving_comments_cache_and_concurrent_edits() {
        let directory = std::env::temp_dir().join(format!("astra-egress-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("config.toml");
        let source = "# user comment\n[tun]\nname = 'astra-test' # keep\n";
        std::fs::write(&path, source).unwrap();
        let mut tun: crate::mode_config::Tun = toml::from_str("name='astra-test'").unwrap();
        tun.interface = Some("Ethernet 2".into());
        crate::subscription::save(
            &path,
            &crate::subscription::Cache {
                base: source.into(),
                effective: format!("{source}\n# cached nodes\n"),
                ownership: Default::default(),
                subscriptions: Default::default(),
            },
        )
        .unwrap();
        let saved = persist_interface(&path, source, &tun).unwrap();
        assert!(saved.contains("# user comment"));
        assert!(saved.contains("name = 'astra-test' # keep"));
        assert!(saved.contains("interface = \"Ethernet 2\""));
        let cache = crate::subscription::read(&path, &saved).unwrap();
        assert!(cache.effective.contains("# cached nodes"));
        assert!(cache.effective.contains("interface = \"Ethernet 2\""));
        tun.interface = Some("Wi-Fi".into());
        assert_eq!(persist_interface(&path, &saved, &tun).unwrap(), saved);
        std::fs::write(&path, "# edited meanwhile\n").unwrap();
        assert!(persist_interface(&path, source, &tun).is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# edited meanwhile\n"
        );
        std::fs::remove_file(path.with_extension("subscription-cache.json")).unwrap();
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
