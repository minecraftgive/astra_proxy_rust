//! Real process maintenance regression; run with ASTRA_TEST_BIN_DIR after building roles.
use anyhow::{Context, Result};
use astra_management::{config::ManagerConfig, maintenance as m, v2::*, ManagerClient};
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::Duration,
};

struct Fixture {
    root: tempfile::TempDir,
    old: PathBuf,
    new: PathBuf,
    config: ManagerConfig,
    host: Child,
}
impl Fixture {
    async fn new(server: bool) -> Result<Self> {
        #[cfg(windows)]
        anyhow::ensure!(
            !astra_platform::windows_service::installed()?,
            "Run the unprivileged regression on an isolated host without the TUN service"
        );
        let root = tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir())?)?;
        let bin = PathBuf::from(
            std::env::var_os("ASTRA_TEST_BIN_DIR")
                .context("Build roles and set ASTRA_TEST_BIN_DIR")?,
        );
        let mut config = ManagerConfig {
            state_dir: root.path().join("state"),
            config_dir: root.path().join("config"),
            runtime_dir: Some(root.path().join("runtime")),
            ..Default::default()
        };
        std::fs::create_dir_all(&config.state_dir)?;
        std::fs::create_dir_all(&config.config_dir)?;
        let old = root.path().join("old");
        let new = root.path().join("new");
        for (directory, release) in [(&old, "maintenance-old"), (&new, "maintenance-new")] {
            std::fs::create_dir(directory)?;
            let components = if server {
                vec![
                    "astra-host",
                    "astra-manager",
                    "astra-server-core",
                    "astra-server-tui",
                ]
            } else {
                let mut names = vec![
                    "astra-host",
                    "astra-manager",
                    "astra-tui",
                    "astra-proxy",
                    "astra-rule",
                    "astra-convert",
                ];
                if cfg!(windows) {
                    names.push("astra-service");
                }
                names
            };
            let mut files = Vec::new();
            for name in &components {
                let filename = astra_platform::platform::executable(name);
                std::fs::copy(bin.join(&filename), directory.join(&filename))?;
                let bytes = std::fs::read(directory.join(&filename))?;
                files.push(serde_json::json!({"path":filename,"size":bytes.len(),"sha256":format!("{:x}",Sha256::digest(bytes))}));
            }
            std::fs::write(
                directory.join("manifest.json"),
                serde_json::to_vec(&serde_json::json!({
                    "schema_version":1,"role":if server {"server"}else{"client"},"release_id":release,"target":astra_update::TARGET,
                    "host_protocol":{"min":1,"max":1},"components":components.into_iter().map(|n|(n,"0.1.0")).collect::<std::collections::BTreeMap<_,_>>(),"files":files
                }))?,
            )?;
        }
        config.bin_dir = old.clone();
        let file = root.path().join("manager.toml");
        std::fs::write(&file, toml::to_string(&config)?)?;
        let log = std::fs::File::create(root.path().join("old.log"))?;
        let host = Command::new(old.join(astra_platform::platform::executable("astra-host")))
            .args(["--config"])
            .arg(file)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()?;
        let fixture = Self {
            root,
            old,
            new,
            config,
            host,
        };
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if fixture.query(Query::Hello).await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await?;
        Ok(fixture)
    }
    async fn request(&self, body: RequestBody) -> Result<ResponseBody> {
        let response = ManagerClient::new(self.config.endpoint())
            .request_v2(Request::new(uuid::Uuid::new_v4().to_string(), body))
            .await?;
        response.result.map_err(|e| anyhow::anyhow!(e.message))
    }
    async fn query(&self, q: Query) -> Result<ResponseBody> {
        self.request(RequestBody::Query(q)).await
    }
    async fn execute(&self, c: astra_management::v2::Command) -> Result<ResponseBody> {
        self.request(RequestBody::Execute {
            operation_id: uuid::Uuid::new_v4().to_string(),
            command: c,
        })
        .await
    }
    async fn prepare(&self) -> Result<m::Record> {
        let ResponseBody::Hello(hello) = self.query(Query::Hello).await? else {
            anyhow::bail!("hello")
        };
        let digest = format!(
            "{:x}",
            Sha256::digest(std::fs::read(self.new.join("manifest.json"))?)
        );
        let ResponseBody::MaintenanceStatus(Some(record)) = self
            .request(RequestBody::Local(LocalCommand::PrepareMaintenance {
                target_directory: self.new.clone(),
                expected_manifest_sha256: digest,
                expected_epoch: hello.epoch,
            }))
            .await?
        else {
            anyhow::bail!("prepare")
        };
        Ok(*record)
    }
    fn spawn_helper(&self, record: &m::Record, action: &str) -> Result<Child> {
        let log = std::fs::File::create(self.root.path().join(format!("helper-{action}.log")))?;
        Ok(Command::new(
            record
                .coordinator
                .join(astra_platform::platform::executable("astra-host")),
        )
        .arg(format!("--maintenance-{action}"))
        .arg(&self.config.state_dir)
        .arg(&record.operation_id)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()?)
    }
    async fn helper(&self, record: &m::Record, action: &str) -> Result<m::Record> {
        let mut child = self.spawn_helper(record, action)?;
        let result = tokio::time::timeout(Duration::from_secs(180), async {
            loop {
                if let Some(status) = child.try_wait()? {
                    return Ok::<_, anyhow::Error>(status);
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await;
        if result.is_err() {
            let _ = child.kill();
        }
        let _ = child.wait();
        result??;
        m::read(&self.config.state_dir)?.context("missing result")
    }
    async fn cleanup(&mut self) {
        if let Ok(Some(mut record)) = m::read(&self.config.state_dir) {
            record.phase = m::Phase::Cancelled;
            let _ = m::write(&record);
        }
        let _ = self.execute(astra_management::v2::Command::Shutdown).await;
        let _ = self.host.wait();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.host.kill();
        let _ = self.host.wait();
    }
}

#[tokio::test]
#[ignore = "requires built client/server packages"]
async fn switches_real_backend_and_restores_exact_instances_on_both_roles() -> Result<()> {
    for server in [false, true] {
        eprintln!(
            "Maintenance role: {}",
            if server { "server" } else { "client" }
        );
        let mut f = Fixture::new(server).await?;
        let result=async {
            for name in ["running","stopped"] {
                let config=f.config.config_dir.join(format!("{name}.toml"));
                let text=if server {"allow_private_targets=true\nshutdown_timeout_secs=0\n[[listeners]]\ntag=\"test\"\nprotocol=\"socks5\"\nlisten=\"127.0.0.1:0\"\n[[listeners.users]]\nname=\"test\"\npassword=\"test-pass\"\n"} else {"standalone=true\nsocks5_listen=\"127.0.0.1:0\"\nshutdown_timeout_secs=0\n[[outbounds]]\nkind=\"socks5\"\ntag=\"node\"\nserver=\"127.0.0.1:9\"\n"};
                let port = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
                std::fs::write(&config,text.replace("127.0.0.1:0", &format!("127.0.0.1:{port}")))?;
                f.execute(astra_management::v2::Command::Add{name:name.into(),kind:if server{InstanceKind::Server}else{InstanceKind::Proxy},config}).await?;
            }
            f.execute(astra_management::v2::Command::Start{instance:"running".into()}).await?;
            if !server {
                let config=f.config.config_dir.join("rules.toml");
                std::fs::write(&config, format!("socket={}\ndefault_action='direct'\n", toml::Value::String(f.root.path().join("routes.sock").to_string_lossy().into_owned())))?;
                f.execute(astra_management::v2::Command::Add { name:"rules".into(), kind:InstanceKind::Rule, config }).await?;
                for name in ["running", "stopped"] {
                    f.execute(astra_management::v2::Command::BindStart { instance:name.into(), targets:vec!["rules".into()] }).await?;
                }
                f.execute(astra_management::v2::Command::Start { instance:"rules".into() }).await?;
                f.execute(astra_management::v2::Command::Stop { instance:"stopped".into() }).await?;
                f.execute(astra_management::v2::Command::Runtime { instance:"running".into(), command:RuntimeCommand::SetMode(astra_model::runtime::Mode::Direct), expected_pid:None }).await?;
            }
            let record=f.prepare().await?;
            assert_eq!(record.instances.len(),if server {1} else {2});
            assert!(f.prepare().await.is_err(), "A second window cannot replace the frozen target");
            assert!(f.execute(astra_management::v2::Command::Start{instance:"stopped".into()}).await.is_err());
            let result=f.helper(&record,"run").await?;
            assert_eq!(result.phase,m::Phase::Complete,"{:?}; {}",result.error,f.root.path().display());
            assert!(f.host.try_wait()?.is_some());
            assert_ne!(result.old_epoch,result.new_epoch.clone().unwrap());
            assert_eq!(result.target,f.new);
            let ResponseBody::Instances(instances)=f.query(Query::Instances).await? else{anyhow::bail!("instances")};
            assert!(instances.iter().find(|i|i.name=="running").unwrap().pid.is_some());
            assert!(instances.iter().find(|i|i.name=="stopped").unwrap().pid.is_none());
            assert!(f.old.join("manifest.json").is_file());
            if !server {
                let ResponseBody::Runtime(RuntimeSnapshot::Client(runtime)) = f.query(Query::Runtime { instance:"running".into() }).await? else {anyhow::bail!("runtime")};
                assert_eq!(runtime["mode"], "direct");
                assert!(instances.iter().find(|i|i.name=="rules").unwrap().pid.is_some());
            }
            Ok::<_,anyhow::Error>(())
        }.await;
        if result.is_err() {
            eprintln!(
                "{}",
                std::fs::read_to_string(f.root.path().join("helper-run.log")).unwrap_or_default()
            );
        }
        f.cleanup().await;
        result?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires built client package"]
async fn invalid_package_keeps_old_host_and_requires_explicit_retry() -> Result<()> {
    let mut f = Fixture::new(false).await?;
    let record = f.prepare().await?;
    std::fs::write(
        f.new
            .join(astra_platform::platform::executable("astra-rule")),
        b"corrupt",
    )?;
    let failed = f.helper(&record, "run").await?;
    assert_eq!(failed.phase, m::Phase::Failed);
    assert!(f.host.try_wait()?.is_none());
    assert!(failed.new_process.is_none());
    std::fs::copy(
        f.old
            .join(astra_platform::platform::executable("astra-rule")),
        f.new
            .join(astra_platform::platform::executable("astra-rule")),
    )?;
    let retried = f.helper(&failed, "retry").await?;
    f.cleanup().await;
    assert_eq!(retried.phase, m::Phase::Complete, "{:?}", retried.error);
    Ok(())
}

#[tokio::test]
#[ignore = "requires built client package"]
async fn cancellation_and_lost_helper_never_start_a_second_backend() -> Result<()> {
    let mut f = Fixture::new(false).await?;
    let record = f.prepare().await?;
    // A prepared transaction without a running helper can be cancelled, and
    // cancelling a stale operation cannot cancel the next one.
    f.request(RequestBody::Local(LocalCommand::CancelMaintenance {
        operation_id: record.operation_id.clone(),
    }))
    .await?;
    assert_eq!(
        m::read(&f.config.state_dir)?.unwrap().phase,
        m::Phase::Cancelled
    );
    assert!(f.host.try_wait()?.is_none());
    let mut record = f.prepare().await?;
    assert!(f
        .request(RequestBody::Local(LocalCommand::CancelMaintenance {
            operation_id: "stale".into()
        }))
        .await
        .is_err());
    // Simulate a coordinator that exited before shutdown with its durable
    // process identity retained, exactly as recovery observes after a crash.
    let mut exited = Command::new(
        f.new
            .join(astra_platform::platform::executable("astra-host")),
    )
    .arg("--help")
    .stdout(Stdio::null())
    .spawn()?;
    let identity = astra_platform::lifecycle::capture(exited.id())?;
    exited.wait()?;
    record.helper = Some(identity);
    m::write(&record)?;
    assert!(f.host.try_wait()?.is_none());
    let result = f.helper(&record, "previous").await?;
    f.cleanup().await;
    assert_eq!(result.phase, m::Phase::Complete, "{:?}", result.error);
    assert_eq!(result.target, f.old);
    assert_eq!(result.release_id, "maintenance-old");
    Ok(())
}

#[tokio::test]
#[ignore = "requires built client package"]
async fn failed_new_backend_stays_stopped_until_manual_previous() -> Result<()> {
    let mut f = Fixture::new(false).await?;
    let mut record = f.prepare().await?;
    // Keep the independent coordinator intact; inject a target host that exits
    // during startup. All hashes are valid, simulating a packaged runtime bug.
    let coordinator = f.root.path().join("coordinator");
    std::fs::create_dir(&coordinator)?;
    let host = astra_platform::platform::executable("astra-host");
    std::fs::copy(f.new.join(&host), coordinator.join(&host))?;
    record.coordinator = coordinator;
    std::fs::copy(
        f.new
            .join(astra_platform::platform::executable("astra-convert")),
        f.new.join(&host),
    )?;
    let bytes = std::fs::read(f.new.join(&host))?;
    let path = f.new.join("manifest.json");
    let mut manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    let entry = manifest["files"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|f| f["path"] == host)
        .unwrap();
    entry["size"] = bytes.len().into();
    entry["sha256"] = format!("{:x}", Sha256::digest(&bytes)).into();
    let manifest = serde_json::to_vec(&manifest)?;
    record.manifest_sha256 = format!("{:x}", Sha256::digest(&manifest));
    std::fs::write(path, manifest)?;
    m::write(&record)?;
    let failed = f.helper(&record, "run").await?;
    assert_eq!(failed.phase, m::Phase::Failed, "{:?}", failed.error);
    assert!(
        f.host.try_wait()?.is_some(),
        "old host must not automatically restart"
    );
    if let Some(process) = &failed.new_process {
        assert!(!astra_platform::lifecycle::matches_live(process)?);
    }
    assert!(failed.restored.is_empty());
    let ordinary = Command::new(f.old.join(&host))
        .arg("--config")
        .arg(f.root.path().join("manager.toml"))
        .output()?;
    assert!(!ordinary.status.success());
    assert!(String::from_utf8_lossy(&ordinary.stderr).contains("Unfinished maintenance"));
    let previous = f.helper(&failed, "previous").await?;
    f.cleanup().await;
    assert_eq!(previous.phase, m::Phase::Complete, "{:?}", previous.error);
    assert_eq!(previous.target, f.old);
    Ok(())
}

#[tokio::test]
#[ignore = "requires built server package"]
async fn partial_restore_failure_cleans_new_business_and_waits_for_explicit_retry() -> Result<()> {
    let mut f = Fixture::new(true).await?;
    let mut blocked_port = 0;
    for name in ["a-running", "z-fail"] {
        let port = std::net::TcpListener::bind("127.0.0.1:0")?
            .local_addr()?
            .port();
        if name == "z-fail" {
            blocked_port = port;
        }
        let config = f.config.config_dir.join(format!("{name}.toml"));
        std::fs::write(&config, format!("allow_private_targets=true\nshutdown_timeout_secs=0\n[[listeners]]\ntag='test'\nprotocol='socks5'\nlisten='127.0.0.1:{port}'\n[[listeners.users]]\nname='test'\npassword='test-pass'\n"))?;
        f.execute(astra_management::v2::Command::Add {
            name: name.into(),
            kind: InstanceKind::Server,
            config,
        })
        .await?;
        f.execute(astra_management::v2::Command::Start {
            instance: name.into(),
        })
        .await?;
    }
    let record = f.prepare().await?;
    let process = record
        .instances
        .iter()
        .find(|i| i.name == "z-fail")
        .unwrap()
        .process
        .clone();
    let home = f.config.state_dir.clone();
    let blocker = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(90), async {
            while astra_platform::lifecycle::matches_live(&process)? {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let listener =
                std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, blocked_port))?;
            loop {
                if m::read(&home)?
                    .is_some_and(|r| matches!(r.phase, m::Phase::Failed | m::Phase::Complete))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            drop(listener);
            Ok::<_, anyhow::Error>(())
        })
        .await?
    });
    let failed = f.helper(&record, "run").await?;
    blocker.await??;
    assert_eq!(failed.phase, m::Phase::Failed, "{:?}", failed.error);
    assert!(f.host.try_wait()?.is_some());
    assert!(
        failed.candidates.contains_key("a-running"),
        "first server must have been restored before injecting the second failure"
    );
    for process in failed.candidates.values() {
        assert!(!astra_platform::lifecycle::matches_live(process)?);
    }
    assert!(!astra_platform::lifecycle::matches_live(
        failed.new_process.as_ref().unwrap()
    )?);
    let retried = f.helper(&failed, "retry").await?;
    f.cleanup().await;
    assert_eq!(retried.phase, m::Phase::Complete, "{:?}", retried.error);
    assert_eq!(retried.restored, vec!["a-running", "z-fail"]);
    Ok(())
}

#[tokio::test]
#[ignore = "requires built client package"]
async fn killed_coordinator_keeps_recovery_pending_and_retry_replaces_its_backend() -> Result<()> {
    let mut f = Fixture::new(false).await?;
    // Model a live interface that saved its session but has not yet switched.
    // Its lease holds the coordinator at Ready so the crash is deterministic.
    let lease = m::register_frontend(
        &f.config.state_dir,
        &f.old
            .join(astra_platform::platform::executable("astra-tui")),
    )?;
    let record = f.prepare().await?;
    m::acknowledge(&record)?;
    let mut helper = f.spawn_helper(&record, "run")?;
    let ready = tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            anyhow::ensure!(
                helper.try_wait()?.is_none(),
                "Coordinator exited before Ready"
            );
            let current = m::read(&f.config.state_dir)?.context("Missing transaction")?;
            if current.phase == m::Phase::Ready {
                return Ok::<_, anyhow::Error>(current);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await??;
    helper.kill()?;
    helper.wait()?;
    let first_backend = ready.new_process.as_ref().unwrap();
    assert!(astra_platform::lifecycle::matches_live(first_backend)?);
    assert!(m::pending(&f.config.state_dir)?);
    assert!(f.host.try_wait()?.is_some());
    assert!(f
        .execute(astra_management::v2::Command::Shutdown)
        .await
        .is_err());
    drop(lease);
    let retried = f.helper(&ready, "retry").await?;
    f.cleanup().await;
    assert_eq!(retried.phase, m::Phase::Complete, "{:?}", retried.error);
    assert!(!astra_platform::lifecycle::matches_live(first_backend)?);
    assert_ne!(retried.new_epoch, ready.new_epoch);
    Ok(())
}
