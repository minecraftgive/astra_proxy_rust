use super::*;
use astra_management::ManagerClient;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    time::{sleep, timeout},
};

async fn request(client: &ManagerClient, body: RequestBody) -> Result<ResponseBody> {
    let response = client
        .request_v2(Request::new(uuid::Uuid::new_v4().to_string(), body))
        .await?;
    response
        .result
        .map_err(|e| anyhow::anyhow!("{:?}: {}", e.code, e.message))
}
async fn execute(client: &ManagerClient, command: Command) -> Result<ResponseBody> {
    request(
        client,
        RequestBody::Execute {
            operation_id: uuid::Uuid::new_v4().to_string(),
            command,
        },
    )
    .await
}
async fn ready(client: &ManagerClient) -> Result<Vec<Instance>> {
    timeout(Duration::from_secs(20), async {
        loop {
            if let Ok(ResponseBody::Instances(rows)) =
                request(client, RequestBody::Query(Query::Instances)).await
            {
                return Ok::<_, anyhow::Error>(rows);
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await?
}

#[tokio::test]
#[ignore = "requires freshly built manager, rule and proxy binaries"]
async fn live_manager_crash_preserves_committed_cores_and_reaps_uncommitted_candidate() {
    let bins = std::env::var_os("ASTRA_UPDATE_TEST_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_exe()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .into()
        });
    let temporary = if cfg!(unix) {
        tempfile::Builder::new().prefix("acr-").tempdir_in("/tmp")
    } else {
        tempfile::Builder::new().prefix("acr-").tempdir()
    }
    .unwrap();
    crate::private_dir(temporary.path()).unwrap();
    let root = temporary.path();
    let config = ManagerConfig {
        state_dir: root.join("state"),
        config_dir: root.join("config"),
        runtime_dir: Some(root.join("runtime")),
        bin_dir: bins.clone(),
        ..Default::default()
    };
    let config_path = root.join("manager.toml");
    std::fs::write(&config_path, toml::to_string(&config).unwrap()).unwrap();
    let manager_exe = bins.join(astra_platform::platform::executable("astra-manager"));
    let spawn = || {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join("manager.log"))
            .unwrap();
        let mut command = tokio::process::Command::new(&manager_exe);
        command
            .arg("--config")
            .arg(&config_path)
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .kill_on_drop(true);
        astra_platform::platform::background_command(command.as_std_mut(), true);
        command.spawn().unwrap()
    };
    let mut manager = spawn();
    let client = ManagerClient::new(config.endpoint());
    let mut candidate = None;
    let mut identities = Vec::new();
    let echo = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = echo.local_addr().unwrap();
    let echo_task = tokio::spawn(async move {
        let (mut stream, _) = echo.accept().await.unwrap();
        let (mut read, mut write) = stream.split();
        tokio::io::copy(&mut read, &mut write).await
    });
    let result: Result<()> = async {
        ready(&client).await?;
        let rule = root.join("rules.toml");
        let route = root.join("route.sock");
        std::fs::write(
            &rule,
            format!(
                "socket={}\ndefault_action='direct'\n",
                toml::Value::String(route.to_string_lossy().into()).to_string()
            ),
        )?;
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let proxy_addr = listener.local_addr()?;
        drop(listener);
        let proxy = root.join("proxy.toml");
        std::fs::write(
            &proxy,
            format!(
                "rule_socket={}\nsocks5_listen='{proxy_addr}'\ndecision_cache_ttl_secs=0\n",
                toml::Value::String(route.to_string_lossy().into()).to_string()
            ),
        )?;
        for (name, kind, path) in [
            ("rules", InstanceKind::Rule, rule),
            ("proxy", InstanceKind::Proxy, proxy),
        ] {
            execute(
                &client,
                Command::Add {
                    name: name.into(),
                    kind,
                    config: path,
                },
            )
            .await?;
            execute(
                &client,
                Command::Start {
                    instance: name.into(),
                },
            )
            .await?;
            identities.push((
                config.state_dir.join(format!("{name}.sock")),
                astra_ipc::control::identity(&config.state_dir.join(format!("{name}.sock")))
                    .await?,
            ));
        }
        let mut stream = tokio::net::TcpStream::connect(proxy_addr).await?;
        stream.write_all(&[5, 1, 0]).await?;
        let mut auth = [0; 2];
        stream.read_exact(&mut auth).await?;
        anyhow::ensure!(auth == [5, 0], "SOCKS authentication failed");
        let mut connect = vec![5, 1, 0, 1, 127, 0, 0, 1];
        connect.extend(target.port().to_be_bytes());
        stream.write_all(&connect).await?;
        let mut reply = [0; 10];
        stream.read_exact(&mut reply).await?;
        anyhow::ensure!(reply[1] == 0, "SOCKS connect failed");
        stream.write_all(b"before").await?;
        let mut before = [0; 6];
        stream.read_exact(&mut before).await?;
        anyhow::ensure!(&before == b"before", "initial echo mismatch");
        let candidate_config = root.join("candidate.toml");
        let candidate_listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let candidate_addr = candidate_listener.local_addr()?;
        drop(candidate_listener);
        std::fs::write(
            &candidate_config,
            format!(
                "rule_socket={}\nsocks5_listen='{candidate_addr}'\n",
                toml::Value::String(route.to_string_lossy().into())
            ),
        )?;
        let candidate_endpoint = root.join("candidate.sock");
        let log = std::fs::File::create(root.join("candidate.log"))?;
        candidate = Some(
            tokio::process::Command::new(
                bins.join(astra_platform::platform::executable("astra-proxy")),
            )
            .arg(candidate_config)
            .env("ASTRA_CONTROL_SOCKET", &candidate_endpoint)
            .env("ASTRA_RUNTIME_DIR", root.join("candidate-runtime"))
            .env(
                "ASTRA_SERVICE_OWNER",
                serde_json::to_string(&astra_platform::lifecycle::capture(manager.id().unwrap())?)?,
            )
            .stdout(log.try_clone()?)
            .stderr(log)
            .kill_on_drop(true)
            .spawn()?,
        );
        timeout(Duration::from_secs(10), async {
            while astra_ipc::control::identity(&candidate_endpoint)
                .await
                .is_err()
            {
                if let Some(status) = candidate.as_mut().unwrap().try_wait()? {
                    anyhow::bail!(
                        "candidate exited {status}: {}",
                        std::fs::read_to_string(root.join("candidate.log"))?
                    );
                }
                sleep(Duration::from_millis(30)).await;
            }
            Ok::<(), anyhow::Error>(())
        })
        .await
        .context("candidate identity readiness")??;
        manager.kill().await?;
        let exited = timeout(Duration::from_secs(10), candidate.as_mut().unwrap().wait()).await??;
        anyhow::ensure!(
            !exited.success(),
            "uncommitted candidate did not fail after parent death"
        );
        for (endpoint, identity) in &identities {
            anyhow::ensure!(
                astra_ipc::control::identity(endpoint).await?.boot_id == identity.boot_id,
                "committed core restarted after manager death"
            );
        }
        stream.write_all(b"orphan").await?;
        let mut orphan = [0; 6];
        timeout(Duration::from_secs(5), stream.read_exact(&mut orphan)).await??;
        anyhow::ensure!(&orphan == b"orphan", "connection lost on manager crash");
        manager = spawn();
        let rows = ready(&client).await?;
        anyhow::ensure!(
            rows.len() == 2 && rows.iter().all(|row| row.pid.is_some()),
            "restart did not adopt both cores"
        );
        stream.write_all(b"adopted").await?;
        let mut after = [0; 7];
        timeout(Duration::from_secs(5), stream.read_exact(&mut after)).await??;
        anyhow::ensure!(&after == b"adopted", "connection lost during adoption");
        drop(stream);
        execute(&client, Command::Shutdown).await?;
        timeout(Duration::from_secs(15), manager.wait()).await??;
        for (_, identity) in &identities {
            anyhow::ensure!(
                !astra_platform::lifecycle::matches_live(&identity.process)?,
                "explicit shutdown left an adopted core alive"
            );
        }
        Ok(())
    }
    .await;
    let _ = manager.kill().await;
    if let Some(mut child) = candidate {
        let _ = child.kill().await;
    }
    for (endpoint, identity) in identities {
        let _ = astra_ipc::control::request(
            &endpoint,
            astra_model::control::Command::StopIdentified {
                boot_id: identity.boot_id,
                force: true,
            },
        )
        .await;
    }
    echo_task.abort();
    if let Err(error) = result {
        let directory = temporary.keep();
        panic!("{error:#}; diagnostics retained at {}", directory.display());
    }
}
