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
                        socket2::Socket::new(socket2::Domain::IPV4, kind, Some(protocol)).unwrap();
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
        &std::fs::read(astra_platform::storage::runtime_path(&path, "egress").unwrap()).unwrap(),
    )
    .unwrap();
    assert_eq!(cache["interface"], "Wi-Fi");
    assert_eq!(cache["gateway"], "192.0.2.254");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
    std::fs::remove_file(astra_platform::storage::runtime_path(&path, "egress").unwrap()).unwrap();
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
    std::fs::remove_file(astra_platform::storage::runtime_path(&path, "egress").unwrap()).unwrap();
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
