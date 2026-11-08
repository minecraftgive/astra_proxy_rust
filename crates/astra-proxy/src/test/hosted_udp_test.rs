use super::*;
fn app() -> Arc<App> {
    Arc::new(App::from_text(&std::env::temp_dir().join("hosted-udp-test.toml"),
            "standalone=true\nstandalone_outbound='direct'\nsocks5_listen='127.0.0.1:1080'\nmax_udp_flows=1\nudp_targets_per_association=1\n[proxy_groups.direct]\nmembers=['DIRECT']\n".into()).unwrap())
}

#[tokio::test]
async fn worker_channel_preserves_empty_udp_and_releases_flow_on_transport_close() {
    let echo = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let destination = echo.local_addr().unwrap();
    let echo = tokio::spawn(async move {
        let mut buffer = [0; 1024];
        for _ in 0..2 {
            let (size, peer) = echo.recv_from(&mut buffer).await.unwrap();
            echo.send_to(&buffer[..size], peer).await.unwrap();
        }
    });
    let (mut host, worker) = tokio::io::duplex(128);
    let app = app();
    let previous = Arc::downgrade(&app);
    let worker = tokio::spawn(serve(
        worker,
        app,
        Metadata {
            flow_id: None,
            inbound: Inbound::Tun,
            source: "127.0.0.1:45000".parse().unwrap(),
            local: destination,
            destination: Some(destination),
            process: None,
        },
    ));
    let ready: std::result::Result<(), String> = receive(&mut host).await.unwrap();
    ready.unwrap();
    tokio::task::yield_now().await;
    assert!(
        previous.upgrade().is_none(),
        "established UDP retained its App"
    );
    for body in [vec![], vec![7; 512]] {
        // Concurrent read keeps the deliberately tiny test channel bounded.
        let (mut read, mut write) = tokio::io::split(&mut host);
        let (_, response) =
            tokio::join!(send_datagram(&mut write, &body), receive_reply(&mut read));
        let (remote, payload) = response.unwrap();
        assert_eq!(remote, destination);
        assert_eq!(payload, body);
    }
    drop(host);
    assert!(timeout(Duration::from_secs(5), worker)
        .await
        .unwrap()
        .unwrap()
        .is_err());
    echo.await.unwrap();
}

#[tokio::test]
async fn generations_share_udp_quota_until_the_lease_connection_closes() {
    use super::super::{Generation, State};
    use std::sync::atomic::{AtomicU64, AtomicUsize};
    let app = app();
    let current = Arc::new(arc_swap::ArcSwap::from(app.clone()));
    let mut generations = std::collections::BTreeMap::new();
    for id in 1..=2 {
        // Short-lived test-harness children supply real process handles;
        // no worker setup, environment mutation or network takeover occurs.
        let child = tokio::process::Command::new(std::env::current_exe().unwrap())
            .arg("--list")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        generations.insert(
            id,
            Arc::new(Generation {
                id,
                directory: std::env::temp_dir(),
                log: std::env::temp_dir().join(format!("unused-worker-{id}.log")),
                endpoint: std::env::temp_dir().join(format!("unused-worker-{id}")),
                token: format!("test-generation-{id}"),
                child: tokio::sync::Mutex::new(crate::platform::worker::WorkerChild::Normal(
                    Box::new(child),
                )),
                flows: AtomicUsize::new(0),
                rule_socket: String::new(),
                snapshot: Default::default(),
                failed: Default::default(),
                route_leases: Default::default(),
            }),
        );
    }
    let host = Arc::new(Host {
        consistency: Default::default(),
        boot: "quota-test".into(),
        next_flow: AtomicU64::new(1),
        endpoint: std::env::temp_dir().join(format!("astra-quota-test-{}", uuid::Uuid::new_v4())),
        path: std::env::temp_dir().join("hosted-udp-test.toml"),
        current: Arc::downgrade(&current),
        state: Arc::new(tokio::sync::Mutex::new(State {
            active: 2,
            prepared: None,
            paused: false,
            generations,
        })),
        admissions: Default::default(),
        transition: Default::default(),
        telemetry: Default::default(),
        traffic_history: Default::default(),
        connection_ids: Default::default(),
        retired: Default::default(),
        tokens: Arc::new(std::sync::Mutex::new(
            [
                "test-generation-1".to_owned(),
                "test-generation-2".to_owned(),
            ]
            .into_iter()
            .collect(),
        )),
        mappings: Arc::new(astra_dns::LocalMappingStore::default()),
        sequence: AtomicU64::new(3),
        tasks: Default::default(),
    });
    let admission = Arc::new(tokio::sync::Semaphore::new(1));
    let lease = |host: Arc<Host>, token: &'static str, admission: Arc<tokio::sync::Semaphore>| async move {
        let (mut client, mut server) = tokio::io::duplex(4096);
        let permit = admission.acquire_owned().await.unwrap();
        let task =
            tokio::spawn(async move { host.hold_udp_permit(&mut server, token, permit).await });
        let ready: std::result::Result<(), String> = receive(&mut client).await.unwrap();
        (client, task, ready)
    };
    let (first, first_task, ready) =
        lease(host.clone(), "test-generation-1", admission.clone()).await;
    ready.unwrap();
    // An idle quota connection must leave management admission available.
    let (second, second_task, rejected) = timeout(
        Duration::from_secs(5),
        lease(host.clone(), "test-generation-2", admission.clone()),
    )
    .await
    .unwrap();
    assert!(rejected.unwrap_err().contains("UDP flow limit"));
    drop(second);
    assert!(second_task.await.unwrap().is_err());
    assert!(app.udp_permits.clone().try_acquire_owned().is_err());
    drop(first);
    first_task.await.unwrap().unwrap();
    let (second, second_task, ready) = lease(host.clone(), "test-generation-2", admission).await;
    ready.unwrap();
    drop(second);
    second_task.await.unwrap().unwrap();
    assert!(app.udp_permits.clone().try_acquire_owned().is_ok());
}

#[tokio::test]
async fn replies_keep_ipv6_scope_empty_and_maximum_payloads() {
    let remote: SocketAddr = "[fe80::123%7]:443".parse().unwrap();
    let (mut writer, mut reader) = tokio::io::duplex(128);
    let task = tokio::spawn(async move {
        send_reply(&mut writer, remote, &[]).await.unwrap();
        send_reply(
            &mut writer,
            remote,
            &vec![123; astra_ipc::host::MAX_DATAGRAM_BYTES],
        )
        .await
        .unwrap();
    });
    let (address, body) = receive_reply(&mut reader).await.unwrap();
    assert_eq!(address, remote);
    assert!(body.is_empty());
    let (address, body) = receive_reply(&mut reader).await.unwrap();
    assert_eq!(address, remote);
    assert_eq!(body, vec![123; astra_ipc::host::MAX_DATAGRAM_BYTES]);
    task.await.unwrap();
}
