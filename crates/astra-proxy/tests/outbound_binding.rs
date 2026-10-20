#![cfg(windows)]
// Separate test process: the outbound registry is intentionally process-global.
use std::os::windows::io::AsRawSocket;
use windows_sys::Win32::{
    NetworkManagement::IpHelper::{GetIfEntry2, MIB_IF_ROW2},
    Networking::WinSock::*,
};

fn binding(socket: &impl AsRawSocket, ipv4: bool) -> u32 {
    let mut value = 0u32;
    let mut size = std::mem::size_of_val(&value) as i32;
    assert_eq!(
        unsafe {
            getsockopt(
                socket.as_raw_socket() as SOCKET,
                if ipv4 { IPPROTO_IP } else { IPPROTO_IPV6 },
                if ipv4 { IP_UNICAST_IF } else { IPV6_UNICAST_IF },
                (&mut value as *mut u32).cast(),
                &mut size,
            )
        },
        0
    );
    // IP_UNICAST_IF takes network order on set, but returns host order on get.
    value
}

#[tokio::test]
async fn outbound_registry_pins_udp_preserves_loopback_and_rolls_back_owners() {
    let mut row = MIB_IF_ROW2 {
        InterfaceIndex: 1,
        ..Default::default()
    };
    assert_eq!(unsafe { GetIfEntry2(&mut row) }, 0);
    let name =
        String::from_utf16_lossy(&row.Alias[..row.Alias.iter().position(|c| *c == 0).unwrap()]);
    let first = meow_common::outbound_iface::install_outbound_interface(&name).unwrap();
    assert!(
        meow_common::outbound_iface::install_outbound_interface("astra-does-not-exist").is_err()
    );
    let second = meow_common::outbound_iface::install_outbound_interface(&name).unwrap();
    for address in ["0.0.0.0:0", "[::]:0"] {
        let udp = meow_common::bind_udp(address).await.unwrap();
        assert_eq!(binding(&udp, udp.local_addr().unwrap().is_ipv4()), 1);
    }
    for address in ["127.0.0.1:0", "[::1]:0"] {
        let server = tokio::net::TcpListener::bind(address).await.unwrap();
        let tcp = astra_protocols::egress::connect(server.local_addr().unwrap())
            .await
            .unwrap();
        let _accepted = server.accept().await.unwrap();
        assert_eq!(binding(&tcp, tcp.local_addr().unwrap().is_ipv4()), 0);
        let udp = meow_common::bind_udp(address).await.unwrap();
        assert_eq!(binding(&udp, udp.local_addr().unwrap().is_ipv4()), 0);
    }
    drop(second);
    assert_eq!(
        meow_common::outbound_iface::outbound_interface().as_deref(),
        Some(name.as_str())
    );
    drop(first);
    assert!(meow_common::outbound_iface::outbound_interface().is_none());
    assert_eq!(
        binding(&meow_common::bind_udp("0.0.0.0:0").await.unwrap(), true),
        0
    );
    // Optional local hardware-interface smoke check; sends no Internet traffic.
    if let (Ok(interface), Ok(address)) = (
        std::env::var("ASTRA_TEST_INTERFACE"),
        std::env::var("ASTRA_TEST_ADDRESS"),
    ) {
        let _guard = meow_common::outbound_iface::install_outbound_interface(&interface).unwrap();
        let server = tokio::net::TcpListener::bind((address.as_str(), 0))
            .await
            .unwrap();
        let tcp = astra_protocols::egress::connect(server.local_addr().unwrap())
            .await
            .unwrap();
        let _accepted = server.accept().await.unwrap();
        let udp = meow_common::bind_udp("0.0.0.0:0").await.unwrap();
        assert_ne!(binding(&tcp, true), 0);
        assert_eq!(binding(&tcp, true), binding(&udp, true));
        eprintln!(
            "physical TCP/UDP socket binding verified: {interface}, index {}",
            binding(&tcp, true)
        );
    }
}
