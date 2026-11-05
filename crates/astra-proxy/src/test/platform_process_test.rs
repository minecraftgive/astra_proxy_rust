use super::*;
#[test]
fn worker_exclusions_survive_interrupted_update_and_unwind_cleanup() {
    let workers = Workers::default();
    workers.register(101);
    workers.register(202);
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        workers
            .0
            .rcu(|previous| -> Arc<std::collections::HashSet<u32>> {
                let mut next = (**previous).clone();
                next.clear();
                panic!("interrupted worker exclusion update");
            });
    }))
    .is_err());
    assert!(workers.contains(101));
    assert!(workers.contains(202));
    struct Unregister<'a>(&'a Workers);
    impl Drop for Unregister<'_> {
        fn drop(&mut self) {
            self.0.unregister(101);
        }
    }
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _cleanup = Unregister(&workers);
        panic!("preparation failed");
    }))
    .is_err());
    assert!(!workers.contains(101));
    assert!(workers.contains(202));
    workers.register(303);
    assert!(workers.contains(202));
    assert!(workers.contains(303));
}
#[tokio::test]
async fn ipv6_tcp_and_udp_socket_owners_are_attributed() {
    if !cfg!(any(windows, target_os = "linux")) {
        return;
    }
    let Ok(listener) = tokio::net::TcpListener::bind("[::1]:0").await else {
        return;
    };
    let client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (_server, _) = listener.accept().await.unwrap();
    let owner = lookup(
        Network::Tcp,
        client.local_addr().unwrap(),
        listener.local_addr().unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(owner.pid, std::process::id());
    let udp = tokio::net::UdpSocket::bind("[::]:0").await.unwrap();
    let source = SocketAddr::new("::1".parse().unwrap(), udp.local_addr().unwrap().port());
    let owner = lookup(Network::Udp, source, listener.local_addr().unwrap())
        .await
        .unwrap();
    assert_eq!(owner.pid, std::process::id());
}
#[tokio::test]
async fn local_tcp_and_udp_socket_owners_are_attributed() {
    if !cfg!(any(windows, target_os = "linux")) {
        return;
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (server, _) = listener.accept().await.unwrap();
    let owner = lookup(
        Network::Tcp,
        client.local_addr().unwrap(),
        server.local_addr().unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(owner.pid, std::process::id());
    assert_eq!(
        owner.name,
        std::env::current_exe()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
    );
    let udp = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let owner = lookup(
        Network::Udp,
        udp.local_addr().unwrap(),
        listener.local_addr().unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(owner.pid, std::process::id());
    assert!(lookup(
        Network::Udp,
        SocketAddr::new(
            "203.0.113.1".parse().unwrap(),
            udp.local_addr().unwrap().port()
        ),
        listener.local_addr().unwrap()
    )
    .await
    .is_none());
    assert!(lookup(
        Network::Tcp,
        client.local_addr().unwrap(),
        "203.0.113.1:80".parse().unwrap()
    )
    .await
    .is_none());
}
