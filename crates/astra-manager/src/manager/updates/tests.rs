//! Real process regression. The outer test isolates the experimental environment
//! in a child test process; no parallel test sees modified process-global state.
use super::*;
use futures_util::FutureExt;
use sha2::{Digest, Sha256};
use std::{io::Read, net::SocketAddr, panic::AssertUnwindSafe};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

fn binaries() -> PathBuf {
    std::env::var_os("ASTRA_UPDATE_TEST_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_exe()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .to_owned()
        })
}

#[test]
fn real_update_transaction_preserves_live_connections() {
    let bin = binaries();
    for component in ["astra-proxy", "astra-rule", "astra-tui", "astra-host"] {
        let file = bin.join(astra_platform::platform::executable(component));
        assert!(
            file.is_file(),
            "Build current binaries before this test: missing {}",
            file.display()
        );
    }
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "manager::updates::tests::update_transaction_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("ASTRA_UPDATE_TEST_CHILD", "1")
        .env("ASTRA_HOSTED_EXPERIMENTAL", "1")
        .env("ASTRA_UPDATE_TEST_BIN", &bin)
        .stdin(Stdio::null());
    astra_platform::platform::background_command(&mut command, true);
    let mut child = command.spawn().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(180);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(
                status.success(),
                "update transaction subprocess failed: {status}"
            );
            break;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("update transaction subprocess exceeded 180 seconds");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn make_package(root: &Path, bin: &Path) -> Result<PathBuf> {
    let package = root.join("package-b");
    private_dir(&package)?;
    let mut components = std::collections::BTreeMap::new();
    let mut files = Vec::new();
    for component in ["astra-proxy", "astra-rule", "astra-tui", "astra-host"] {
        let name = astra_platform::platform::executable(component);
        let source = bin.join(&name);
        let target = package.join(&name);
        // All fixture inputs are immutable binaries, never edited through links.
        if fs::hard_link(&source, &target).is_err() {
            fs::copy(&source, &target)?;
        }
        let mut reader = fs::File::open(&source)?;
        let mut hash = Sha256::new();
        let mut buffer = vec![0; 1024 * 1024];
        loop {
            let size = reader.read(&mut buffer)?;
            if size == 0 {
                break;
            }
            hash.update(&buffer[..size]);
        }
        components.insert(component.to_owned(), "test-b".to_owned());
        files.push(astra_update::ReleaseFile {
            path: name,
            size: source.metadata()?.len(),
            sha256: format!("{:x}", hash.finalize()),
        });
    }
    let manifest = astra_update::ReleaseManifest {
        schema_version: 1,
        role: Default::default(),
        release_id: "test-b".into(),
        target: astra_update::TARGET.into(),
        host_protocol: astra_update::ProtocolRange { min: 1, max: 1 },
        components,
        files,
    };
    fs::write(
        package.join(astra_update::MANIFEST_FILE),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    Ok(package)
}

async fn statuses(manager: &Manager) -> Result<Vec<Status>> {
    let mut result = vec![];
    for name in ["proxy-one", "proxy-two"] {
        result.push(serde_json::from_value(
            hosted::request(
                &hosted::endpoint(&manager.socket(name)),
                HostCommand::Status,
            )
            .await?,
        )?);
    }
    Ok(result)
}
fn active(status: &Status) -> &hosted::GenerationStatus {
    status
        .generations
        .iter()
        .find(|g| g.generation == status.active)
        .expect("active generation exists")
}
async fn connection(proxy: SocketAddr, destination: SocketAddr) -> Result<TcpStream> {
    timeout(Duration::from_secs(5), async {
        let mut stream = TcpStream::connect(proxy).await?;
        stream.write_all(&[5, 1, 0]).await?;
        let mut greeting = [0; 2];
        stream.read_exact(&mut greeting).await?;
        anyhow::ensure!(greeting == [5, 0], "SOCKS greeting rejected");
        let port = destination.port().to_be_bytes();
        stream
            .write_all(&[5, 1, 0, 1, 127, 0, 0, 1, port[0], port[1]])
            .await?;
        let mut reply = [0; 10];
        stream.read_exact(&mut reply).await?;
        anyhow::ensure!(reply[1] == 0, "SOCKS CONNECT rejected: {}", reply[1]);
        Ok::<_, anyhow::Error>(stream)
    })
    .await
    .context("SOCKS handshake timed out")?
}
async fn echo(stream: &mut TcpStream, text: &[u8]) -> Result<()> {
    timeout(Duration::from_secs(5), async {
        stream.write_all(text).await?;
        let mut reply = vec![0; text.len()];
        stream.read_exact(&mut reply).await?;
        anyhow::ensure!(reply == text, "live stream payload changed");
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("existing connection stopped echoing")?
}
struct Task(tokio::task::JoinHandle<()>);
impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn scenario(manager: &mut Manager, root: &Path, bin: &Path) -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let destination = listener.local_addr()?;
    let _echo_server = Task(tokio::spawn(async move {
        let mut streams = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let Ok((mut stream, _)) = accepted else { break; };
                    streams.spawn(async move { let (mut read,mut write) = stream.split(); let _ = tokio::io::copy(&mut read,&mut write).await; });
                }
                Some(_) = streams.join_next(), if !streams.is_empty() => {}
            }
        }
    }));
    let route = root.join("route.sock");
    let rule_config = root.join("rule.toml");
    fs::write(
        &rule_config,
        format!(
            "socket = {:?}\ndefault_action = \"direct\"\n",
            route.to_string_lossy()
        ),
    )?;
    manager
        .action(Action::Add(Instance {
            name: "shared-rule".into(),
            kind: Kind::Rule,
            config: rule_config,
        }))
        .await?;
    let mut addresses = vec![];
    // Keep reservations until both configurations have distinct ports.
    let reserve_one = std::net::TcpListener::bind("127.0.0.1:0")?;
    let reserve_two = std::net::TcpListener::bind("127.0.0.1:0")?;
    addresses.push(reserve_one.local_addr()?);
    addresses.push(reserve_two.local_addr()?);
    for (name, address) in ["proxy-one", "proxy-two"].into_iter().zip(&addresses) {
        let config = root.join(format!("{name}.toml"));
        fs::write(
            &config,
            format!(
                "rule_socket = {:?}\nsocks5_listen = \"{address}\"\ndecision_cache_ttl_secs = 0\n",
                route.to_string_lossy()
            ),
        )?;
        manager
            .action(Action::Add(Instance {
                name: name.into(),
                kind: Kind::Proxy,
                config,
            }))
            .await?;
    }
    drop((reserve_one, reserve_two));
    for name in ["proxy-one", "proxy-two"] {
        manager.action(Action::Start(name.into())).await?;
    }
    let first = statuses(manager).await?;
    let mut original = vec![];
    for address in &addresses {
        let mut stream = connection(*address, destination).await?;
        echo(&mut stream, b"A before update").await?;
        original.push(stream);
    }
    let package = make_package(root, bin)?;

    // Occupy only the second host's preparation slot: the manager must undo
    // the first candidate after a later instance rejects the transaction.
    let second = hosted::endpoint(&manager.socket("proxy-two"));
    let reserved = hosted::request(
        &second,
        HostCommand::Prepare {
            directory: bin.to_owned(),
            rule_socket: None,
        },
    )
    .await?
    .as_u64()
    .context("missing reserved generation")?;
    let error = manager
        .action(Action::UpdatePackage(package.clone()))
        .await
        .err()
        .context("multi-instance preparation unexpectedly succeeded")?;
    anyhow::ensure!(
        format!("{error:#}").contains("already prepared"),
        "unexpected preparation failure: {error:#}"
    );
    let failed = statuses(manager).await?;
    anyhow::ensure!(
        failed
            .iter()
            .zip(&first)
            .all(|(after, before)| after.active == before.active),
        "failed prepare changed active generation"
    );
    anyhow::ensure!(
        failed[0].generations.iter().all(|g| !g.prepared),
        "first candidate was not aborted"
    );
    anyhow::ensure!(
        manager.updates.rule_generations.is_empty(),
        "aborted transaction retained a candidate rule"
    );
    anyhow::ensure!(
        fs::canonicalize(&manager.bin)? == fs::canonicalize(bin)?,
        "failed prepare changed binary directory"
    );
    for stream in &mut original {
        echo(stream, b"A survives failed prepare").await?;
    }
    for address in &addresses {
        echo(
            &mut connection(*address, destination).await?,
            b"new A after failed prepare",
        )
        .await?;
    }
    hosted::request(
        &second,
        HostCommand::Abort {
            generation: reserved,
        },
    )
    .await?;

    manager.action(Action::UpdatePackage(package)).await?;
    anyhow::ensure!(
        manager.updates.rule_revisions.as_ref() == Some(&manager.source_rule_revisions().await?),
        "first activation did not capture the source Rule revision baseline"
    );
    let upgraded = statuses(manager).await?;
    anyhow::ensure!(
        upgraded
            .iter()
            .zip(&first)
            .all(|(after, before)| after.active != before.active),
        "upgrade did not switch both hosts"
    );
    anyhow::ensure!(
        active(&upgraded[0]).rule_socket == active(&upgraded[1]).rule_socket,
        "shared rule was duplicated per proxy"
    );
    anyhow::ensure!(
        manager.updates.rule_generations.len() == 1,
        "expected one shared candidate rule"
    );
    anyhow::ensure!(
        active(&upgraded[0]).directory == manager.bin,
        "new worker executable is not from release B"
    );
    let mut newer = vec![];
    for address in &addresses {
        let mut stream = connection(*address, destination).await?;
        echo(&mut stream, b"new B connection").await?;
        newer.push(stream);
    }
    for stream in &mut original {
        echo(stream, b"A remains alive while B is active").await?;
    }
    let live = statuses(manager).await?;
    for (status, old) in live.iter().zip(&first) {
        anyhow::ensure!(
            active(status).flows > 0,
            "new connection did not enter B generation"
        );
        anyhow::ensure!(
            status
                .generations
                .iter()
                .any(|g| g.generation == old.active && g.flows > 0),
            "original A generation was not retained"
        );
    }

    manager.action(Action::Rollback).await?;
    let rollback = statuses(manager).await?;
    for (after, before) in rollback.iter().zip(&upgraded) {
        anyhow::ensure!(
            after.active != before.active,
            "rollback did not switch new connections"
        );
        anyhow::ensure!(
            fs::canonicalize(&active(after).directory)? == fs::canonicalize(bin)?,
            "rollback did not use release A executable"
        );
    }
    for stream in &mut original {
        echo(stream, b"old A survives rollback").await?;
    }
    for stream in &mut newer {
        echo(stream, b"B survives rollback").await?;
    }
    let mut rolled_back = vec![];
    for address in &addresses {
        let mut stream = connection(*address, destination).await?;
        echo(&mut stream, b"new A after rollback").await?;
        rolled_back.push(stream);
    }
    for status in statuses(manager).await? {
        anyhow::ensure!(
            active(&status).flows > 0,
            "rollback new stream entered wrong generation"
        );
    }
    manager.action(Action::UpdateStatus).await?;
    anyhow::ensure!(
        manager.updates.rule_generations.len() == 2,
        "draining B still needs its rule generation"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "launched by real_update_transaction_preserves_live_connections with isolated environment"]
async fn update_transaction_child() {
    #[cfg(windows)]
    assert!(
        !astra_platform::elevation::is_elevated().unwrap(),
        "run this integration test as an ordinary user; it never requests UAC"
    );
    assert_eq!(
        std::env::var("ASTRA_UPDATE_TEST_CHILD").as_deref(),
        Ok("1"),
        "run the parent test"
    );
    let bin = binaries();
    // Unix endpoints must fit sun_path even when the checkout or artifact
    // directory is long. Keep all transaction files together under a private
    // short root: package copies, staging and releases remain on one filesystem.
    #[cfg(unix)]
    let temporary = tempfile::Builder::new().prefix("au-").tempdir_in("/tmp");
    #[cfg(not(unix))]
    let temporary = tempfile::Builder::new().prefix("au-").tempdir();
    // Failed assertions retain diagnostics; successful runs remove this root.
    let root = temporary.unwrap().keep();
    let state = root.join("state");
    let config = root.join("config");
    private_dir(&state).unwrap();
    private_dir(&config).unwrap();
    let mut manager = Manager::load_with_config(state, config, bin.clone()).unwrap();
    let result = AssertUnwindSafe(scenario(&mut manager, &root, &bin))
        .catch_unwind()
        .await;
    manager.stop_all().await.unwrap();
    drop(manager);
    match result {
        Ok(Ok(())) => {
            // This is the private directory allocated by this test invocation.
            for _ in 0..30 {
                if fs::remove_dir_all(&root).is_ok() {
                    return;
                }
                sleep(Duration::from_millis(100)).await;
            }
            eprintln!(
                "Test passed; retained locked artifacts at {}",
                root.display()
            );
        }
        Ok(Err(error)) => panic!("{error:#}; retained diagnostics at {}", root.display()),
        Err(error) => {
            eprintln!("Retained diagnostics at {}", root.display());
            std::panic::resume_unwind(error);
        }
    }
}
