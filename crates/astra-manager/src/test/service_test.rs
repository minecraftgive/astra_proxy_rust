use super::*;
use astra_management::ManagerClient;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::oneshot,
    task::JoinSet,
    time::{sleep, timeout},
};

async fn request(client: &ManagerClient, id: &str, body: RequestBody) -> ResponseBody {
    timeout(
        Duration::from_secs(10),
        client.request_v2(Request::new(id, body)),
    )
    .await
    .expect("management request timed out")
    .expect("management transport failed")
    .result
    .expect("management operation failed")
}

async fn task(client: &ManagerClient) -> Task {
    let ResponseBody::Task(task) = request(
        client,
        "observe-task",
        RequestBody::Query(Query::Task {
            operation_id: "refresh-once".into(),
        }),
    )
    .await
    else {
        panic!("expected a persisted task");
    };
    task
}

#[tokio::test]
#[ignore = "requires freshly built core binaries"]
async fn live_accepted_execute_survives_frontend_disconnect_and_is_not_replayed() {
    let binaries = std::env::var_os("ASTRA_UPDATE_TEST_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_exe()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .to_owned()
        });
    assert!(
        binaries
            .join(astra_platform::platform::executable("astra-proxy"))
            .is_file(),
        "build astra-proxy and set ASTRA_UPDATE_TEST_BIN to its binary directory"
    );
    // macOS's default /var/folders temporary path can exceed sun_path after
    // adding service directories; never use a shared or predictable endpoint.
    let temporary = if cfg!(unix) {
        tempfile::Builder::new().prefix("ad-").tempdir_in("/tmp")
    } else {
        tempfile::Builder::new().prefix("ad-").tempdir()
    }
    .unwrap();
    crate::private_dir(temporary.path()).unwrap();
    let config = ManagerConfig {
        state_dir: temporary.path().join("state"),
        config_dir: temporary.path().join("config"),
        runtime_dir: Some(temporary.path().join("run")),
        bin_dir: binaries,
        ..ManagerConfig::default()
    };
    let endpoint = config.endpoint();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let observed_hits = hits.clone();
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let mut children: JoinSet<Result<()>> = JoinSet::new();
    children.spawn(async move {
        let mut entered = Some(entered_tx);
        let mut release = Some(release_rx);
        loop {
            let (mut stream, _) = listener.accept().await?;
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                anyhow::ensure!(headers.len() < 8192, "oversized fixture request");
                headers.push(stream.read_u8().await?);
            }
            observed_hits.fetch_add(1, Ordering::SeqCst);
            if let Some(entered) = entered.take() {
                entered.send(()).unwrap();
                release.take().unwrap().await.unwrap();
            }
            let body = "proxies: [{name: fetched, type: http, server: 127.0.0.1, port: 1234}]";
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await?;
        }
    });
    let service = children.spawn(run_config(config));
    timeout(Duration::from_secs(10), async {
        while !endpoint.exists() {
            assert!(!service.is_finished(), "manager exited before binding IPC");
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let client = ManagerClient::new(endpoint.clone());
    let source = temporary.path().join("proxy.toml");
    let text = format!("standalone=true\nsocks5_listen='127.0.0.1:1080'\n[subscriptions.remote]\nurl='http://{address}/sub'\nproxy='DIRECT'\n");
    std::fs::write(&source, &text).unwrap();
    request(
        &client,
        "register",
        RequestBody::Execute {
            operation_id: "register-proxy".into(),
            command: Command::Add {
                name: "proxy".into(),
                kind: InstanceKind::Proxy,
                config: source.clone(),
            },
        },
    )
    .await;
    let refresh = RequestBody::Execute {
        operation_id: "refresh-once".into(),
        command: Command::UpdateSubscriptions {
            instance: "proxy".into(),
        },
    };
    let original_client = client.clone();
    let original_request = Request::new("original-frontend", refresh.clone());
    let frontend = tokio::spawn(async move { original_client.request_v2(original_request).await });
    // The real core has reached HTTP and the task is durably Running before
    // losing observation. No sleep determines when the client is disconnected.
    timeout(Duration::from_secs(10), entered_rx)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(task(&client).await.state, TaskState::Running));
    frontend.abort();
    assert!(frontend.await.unwrap_err().is_cancelled());
    assert!(matches!(task(&client).await.state, TaskState::Running));
    release_tx.send(()).unwrap();
    let finished = timeout(Duration::from_secs(15), async {
        loop {
            let task = task(&client).await;
            if !matches!(task.state, TaskState::Running) {
                break task;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        matches!(finished.state, TaskState::Complete),
        "{finished:?}"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    let nodes = request(
        &client,
        "read-refreshed-nodes",
        RequestBody::Query(Query::SubscriptionNodes {
            instance: "proxy".into(),
        }),
    )
    .await;
    assert!(
        matches!(nodes, ResponseBody::SubscriptionNodes(nodes) if nodes["remote"].tags == ["fetched"])
    );
    let replay = request(&client, "reconnected-frontend", refresh).await;
    assert!(matches!(replay, ResponseBody::Done { message } if message.contains("Updated 1")));
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "operation was executed twice"
    );
    assert_eq!(
        serde_json::to_value(task(&client).await).unwrap(),
        serde_json::to_value(finished).unwrap(),
        "reconciliation must preserve the original durable outcome"
    );
    assert_eq!(std::fs::read_to_string(&source).unwrap(), text);
    request(
        &client,
        "shutdown",
        RequestBody::Execute {
            operation_id: "stop-manager".into(),
            command: Command::Shutdown,
        },
    )
    .await;
    timeout(Duration::from_secs(5), children.join_next())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(service.is_finished());
    assert!(!endpoint.exists());
    children.shutdown().await;
}

/// Exercise real IPC and an owned diagnostic process without a prebuilt core or
/// external network. The FIFO holds the subprocess at a deterministic checkpoint.
#[cfg(unix)]
#[tokio::test]
async fn network_job_survives_ipc_disconnect_and_reconnect_does_not_spawn_twice() {
    use std::os::unix::{ffi::OsStrExt, fs::PermissionsExt};
    let temporary = tempfile::Builder::new()
        .prefix("nd-")
        .tempdir_in("/tmp")
        .unwrap();
    let canonical_root = std::fs::canonicalize(temporary.path()).unwrap();
    let root = canonical_root.as_path();
    let config = ManagerConfig {
        state_dir: root.join("state"),
        config_dir: root.join("config"),
        runtime_dir: Some(root.join("runtime")),
        bin_dir: root.join("bin"),
        ..Default::default()
    };
    for directory in [&config.state_dir, &config.config_dir, &config.bin_dir] {
        crate::private_dir(directory).unwrap();
    }
    let source = config.config_dir.join("proxy.toml");
    std::fs::write(&source, "standalone=true\nauto_start=false\n").unwrap();
    #[derive(serde::Serialize)]
    struct RegistryFixture {
        instances: Vec<crate::protocol::Instance>,
    }
    std::fs::write(
        config.config_dir.join("instances.toml"),
        toml::to_string(&RegistryFixture {
            instances: vec![crate::protocol::Instance {
                name: "proxy".into(),
                kind: crate::protocol::Kind::Proxy,
                config: source.clone(),
            }],
        })
        .unwrap(),
    )
    .unwrap();
    let gate_path = source.with_extension("toml.gate");
    let gate_name = std::ffi::CString::new(gate_path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(gate_name.as_ptr(), 0o600) }, 0);
    // Holding both ends lets the test release the read without creating a
    // blocking helper task, including on early test failure.
    let mut gate = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&gate_path)
        .unwrap();
    let binary = config.bin_dir.join("astra-proxy");
    let mut info = astra_platform::program_info::ProgramInfo::collect(
        astra_platform::build_info!(),
        vec![astra_platform::program_info::FeatureSupport::new(
            astra_model::control::NETWORK_DIAGNOSTICS_STREAM_CAPABILITY,
            "diagnostic",
            true,
            "fixture",
        )],
    );
    info.build.name = "astra-proxy".into();
    let metadata = serde_json::to_string(&info).unwrap().replace('\'', "'\\''");
    let script = format!(
        "#!/bin/sh\nif [ \"$1\" = --program-info ]; then printf '%s\\n' '{metadata}'; exit 0; fi\n"
    );
    std::fs::write(&binary, script + r#"
set -eu
[ "$1" = "--network-diagnostics-stream" ]
IFS= read -r payload || :
case "$payload" in
  *'"config":""'*) ;;
  *) printf 'system probe unexpectedly loaded a profile' >&2; exit 3 ;;
esac
printf '%s\n' "$$" >> "$2.launches"
printf '%s\n' '{"event":"started","egress":"system","probe":"TCP local fixture","note":"local process fixture","running_config":false}'
printf '%s\n' '{"event":"attempt","seq":1,"rtt_ms":1.0,"detail":"before disconnect","error":null}'
IFS= read -r release < "$2.gate"
printf '%s\n' '{"event":"attempt","seq":2,"rtt_ms":1.0,"detail":"after reconnect","error":null}'
printf '%s\n' '{"event":"summary","sent":2,"received":2,"min_ms":1.0,"avg_ms":1.0,"max_ms":1.0}'
printf '%s\n' '{"event":"done"}'
"#).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let endpoint = config.endpoint();
    let mut service = JoinSet::new();
    let running = service.spawn(run_config(config));
    timeout(Duration::from_secs(5), async {
        while !endpoint.exists() {
            assert!(
                !running.is_finished(),
                "manager stopped before IPC was ready"
            );
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let client = ManagerClient::new(endpoint.clone());
    // Ensure the actor initialized, not just the IPC listener.
    request(&client, "ready", RequestBody::Query(Query::Hello)).await;
    let command = RequestBody::Execute {
        operation_id: "network-once".into(),
        command: Command::NetworkDiagnostics {
            instance: "proxy".into(),
            request: NetToolRequest {
                egress: astra_model::nettool::Egress::System,
                probe: astra_model::nettool::Probe::Tcp {
                    host: "127.0.0.1".into(),
                    port: 9,
                },
                count: 2,
                interval_ms: "0".into(),
                timeout_ms: "100".into(),
            }
            .into(),
        },
    };
    let mut connection = astra_platform::platform::connect_trusted(&endpoint)
        .await
        .unwrap();
    astra_management::client::send(
        &mut connection,
        &Request::new("disconnected-frontend", command.clone()),
    )
    .await
    .unwrap();
    // Disconnect before reading the acceptance; the operation ID is the only
    // durable correlation the reconnecting frontend has.
    drop(connection);
    let report = timeout(Duration::from_secs(5), async {
        loop {
            let response = client
                .request_v2(Request::new(
                    "poll",
                    RequestBody::Query(Query::NetworkReport {
                        operation_id: "network-once".into(),
                        cursor: None,
                    }),
                ))
                .await
                .unwrap();
            if let Ok(ResponseBody::NetworkReport(report)) = response.result {
                assert!(
                    !report.state.terminal(),
                    "fixture stopped before release: {report:?}"
                );
                if report
                    .items
                    .iter()
                    .any(|item| item.data["event"] == "attempt")
                {
                    break report;
                }
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(report.state, NetworkState::Running);
    let launches = source.with_extension("toml.launches");
    let original_launch = std::fs::read_to_string(&launches).unwrap();
    let pid: i32 = original_launch.trim().parse().unwrap();
    assert_eq!(unsafe { libc::kill(pid, 0) }, 0);
    let repeated = request(&client, "reconnect", command.clone()).await;
    assert!(matches!(
        repeated,
        ResponseBody::Task(Task {
            state: TaskState::Running,
            ..
        })
    ));
    assert_eq!(std::fs::read_to_string(&launches).unwrap(), original_launch);
    // A real serialized configuration operation still completes while the
    // diagnostic subprocess remains held at its checkpoint.
    let clone = source.parent().unwrap().join("copy.toml");
    request(
        &client,
        "clone-config",
        RequestBody::Execute {
            operation_id: "clone-during-diagnostic".into(),
            command: Command::CloneConfig {
                instance: "proxy".into(),
                destination: clone.clone(),
            },
        },
    )
    .await;
    assert_eq!(
        std::fs::read(&clone).unwrap(),
        std::fs::read(&source).unwrap()
    );
    std::io::Write::write_all(&mut gate, b"release\n").unwrap();
    let finished = timeout(Duration::from_secs(5), async {
        loop {
            let ResponseBody::NetworkReport(report) = request(
                &client,
                "poll-finish",
                RequestBody::Query(Query::NetworkReport {
                    operation_id: "network-once".into(),
                    cursor: None,
                }),
            )
            .await
            else {
                panic!("expected report")
            };
            if report.state.terminal() {
                break report;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(finished.state, NetworkState::Complete);
    assert_eq!(
        finished
            .items
            .iter()
            .filter(|item| item.data["event"] == "attempt")
            .count(),
        2
    );
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        -1,
        "complete must imply process reaped"
    );
    let repeated = request(&client, "repeat-completed", command).await;
    assert!(matches!(
        repeated,
        ResponseBody::Task(Task {
            state: TaskState::Complete,
            ..
        })
    ));
    assert_eq!(std::fs::read_to_string(&launches).unwrap(), original_launch);
    request(
        &client,
        "shutdown",
        RequestBody::Execute {
            operation_id: "shutdown-fixture".into(),
            command: Command::Shutdown,
        },
    )
    .await;
    timeout(Duration::from_secs(5), service.join_next())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(running.is_finished());
    assert!(!endpoint.exists());
}
