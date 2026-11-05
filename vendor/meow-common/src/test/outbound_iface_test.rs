use super::*;

/// The loopback interface: the only one every host is guaranteed to
/// have, and binding to it keeps the concurrent loopback socket tests
/// in this binary working (a made-up name would break them).
const LO: &str = if cfg!(target_os = "macos") {
    "lo0"
} else {
    "lo"
};

/// Id of the owner in effect — distinguishes owners that bind the same
/// interface.
fn current_owner() -> Option<u64> {
    OWNERS.read().last().map(|o| o.id)
}

fn live_owners() -> usize {
    OWNERS.read().len()
}

fn lo() -> OutboundIfaceGuard {
    install_outbound_interface(LO).expect("loopback must exist")
}

/// One test drives every case because the registry is process-global;
/// separate `#[test]` fns would race each other.
#[test]
fn owners_install_apply_supersede_and_restore() {
    assert!(outbound_interface().is_none());

    // A bogus interface must be rejected and leave nothing installed.
    assert!(install_outbound_interface("no-such-iface-zz9").is_err());
    assert!(install_outbound_interface("").is_err());
    assert!(install_outbound_interface("lo0\0x").is_err());
    assert!(outbound_interface().is_none());

    // Install → apply → drop clears.
    let a = lo();
    assert_eq!(a.interface(), LO);
    assert_eq!(outbound_interface().as_deref(), Some(LO));
    assert_eq!(current_owner(), Some(a.id));
    // Binding a fresh socket to loopback succeeds, in both families
    // and for both socket types.
    for domain in [socket2::Domain::IPV4, socket2::Domain::IPV6] {
        for (ty, proto) in [
            (socket2::Type::STREAM, socket2::Protocol::TCP),
            (socket2::Type::DGRAM, socket2::Protocol::UDP),
        ] {
            let socket = socket2::Socket::new(domain, ty, Some(proto)).unwrap();
            apply_outbound_interface(&socket, domain).expect("bind to loopback");
            assert_bound_to_loopback(&socket, domain);
        }
    }
    dial_chokepoints_bind_their_sockets();
    drop(a);
    assert!(outbound_interface().is_none());
    assert_eq!(live_owners(), 0);

    // With nothing installed, apply is a no-op.
    let socket2 = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )
    .unwrap();
    apply_outbound_interface(&socket2, socket2::Domain::IPV4).expect("no-op apply");
    #[cfg(target_os = "macos")]
    assert_eq!(socket2.device_index_v4().unwrap(), None);

    // Supersede, then the superseding owner leaves (a rejected reload):
    // the still-live superseded owner is back in effect, not cleared.
    let a = lo();
    let b = lo();
    assert_eq!(current_owner(), Some(b.id));
    drop(b);
    assert_eq!(current_owner(), Some(a.id));
    assert_eq!(outbound_interface().as_deref(), Some(LO));
    drop(a);
    assert!(outbound_interface().is_none());

    // Out of order (a reload restart: the new binding is installed,
    // then the old listener is torn down): dropping the superseded
    // owner is a no-op — the successor's binding survives.
    let a = lo();
    let b = lo();
    drop(a);
    assert_eq!(current_owner(), Some(b.id));
    assert_eq!(outbound_interface().as_deref(), Some(LO));
    // The superseding owner outlived its predecessor: nothing to
    // restore, so its drop clears.
    drop(b);
    assert!(outbound_interface().is_none());
    assert_eq!(live_owners(), 0);

    // Three deep, middle owner dropped first: the top stays in effect,
    // and the top leaving skips the dead middle and restores the bottom.
    let a = lo();
    let b = lo();
    let c = lo();
    drop(b);
    assert_eq!(current_owner(), Some(c.id));
    drop(c);
    assert_eq!(current_owner(), Some(a.id));
    assert_eq!(outbound_interface().as_deref(), Some(LO));
    drop(a);
    assert!(outbound_interface().is_none());

    // Three deep, unwound strictly in order.
    let a = lo();
    let b = lo();
    let c = lo();
    drop(c);
    assert_eq!(current_owner(), Some(b.id));
    drop(b);
    assert_eq!(current_owner(), Some(a.id));
    drop(a);
    assert!(outbound_interface().is_none());

    // Three deep, bottom first then top: the middle takes over.
    let a = lo();
    let b = lo();
    let c = lo();
    drop(a);
    assert_eq!(current_owner(), Some(c.id));
    drop(c);
    assert_eq!(current_owner(), Some(b.id));
    drop(b);
    assert!(outbound_interface().is_none());
    assert_eq!(live_owners(), 0);

    // A failed install while an owner is live leaves it in effect.
    let a = lo();
    assert!(install_outbound_interface("no-such-iface-zz9").is_err());
    assert_eq!(current_owner(), Some(a.id));
    assert_eq!(live_owners(), 1);
    drop(a);
    assert!(outbound_interface().is_none());
}

