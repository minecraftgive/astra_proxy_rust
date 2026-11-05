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
