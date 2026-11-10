use super::*;

impl NativeClient {
    /// Exercise the real Agent -> NativeClient -> Work path without launching
    /// cores, querying SCM, or invoking UAC.
    pub(crate) fn with_test_runtime_actor(
        home: PathBuf,
        mut handle: impl FnMut(Action, bool) -> Result<Reply> + Send + 'static,
    ) -> Result<(Self, tokio::task::JoinHandle<()>)> {
        astra_platform::storage::private_dir(&home.join("state"))?;
        let manager = Manager::load_with_runtime(
            home.join("state"),
            home.join("config"),
            home.join("bin"),
            Some(home.join("runtime")),
        )?;
        let hub = Hub::new(&manager)?;
        let (sender, mut requests) = mpsc::channel::<Work>(8);
        let (stopped, _stop) = mpsc::channel(1);
        let task = tokio::spawn(async move {
            while let Some(work) = requests.recv().await {
                assert!(work.save.is_none());
                let _ = work.reply.send(handle(work.action, work.allow_runtime_uac));
            }
        });
        Ok((
            Self {
                hub,
                sender,
                stopped,
                diagnostics: Arc::new(Semaphore::new(1)),
                probes: Default::default(),
            },
            task,
        ))
    }
}

#[tokio::test]
async fn in_process_actor_preserves_typed_errors_and_context() {
    let home = tempfile::tempdir().unwrap();
    let (client, actor) =
        NativeClient::with_test_runtime_actor(home.path().to_path_buf(), |_, _| {
            Err(
                crate::error::message(astra_management::v2::ErrorCode::Conflict, "名称已被使用")
                    .context("native action"),
            )
        })
        .unwrap();
    let error = client
        .call(Action::Start("proxy".into()))
        .await
        .unwrap_err();
    assert!(matches!(
        crate::error::code(&error),
        astra_management::v2::ErrorCode::Conflict
    ));
    assert!(format!("{error:#}").contains("名称已被使用"));
    actor.abort();
}

#[tokio::test]
async fn ordinary_dns_telemetry_survives_runtime_query_and_live_subscription() {
    use astra_model::runtime::{
        Coverage, DnsQuery, DnsQueryLog, DnsResolverStatus, RuntimeTelemetry, RuntimeView, Snapshot,
    };
    let home = tempfile::tempdir().unwrap();
    let (client, actor) =
        NativeClient::with_test_runtime_actor(home.path().to_path_buf(), |_, _| {
            panic!("read must not reach mutation actor")
        })
        .unwrap();
    let core_socket = home.path().join("c.sock");
    let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let failure = fail.clone();
    let sequence = AtomicU64::new(0);
    let core = control::Control::bind(Some(core_socket.clone()))
        .unwrap()
        .with_runtime_telemetry(move || {
            let fail = failure.load(Ordering::Relaxed);
            let sequence = sequence.fetch_add(1, Ordering::Relaxed) + 1;
            async move {
                if fail {
                    return Err("sample failed".into());
                }
                serde_json::to_string(&RuntimeView {
                    snapshot: Snapshot {
                        pid: std::process::id(),
                        ..Default::default()
                    },
                    telemetry: Some(RuntimeTelemetry {
                        host_boot_id: astra_platform::lifecycle::boot_id().into(),
                        active_generation: Some(1),
                        sample_sequence: sequence,
                        coverage: Coverage::Complete,
                        dns: Some(Box::new(DnsQueryLog {
                            records: vec![DnsQuery {
                                domain: "ordinary.example".into(),
                                ..Default::default()
                            }],
                            truncated: false,
                        })),
                        dns_resolver: Some(Box::new(DnsResolverStatus {
                            enabled: true,
                            ..Default::default()
                        })),
                        ..Default::default()
                    }),
                })
                .map_err(|error| error.to_string())
            }
        });
    let core_task = tokio::spawn(async move { core.shutdown().await.unwrap() });
    client.hub.base.send_modify(|base| {
        base.routes.insert(
            "proxy".into(),
            Route {
                instance: Instance {
                    name: "proxy".into(),
                    kind: Kind::Proxy,
                    config: home.path().join("proxy.toml"),
                },
                pid: Some(std::process::id()),
                socket: core_socket.clone(),
            },
        );
    });
    let value = client.runtime_sample("proxy").await.unwrap();
    assert_eq!(
        value["telemetry"]["dns"]["records"][0]["domain"],
        "ordinary.example"
    );
    assert_eq!(value["telemetry"]["dns_resolver"]["enabled"], true);
    let socket = home.path().join("m.sock");
    let listener = astra_platform::platform::bind_listener(&socket).unwrap();
    let watcher = {
        let client = client.clone();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            serve(
                stream,
                client.hub,
                client.sender,
                client.stopped,
                client.diagnostics,
                Arc::new(Semaphore::new(1)),
            )
            .await
            .unwrap();
        })
    };
    let mut stream = astra_platform::platform::connect_trusted(&socket)
        .await
        .unwrap();
    astra_ipc::send_management(
        &mut stream,
        &Request {
            version: astra_ipc::API_VERSION,
            action: Action::WatchRuntime {
                name: "proxy".into(),
                expected_pid: std::process::id(),
                interval_ms: 100,
            },
        },
    )
    .await
    .unwrap();
    let mut previous = 0;
    for _ in 0..2 {
        let reply: Reply = timeout(
            Duration::from_secs(3),
            astra_ipc::receive_management(&mut stream),
        )
        .await
        .unwrap()
        .unwrap();
        let view: RuntimeView = serde_json::from_str(&reply.text).unwrap();
        let meta = view.telemetry.unwrap();
        assert!(meta.sample_sequence > previous);
        previous = meta.sample_sequence;
        assert_eq!(meta.dns.unwrap().records[0].domain, "ordinary.example");
    }
    drop(stream);
    watcher.await.unwrap();
    fail.store(true, Ordering::Relaxed);
    assert!(
        format!("{:#}", client.runtime_sample("proxy").await.unwrap_err())
            .contains("sample failed"),
        "supported telemetry failures must not silently fall back to old snapshots"
    );
    control::request(&core_socket, astra_model::control::Command::Stop)
        .await
        .unwrap();
    core_task.await.unwrap();
    actor.abort();
}
