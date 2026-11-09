use super::*;
use std::sync::atomic::AtomicBool;
async fn fixture(
    changed: bool,
) -> (
    tempfile::TempDir,
    Arc<Hub>,
    Arc<crate::network::Scheduler>,
    Arc<AtomicBool>,
    Arc<AtomicBool>,
    tokio::task::JoinHandle<()>,
) {
    let root = tempfile::Builder::new()
        .prefix("group-")
        .tempdir_in("/tmp")
        .unwrap();
    private_dir(&root.path().join("state")).unwrap();
    let manager = Manager::load_with_runtime(
        root.path().join("state"),
        root.path().join("config"),
        root.path().join("bin"),
        Some(root.path().join("runtime")),
    )
    .unwrap();
    let hub = Hub::new(&manager).unwrap();
    let db = crate::database::Database::open(root.path().join("jobs.sqlite"), 10000).unwrap();
    let scheduler = crate::network::Scheduler::new(db, root.path().into());
    hub.network_scheduler.set(scheduler.clone()).ok().unwrap();
    let socket = root.path().join("core.sock");
    let listener = astra_platform::platform::bind_listener(&socket).unwrap();
    let identity = astra_platform::lifecycle::identity(None).unwrap();
    let pid = identity.process.pid;
    hub.base.send_modify(|base| {
        base.routes.insert(
            "proxy".into(),
            Route {
                instance: Instance {
                    name: "proxy".into(),
                    kind: Kind::Proxy,
                    config: root.path().join("profile"),
                },
                pid: Some(pid),
                socket: socket.clone(),
            },
        );
    });
    let started = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let (seen, done) = (started.clone(), release.clone());
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = astra_ipc::receive::<astra_model::control::Request>(&mut stream)
                .await
                .unwrap();
            match request.command {
                astra_model::control::Command::Identity => {
                    let mut value = identity.clone();
                    if changed && seen.load(Ordering::SeqCst) {
                        value.boot_id = "changed".into();
                    }
                    astra_ipc::send_management(&mut stream, &value)
                        .await
                        .unwrap();
                }
                astra_model::control::Command::Session => {
                    astra_ipc::send_management(
                        &mut stream,
                        &astra_ipc::session::Event::<astra_model::control::Response>::Ready(
                            astra_ipc::API_VERSION,
                        ),
                    )
                    .await
                    .unwrap();
                    let request =
                        astra_ipc::receive_management::<astra_model::control::Request>(&mut stream)
                            .await
                            .unwrap();
                    if matches!(
                        request.command,
                        astra_model::control::Command::Runtime(runtime::Command::TestGroup(_))
                    ) {
                        seen.store(true, Ordering::SeqCst);
                    }
                    let snapshot = runtime::Snapshot {
                        pid,
                        groups: vec![runtime::Group {
                            name: "manual".into(),
                            mode: "select".into(),
                            selected: None,
                            testing: seen.load(Ordering::SeqCst) && !done.load(Ordering::SeqCst),
                            members: vec![],
                        }],
                        ..Default::default()
                    };
                    astra_ipc::send_management(
                        &mut stream,
                        &astra_ipc::session::Event::Reply(astra_model::control::Response::Runtime(
                            Ok(snapshot),
                        )),
                    )
                    .await
                    .unwrap();
                }
                command => panic!("unexpected {command:?}"),
            }
        }
    });
    (root, hub, scheduler, started, release, server)
}
#[tokio::test]
async fn manual_group_test_retains_nat_gate_until_same_core_reports_idle() {
    let (_root, hub, scheduler, started, release, server) = fixture(false).await;
    let work = tokio::spawn(async move {
        hub.manual_group_test("proxy".into(), "manual".into(), None)
            .await
    });
    timeout(Duration::from_secs(3), async {
        while !started.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let (ready, mut nat) = watch::channel(false);
    scheduler
        .submit_mode(
            "nat".into(),
            "nat".into(),
            true,
            move |_, mut cancel| async move {
                ready.send_replace(true);
                crate::network::cancelled(&mut cancel).await;
                Ok(())
            },
        )
        .await
        .unwrap();
    assert!(timeout(Duration::from_millis(30), nat.wait_for(|v| *v))
        .await
        .is_err());
    release.store(true, Ordering::SeqCst);
    timeout(Duration::from_secs(3), work)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    timeout(Duration::from_secs(3), nat.wait_for(|v| *v))
        .await
        .unwrap()
        .unwrap();
    scheduler.cancel("nat").await.unwrap();
    server.abort();
    let _ = server.await;
}
#[tokio::test]
async fn changed_group_test_identity_blocks_future_nat_without_stopping_user_core() {
    let (_root, hub, scheduler, _started, _release, server) = fixture(true).await;
    let error = hub
        .manual_group_test("proxy".into(), "manual".into(), None)
        .await
        .unwrap_err();
    assert!(error
        .downcast_ref::<crate::network::CleanupUnconfirmed>()
        .is_some());
    assert!(scheduler
        .submit_mode("nat".into(), "nat".into(), true, |_, _| async {
            panic!("NAT cannot follow unconfirmed group cleanup")
        })
        .await
        .is_err());
    assert_eq!(unsafe { libc::kill(std::process::id() as i32, 0) }, 0);
    server.abort();
    let _ = server.await;
}