/// The dial chokepoints behind `connect_tcp` / `bind_udp`, driven with
/// the loopback interface installed. Called from the one registry test
/// (the chokepoints read the process-global registry), and through the
/// `*_iface_bound` halves directly: the public entry points prefer a
/// `SocketProtector`, which concurrent tests in this binary install.
fn dial_chokepoints_bind_their_sockets() {
    use crate::socket_protect::{bind_udp_iface_bound, connect_tcp_iface_bound};
    use socket2::{Domain, SockRef};
    use std::net::SocketAddr;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        // TCP: the dial is bound and still reaches a loopback listener.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = connect_tcp_iface_bound(addr).await.expect("loopback dial");
        listener.accept().await.unwrap();
        assert_bound_to_loopback(&SockRef::from(&stream), Domain::IPV4);

        // UDP: a wildcard socket is bound and carries a datagram.
        let receiver = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let wildcard: SocketAddr = "0.0.0.0:0".parse().unwrap();
        let socket = bind_udp_iface_bound(wildcard).expect("wildcard bind");
        assert_bound_to_loopback(&SockRef::from(&socket), Domain::IPV4);
        socket
            .send_to(b"ping", receiver.local_addr().unwrap())
            .await
            .unwrap();
        let mut buf = [0u8; 8];
        let (n, _) = receiver.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"ping");

        let wildcard6: SocketAddr = "[::]:0".parse().unwrap();
        let socket6 = bind_udp_iface_bound(wildcard6).expect("v6 wildcard bind");
        assert_bound_to_loopback(&SockRef::from(&socket6), Domain::IPV6);

        // The scope is enforced by the kernel, not just recorded: a
        // socket bound to loopback has no route to a non-loopback
        // destination (TEST-NET-1 — nothing leaves the host either way).
        #[cfg(target_os = "macos")]
        {
            let outside: SocketAddr = "192.0.2.1:9".parse().unwrap();
            let err = connect_tcp_iface_bound(outside)
                .await
                .expect_err("loopback-scoped socket must not reach TEST-NET-1");
            assert_eq!(err.raw_os_error(), Some(libc::ENETUNREACH), "{err}");
            let err = socket.send_to(b"x", outside).await.expect_err("scoped UDP");
            assert_eq!(err.raw_os_error(), Some(libc::ENETUNREACH), "{err}");

            // A UDP socket pinned to a loopback address is exempt.
            let pinned: SocketAddr = "127.0.0.1:0".parse().unwrap();
            let socket = bind_udp_iface_bound(pinned).expect("loopback bind");
            assert_eq!(SockRef::from(&socket).device_index_v4().unwrap(), None);
            assert!(!binds_peer(pinned.ip()) && binds_peer(outside.ip()));
        }
    });
}

/// The kernel reports the binding the socket option installed.
fn assert_bound_to_loopback(socket: &socket2::Socket, domain: socket2::Domain) {
    #[cfg(target_os = "macos")]
    {
        let want = interface_index(LO).unwrap();
        let got = if domain == socket2::Domain::IPV6 {
            socket.device_index_v6().unwrap()
        } else {
            socket.device_index_v4().unwrap()
        };
        assert_eq!(got.map(std::num::NonZeroU32::get), want);
    }
    #[cfg(target_os = "linux")]
    {
        let _ = domain;
        assert_eq!(socket.device().unwrap().as_deref(), Some(LO.as_bytes()));
    }
}
