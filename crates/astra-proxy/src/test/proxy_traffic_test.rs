use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing_subscriber::prelude::*;

#[test]
fn node_totals_include_closed_tcp_and_udp_flows_but_exclude_direct_bytes() {
    let metrics = Arc::new(Metrics::default());
    for (network, action, up, down) in [
        (Network::Tcp, RouteAction::Proxy("tokyo".into()), 100, 200),
        (Network::Udp, RouteAction::Proxy("tokyo".into()), 30, 40),
        (Network::Tcp, RouteAction::Proxy("osaka".into()), 7, 8),
        (Network::Udp, RouteAction::Direct, 900, 1000),
    ] {
        let flow = Traffic::new(
            &metrics,
            None,
            &Target {
                host: "example.test".into(),
                port: 443,
            },
            Inbound::Socks5,
            network,
            &action,
        )
        .unwrap();
        flow.upload(up);
        flow.download(down);
    }
    assert_eq!(metrics.snapshot().active, 0);
    assert_eq!(
        metrics.node_totals(),
        [("osaka".into(), 15), ("tokyo".into(), 370)].into()
    );
    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.proxy_upload + snapshot.proxy_download, 385);
    assert_eq!(snapshot.upload + snapshot.download, 2285);
}
#[tokio::test]
async fn poisoned_flow_index_does_not_panic_again_during_traffic_drop() {
    let metrics = Arc::new(Metrics::default());
    let make = || {
        Traffic::new(
            &metrics,
            None,
            &Target {
                host: "example.test".into(),
                port: 443,
            },
            Inbound::Socks5,
            Network::Tcp,
            &RouteAction::Proxy("node".into()),
        )
        .unwrap()
    };
    let dropping = make();
    let survivor = make();
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = metrics.flows.lock().unwrap();
        panic!("injected traffic index panic");
    }))
    .is_err());
    // Drop runs while another panic is already unwinding. An unwrap here
    // would abort the test process instead of preserving the original panic.
    let original = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _traffic = dropping;
        panic!("original connection task panic");
    }))
    .unwrap_err();
    assert_eq!(
        original.downcast_ref::<&str>(),
        Some(&"original connection task panic")
    );
    tokio::time::timeout(std::time::Duration::from_secs(1), survivor.cancellation())
        .await
        .unwrap();
    assert_eq!(metrics.snapshot().active, 0);
    assert!(!metrics.flows.is_poisoned());
    let next = make();
    assert_eq!(metrics.snapshot().active, 1);
    assert!(!*next.cancel.borrow());
    drop(next);
    drop(survivor);
    assert_eq!(metrics.snapshot().active, 0);
}
#[tokio::test(flavor = "current_thread")]
async fn stream_counts_actual_transferred_bytes_and_handles_half_close() {
    let subscriber =
        tracing_subscriber::registry().with(tracing_subscriber::EnvFilter::new("debug"));
    let _guard = tracing::subscriber::set_default(subscriber);
    let (inner, mut peer) = tokio::io::duplex(64);
    let target = Target {
        host: "example.test".into(),
        port: 443,
    };
    let traffic = Traffic::new(
        &Arc::default(),
        Some("127.0.0.1:40000".parse().unwrap()),
        &target,
        Inbound::Socks5,
        Network::Tcp,
        &RouteAction::Direct,
    );
    assert!(traffic.is_some());
    let mut stream = Stream::new(inner, traffic);
    tokio::try_join!(
        async {
            let slices = [io::IoSlice::new(b"he"), io::IoSlice::new(b"llo")];
            let size = stream.write_vectored(&slices).await?;
            stream.write_all(&b"hello"[size..]).await?;
            stream.shutdown().await?;
            let mut response = Vec::new();
            stream.read_to_end(&mut response).await?;
            assert_eq!(response, b"world!");
            Ok::<_, io::Error>(())
        },
        async {
            let mut request = Vec::new();
            peer.read_to_end(&mut request).await?;
            assert_eq!(request, b"hello");
            peer.write_all(b"world!").await?;
            peer.shutdown().await
        }
    )
    .unwrap();
    let traffic = stream.traffic.as_ref().unwrap();
    assert_eq!(traffic.up.load(Ordering::Relaxed), 5);
    assert_eq!(traffic.down.load(Ordering::Relaxed), 6);
    assert!(!traffic.failed.load(Ordering::Relaxed));
}
#[tokio::test]
async fn closing_a_flow_wakes_idle_io_and_repeated_io_stays_closed_without_route_logging() {
    let metrics = Arc::new(Metrics::default());
    let (inner, _peer) = tokio::io::duplex(16);
    let traffic = Traffic::new(
        &metrics,
        None,
        &Target {
            host: "example.test".into(),
            port: 443,
        },
        Inbound::Socks5,
        Network::Tcp,
        &RouteAction::Direct,
    );
    let mut stream = Stream::new(inner, traffic);
    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.active, 1);
    assert!(metrics.disconnect(Some(snapshot.connections[0].id)));
    for _ in 0..2 {
        assert_eq!(
            stream.read_u8().await.unwrap_err().kind(),
            io::ErrorKind::ConnectionAborted
        );
        assert_eq!(
            stream.write_all(b"never sent").await.unwrap_err().kind(),
            io::ErrorKind::ConnectionAborted
        );
    }
    drop(stream);
    assert_eq!(metrics.snapshot().active, 0);
    assert_eq!(metrics.snapshot().upload, 0);
}

#[tokio::test(flavor = "current_thread")]
async fn failed_write_is_reported_without_counting_unsent_bytes() {
    let subscriber =
        tracing_subscriber::registry().with(tracing_subscriber::EnvFilter::new("debug"));
    let _guard = tracing::subscriber::set_default(subscriber);
    let (inner, peer) = tokio::io::duplex(64);
    drop(peer);
    let target = Target {
        host: "example.test".into(),
        port: 443,
    };
    let traffic = Traffic::new(
        &Arc::default(),
        None,
        &target,
        Inbound::Tun,
        Network::Tcp,
        &RouteAction::Direct,
    );
    let mut stream = Stream::new(inner, traffic);
    assert!(stream.write_all(b"not sent").await.is_err());
    let traffic = stream.traffic.as_ref().unwrap();
    assert!(traffic.failed.load(Ordering::Relaxed));
    assert_eq!(traffic.up.load(Ordering::Relaxed), 0);
}
