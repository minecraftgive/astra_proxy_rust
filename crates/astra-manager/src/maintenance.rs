//! Independent local maintenance coordinator. It outlives both frontends and hosts.
use crate::agent::maintenance::verify_target;
use anyhow::{ensure, Context, Result};
use astra_management::{
    maintenance::{self as m, Phase, Record},
    v2::*,
};
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::io::AsyncWriteExt;

async fn request(record: &Record, body: RequestBody) -> Result<ResponseBody> {
    let reply = astra_management::ManagerClient::new(record.config.endpoint())
        .request_v2(Request::new(uuid::Uuid::new_v4().to_string(), body))
        .await?;
    reply
        .result
        .map_err(|error| anyhow::anyhow!("{}", error.message))
}
async fn local(record: &Record, command: LocalCommand) -> Result<ResponseBody> {
    request(record, RequestBody::Local(command)).await
}
fn phase(record: &mut Record, phase: Phase) -> Result<()> {
    eprintln!("{}: {}", record.operation_id, phase.label());
    record.phase = phase;
    m::write(record)
}
pub(crate) async fn wait_gone(
    process: &astra_platform::lifecycle::ProcessIdentity,
    seconds: u64,
) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(seconds), async {
        while astra_platform::lifecycle::matches_live(process)? {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("Process did not exit; maintenance stopped without launching another backend")?
}
async fn preflight(record: &Record) -> Result<()> {
    let manifest = verify_target(&record.target, &record.manifest_sha256)?;
    ensure!(
        manifest.release_id == record.release_id,
        "Maintenance release changed"
    );
    target_config(record)?;
    ensure!(
        !record.config.web.enabled || manifest.components.contains_key("astra-web"),
        "Managed web gateway must be part of the verified target package"
    );
    for frontend in &record.frontends {
        let name = frontend
            .executable
            .file_name()
            .context("Unknown maintenance frontend")?;
        ensure!(
            manifest
                .files
                .iter()
                .any(|f| std::path::Path::new(&f.path) == std::path::Path::new(name))
                && record.target.join(name).is_file(),
            "New package lacks running interface {}",
            name.to_string_lossy()
        );
    }
    for instance in &record.instances {
        let text = crate::agent::bounded_text(&instance.config)?;
        ensure!(
            crate::store::revision(&text) == instance.config_revision,
            "Configuration changed: {}",
            instance.name
        );
        let binary = match instance.kind {
            InstanceKind::Proxy => "astra-proxy",
            InstanceKind::Rule => "astra-rule",
            InstanceKind::Server => "astra-server-core",
        };
        ensure!(
            manifest.components.contains_key(binary),
            "New package lacks {binary}"
        );
        let result = tokio::time::timeout(Duration::from_secs(90), async {
            let mut command = astra_platform::platform::background_core(
                record
                    .target
                    .join(astra_platform::platform::executable(binary)),
            );
            command
                .arg("--check-stdin")
                .arg(&instance.config)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            if let Some(runtime) = &record.config.runtime_dir {
                command.env("ASTRA_RUNTIME_DIR", runtime);
            }
            let mut child = command.spawn()?;
            child
                .stdin
                .take()
                .context("Missing validation input")?
                .write_all(text.as_bytes())
                .await?;
            child.wait_with_output().await.map_err(anyhow::Error::from)
        })
        .await
        .context("Maintenance configuration check timed out")??;
        ensure!(
            result.status.success(),
            "New version rejected {}: {}",
            instance.name,
            String::from_utf8_lossy(&result.stderr)
        );
    }
    Ok(())
}

fn target_config(record: &Record) -> Result<astra_management::config::ManagerConfig> {
    let mut config = record.config.clone();
    config.bin_dir = record.target.clone();
    if config.web.enabled {
        let binary = record
            .target
            .join(astra_platform::platform::executable("astra-web"));
        ensure!(binary.is_file(), "New package is missing the managed web gateway; use a complete package containing astra-web");
        config.web.binary = Some(binary);
    }
    config.validate()?;
    Ok(config)
}

async fn cleanup(record: &mut Record, child: &mut Option<std::process::Child>) -> Result<()> {
    let mut errors = Vec::new();
    // Never trust a stale PID. Abort is accepted only by the recorded new host.
    if let Some(process) = &record.new_process {
        if astra_platform::lifecycle::matches_live(process)? {
            let abort = tokio::time::timeout(
                Duration::from_secs(75),
                local(
                    record,
                    LocalCommand::AbortMaintenance {
                        operation_id: record.operation_id.clone(),
                    },
                ),
            )
            .await;
            if !matches!(abort, Ok(Ok(_))) {
                errors.push(format!(
                    "backend PID {} did not acknowledge cleanup",
                    process.pid
                ));
            }
            if let Err(error) = wait_gone(process, 10).await {
                errors.push(format!("backend PID {}: {error:#}", process.pid));
            }
        }
    }
    // The host journals cores as they become ready, even if a restore request
    // fails halfway through. Only a matching, peer-bound core may be stopped.
    if let Some(latest) = m::read(&record.config.state_dir)? {
        ensure!(
            latest.operation_id == record.operation_id,
            "Maintenance operation changed during cleanup"
        );
        record.candidates = latest.candidates;
    }
    for (name, process) in &record.candidates {
        if astra_platform::lifecycle::matches_live(process).is_ok_and(|live| !live) {
            continue;
        }
        let instance = record
            .instances
            .iter()
            .find(|i| i.name == *name)
            .context("Unknown maintenance candidate")?;
        let endpoint = instance.endpoint(&record.config.state_dir);
        let stopped = tokio::time::timeout(Duration::from_secs(15), async {
            let identity = if instance.kind == InstanceKind::Server {
                astra_management::server::call(&endpoint, astra_management::server::Action::Status)
                    .await?
                    .identity
                    .context("Missing server cleanup identity")?
            } else {
                astra_ipc::control::identity(&endpoint).await?
            };
            ensure!(
                identity.process == *process,
                "Process identity changed; will not stop it"
            );
            if instance.kind == InstanceKind::Server {
                astra_management::server::call(
                    &endpoint,
                    astra_management::server::Action::StopIdentified {
                        boot_id: identity.boot_id,
                        force: true,
                    },
                )
                .await?;
            } else {
                astra_ipc::control::request(
                    &endpoint,
                    astra_model::control::Command::StopIdentified {
                        boot_id: identity.boot_id,
                        force: true,
                    },
                )
                .await?;
            }
            wait_gone(process, 10).await
        })
        .await;
        if !matches!(stopped, Ok(Ok(()))) {
            errors.push(format!(
                "remaining core {name} PID {}; inspect its log",
                process.pid
            ));
        }
    }
    if let Some(mut spawned) = child.take() {
        if spawned.try_wait()?.is_none() && record.config.web.enabled {
            errors.push(format!(
                "backend PID {} still supervises a gateway; inspect web/process.json",
                spawned.id()
            ));
            std::thread::spawn(move || {
                let _ = spawned.wait();
            });
        } else {
            if spawned.try_wait()?.is_none() {
                // Retained OS handle identifies our own child; killing an arbitrary
                // PID from a recovery record is intentionally unsupported.
                spawned.kill()?;
            }
            spawned.wait()?;
        }
    }
    ensure!(errors.is_empty(), "{}", errors.join("; "));
    Ok(())
}

/// CLI accepts a state directory and ID, never commands, credentials or arbitrary
/// process IDs. Everything else comes from the private manager-prepared record.
pub async fn run(home: PathBuf, id: String, recovery: &str) -> Result<()> {
    let home = std::fs::canonicalize(home)?;
    astra_platform::platform::validate_state_directory(&home)?;
    let _lock = m::lock(&home)?;
    let mut record = m::read(&home)?.context("No maintenance operation")?;
    ensure!(record.operation_id == id, "Maintenance operation changed");
    if let Some(helper) = &record.helper {
        ensure!(
            !astra_platform::lifecycle::matches_live(helper)?,
            "The previous maintenance helper is still running"
        );
    }
    ensure!(
        matches!(recovery, "run" | "retry" | "previous"),
        "Invalid maintenance recovery action"
    );
    ensure!(
        if recovery == "run" {
            record.phase == Phase::Prepared
        } else {
            !record.phase.terminal()
        },
        "Maintenance is not eligible for this action"
    );
    if recovery == "retry" {
        let target = verify_target(&record.coordinator, &record.coordinator_sha256)?;
        record.target = record.coordinator.clone();
        record.manifest_sha256 = record.coordinator_sha256.clone();
        record.release_id = target.release_id;
    }
    if recovery == "previous" {
        let digest = record.previous_manifest_sha256.clone().context(
            "Previous version has no verified package; choose a complete package for recovery",
        )?;
        let old = verify_target(&record.previous, &digest)?;
        record.target = record.previous.clone();
        record.manifest_sha256 = digest;
        record.release_id = old.release_id;
    }
    record.helper = Some(astra_platform::lifecycle::capture(std::process::id())?);
    if recovery != "run" {
        let _ = std::fs::remove_file(m::directory(&home).join("cancel.json"));
    }
    record.error = None;
    record.failed_phase = None;
    m::write(&record)?;
    let mut child = None;
    let result = async {
        if recovery != "run" {
            cleanup(&mut record, &mut child).await?;
            record.frontends = m::frontends(&record.config.state_dir)?;
            record.checkpoint_id = uuid::Uuid::new_v4().to_string();
            phase(&mut record, Phase::Prepared)?;
        }
        execute(&mut record, &mut child).await
    }
    .await;
    if let Err(error) = &result {
        if record.phase == Phase::Cancelled {
            return Ok(());
        }
        let mut message = format!("{error:#}");
        if let Err(error) = cleanup(&mut record, &mut child).await {
            message.push_str(&format!("; cleanup requires attention: {error:#}"));
        }
        record.error = Some(message);
        record.failed_phase = Some(record.phase);
        phase(&mut record, Phase::Failed)?;
    }
    if let Some(mut child) = child {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
    result
}
async fn execute(record: &mut Record, spawned: &mut Option<std::process::Child>) -> Result<()> {
    if m::cancelled(record)? {
        phase(record, Phase::Cancelled)?;
        anyhow::bail!("Maintenance cancelled before shutdown");
    }
    preflight(record).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if m::cancelled(record)? {
            phase(record, Phase::Cancelled)?;
            anyhow::bail!("Maintenance cancelled before shutdown");
        }
        if record
            .frontends
            .iter()
            .map(|f| m::checkpointed(record, &f.process))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .all(|ready| ready)
        {
            break;
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "A local interface has unsaved work; close or save it before retrying maintenance"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // An authorized privileged helper is prepared before any shutdown. Its
    // platform-specific implementation never elevates the UI or manager.
    let mut privilege = crate::maintenance_privilege::prepare(record).await?;
    if m::cancelled(record)? {
        phase(record, Phase::Cancelled)?;
        anyhow::bail!("Maintenance cancelled before shutdown");
    }
    if astra_platform::lifecycle::matches_live(&record.old_process)? {
        let ResponseBody::Hello(hello) = request(record, RequestBody::Query(Query::Hello)).await?
        else {
            anyhow::bail!("Invalid manager identity response")
        };
        ensure!(
            hello.epoch == record.old_epoch,
            "Original manager identity changed"
        );
        phase(record, Phase::Stopping)?;
        local(
            record,
            LocalCommand::CommitMaintenance {
                operation_id: record.operation_id.clone(),
            },
        )
        .await?;
        wait_gone(&record.old_process, 90).await?;
    }
    privilege.wait_stopped(record).await?;
    for process in &record.managed_processes {
        wait_gone(process, 30).await?;
    }
    if let Some(process) = &record.new_process {
        wait_gone(process, 1).await?;
    }
    phase(record, Phase::UpdatingService)?;
    privilege.update_service(record).await?;
    verify_target(&record.target, &record.manifest_sha256)?;
    let config = target_config(record)?;
    let config_file = m::directory(&config.state_dir).join("manager.toml");
    astra_platform::storage::atomic_write_default(
        &config_file,
        toml::to_string(&config)?.as_bytes(),
    )?;
    let endpoint = config.endpoint();
    if endpoint.exists() {
        astra_platform::platform::recover_stale_manager_endpoint(&endpoint)?;
    }
    ensure!(!endpoint.exists(), "Manager endpoint remains occupied");
    record.candidates.clear();
    record.new_process = None;
    record.new_epoch = None;
    phase(record, Phase::Starting)?;
    let runtime = config
        .runtime_dir
        .clone()
        .unwrap_or(astra_platform::storage::runtime_root()?);
    for instance in &record.instances {
        if let Some(saved) = &instance.runtime {
            let mut overrides = saved.clone();
            #[cfg(unix)]
            {
                overrides.tun = Some(false);
            }
            overrides.system_proxy = Some(false);
            let path =
                astra_platform::storage::runtime_path_in(&runtime, &instance.config, "overrides")?;
            astra_platform::storage::atomic_write_default(&path, &serde_json::to_vec(&overrides)?)?;
        }
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(config.state_dir.join("manager.log"))?;
    let mut command = std::process::Command::new(
        record
            .target
            .join(astra_platform::platform::executable("astra-host")),
    );
    command
        .env("ASTRA_MAINTENANCE_OPERATION", &record.operation_id)
        .arg("--config")
        .arg(&config_file)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    astra_platform::platform::background_command(&mut command, false);
    *spawned = Some(command.spawn().context("Start new maintenance backend")?);
    let child = spawned.as_mut().expect("spawned backend");
    record.new_process = Some(astra_platform::lifecycle::capture(child.id())?);
    record.new_epoch = None;
    record.restored.clear();
    m::write(record)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        ensure!(
            child.try_wait()?.is_none(),
            "New backend exited before readiness"
        );
        if let Ok(Ok(ResponseBody::Hello(hello))) = tokio::time::timeout(
            Duration::from_secs(1),
            request(record, RequestBody::Query(Query::Hello)),
        )
        .await
        {
            if hello.epoch != record.old_epoch
                && hello.capabilities.iter().any(|c| c == m::CAPABILITY)
            {
                record.new_epoch = Some(hello.epoch);
                break;
            }
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "New backend readiness timed out"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    phase(record, Phase::Restoring)?;
    #[cfg(windows)]
    local(
        record,
        LocalCommand::RestoreMaintenance {
            operation_id: record.operation_id.clone(),
            dependencies_only: true,
        },
    )
    .await?;
    privilege.before_restore(record).await?;
    record.candidates = m::read(&record.config.state_dir)?
        .context("Missing maintenance record")?
        .candidates;
    local(
        record,
        LocalCommand::RestoreMaintenance {
            operation_id: record.operation_id.clone(),
            dependencies_only: false,
        },
    )
    .await?;
    privilege.restore_tun(record).await?;
    record.candidates = m::read(&record.config.state_dir)?
        .context("Missing maintenance record")?
        .candidates;
    if config.web.enabled {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let mut address = config.web.listen;
            if address.ip().is_unspecified() {
                address.set_ip(if address.is_ipv4() {
                    std::net::Ipv4Addr::LOCALHOST.into()
                } else {
                    std::net::Ipv6Addr::LOCALHOST.into()
                });
            }
            if crate::agent::maintenance::managed_processes(&config).is_ok()
                && matches!(
                    tokio::time::timeout(
                        Duration::from_secs(1),
                        tokio::net::TcpStream::connect(address)
                    )
                    .await,
                    Ok(Ok(_))
                )
            {
                break;
            }
            ensure!(
                tokio::time::Instant::now() < deadline,
                "Updated managed web gateway did not become ready"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    record.restored = record.instances.iter().map(|i| i.name.clone()).collect();
    phase(record, Phase::Ready)?;
    local(
        record,
        LocalCommand::FinishMaintenance {
            operation_id: record.operation_id.clone(),
        },
    )
    .await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        if record
            .frontends
            .iter()
            .map(|f| m::switched(record, f))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .all(|ready| ready)
        {
            break;
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "Updated interfaces did not become ready; inspect their logs before retrying"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Bootstrap consults this pinned target before older update-store selections.
    astra_platform::storage::atomic_write_default(
        &m::directory(&config.state_dir).join("launch.json"),
        &serde_json::to_vec(
            &serde_json::json!({"directory":record.target,"manifest_sha256":record.manifest_sha256}),
        )?,
    )?;
    phase(record, Phase::Complete)?;
    Ok(())
}
