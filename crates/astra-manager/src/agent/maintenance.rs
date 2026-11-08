use super::*;
use astra_management::maintenance::{self as m, Phase, Record};
use sha2::{Digest, Sha256};

pub(crate) fn manifest_hash(directory: &Path) -> Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(astra_platform::storage::read_bounded(
            &directory.join(astra_update::MANIFEST_FILE),
            astra_update::MANIFEST_LIMIT
        )?)
    ))
}
pub(crate) fn verify_target(
    directory: &Path,
    digest: &str,
) -> Result<astra_update::ReleaseManifest> {
    let manifest = astra_update::verify_installation_directory(directory, &Default::default())?;
    anyhow::ensure!(
        matches!(
            manifest.role,
            astra_update::ReleaseRole::Client | astra_update::ReleaseRole::Server
        ),
        "Maintenance requires a complete client or server package"
    );
    anyhow::ensure!(
        manifest_hash(directory)? == digest,
        "The maintenance package changed"
    );
    #[cfg(windows)]
    if directory.join("wintun.dll").is_file() {
        anyhow::ensure!(
            manifest.files.iter().any(|f| f.path == "wintun.dll"),
            "Target TUN driver is not part of the verified package"
        );
    }
    Ok(manifest)
}
impl AgentService {
    pub(super) fn maintenance_gate(&self) -> Result<()> {
        anyhow::ensure!(
            !m::pending(&self.config.state_dir)?,
            "Conflict: maintenance is pending; use its local recovery controls"
        );
        Ok(())
    }
    pub(super) async fn maintenance(&self, command: LocalCommand) -> Result<ResponseBody> {
        let _serial = self.mutation.lock().await;
        match command {
            LocalCommand::PrepareMaintenance {
                target_directory,
                expected_manifest_sha256,
                expected_epoch,
            } => {
                let _lock = m::lock(&self.config.state_dir)?;
                self.maintenance_gate()?;
                anyhow::ensure!(
                    expected_epoch == self.identity.boot_id,
                    "Conflict: manager changed before maintenance"
                );
                let target = std::fs::canonicalize(target_directory)?;
                let manifest = verify_target(&target, &expected_manifest_sha256)?;
                let status = self.call(old::Action::UpdateStatus).await?;
                let status: old::updates::UpdateStatus = serde_json::from_str(&status.text)?;
                anyhow::ensure!(
                    !status.capabilities.operation_running
                        && status.capabilities.recovery_operation_id.is_none(),
                    "Finish or recover the online update before maintenance"
                );
                let mut instances = Vec::new();
                for instance in self.instances().await? {
                    anyhow::ensure!(
                        instance.state != "Unreachable" && instance.state != "Unresponsive",
                        "Instance {} is unreachable; resolve its process state before maintenance",
                        instance.name
                    );
                    let Some(pid) = instance.pid else {
                        continue;
                    };
                    anyhow::ensure!(
                        !instance.changed,
                        "Save or reload the changed configuration for {} before maintenance",
                        instance.name
                    );
                    let runtime = if instance.kind == InstanceKind::Proxy {
                        let reply = self
                            .call(old::Action::Runtime {
                                name: instance.name.clone(),
                                command: astra_model::runtime::Command::Snapshot,
                                expected_pid: Some(pid),
                            })
                            .await?;
                        let snapshot: astra_model::runtime::Snapshot =
                            serde_json::from_str(&reply.text)?;
                        Some(astra_model::runtime::Overrides {
                            mode: snapshot.mode,
                            selected: snapshot
                                .groups
                                .into_iter()
                                .filter_map(|g| g.selected.map(|s| (g.name, s)))
                                .collect(),
                            system_proxy: Some(snapshot.system_proxy),
                            tun: Some(snapshot.tun),
                            allow_lan: Some(snapshot.allow_lan),
                        })
                    } else {
                        None
                    };
                    let endpoint = if instance.kind == InstanceKind::Server {
                        self.server_endpoint(&instance.name)
                    } else {
                        self.config
                            .state_dir
                            .join(format!("{}.sock", instance.name))
                    };
                    let identity = if instance.kind == InstanceKind::Server {
                        self.verify_server(&instance.name).await?
                    } else {
                        astra_ipc::control::identity(&endpoint).await?
                    };
                    let text = bounded_text(&instance.config)?;
                    anyhow::ensure!(identity.applied.as_ref().is_some_and(|a| a.digest == astra_ipc::control::config_digest(&text)), "Running configuration differs from disk for {}; reload it before maintenance", instance.name);
                    let process = identity.process;
                    anyhow::ensure!(process.pid == pid, "Instance changed during snapshot");
                    instances.push(m::Instance {
                        name: instance.name,
                        kind: instance.kind,
                        config_revision: store::revision(&text),
                        config: instance.config,
                        process,
                        runtime,
                    });
                }
                let helper_sha256 = manifest
                    .files
                    .iter()
                    .find(|file| file.path == astra_platform::platform::executable("astra-host"))
                    .context("Complete package lacks maintenance helper")?
                    .sha256
                    .clone();
                let record = Record {
                    version: 1,
                    operation_id: uuid::Uuid::new_v4().to_string(),
                    checkpoint_id: uuid::Uuid::new_v4().to_string(),
                    phase: Phase::Prepared,
                    release_id: manifest.release_id,
                    coordinator: target.clone(),
                    coordinator_sha256: expected_manifest_sha256.clone(),
                    helper_sha256,
                    target,
                    manifest_sha256: expected_manifest_sha256,
                    previous: self.config.bin_dir.clone(),
                    previous_manifest_sha256: manifest_hash(&self.config.bin_dir).ok(),
                    old_epoch: self.identity.boot_id.clone(),
                    old_process: astra_platform::lifecycle::capture(std::process::id())?,
                    config: self.config.clone(),
                    instances,
                    frontends: m::frontends(&self.config.state_dir)?,
                    managed_processes: managed_processes(&self.config)?,
                    helper: None,
                    new_process: None,
                    new_epoch: None,
                    restored: Vec::new(),
                    error: None,
                    failed_phase: None,
                    service_update: false,
                    candidates: Default::default(),
                };
                m::write(&record)?;
                Ok(ResponseBody::MaintenanceStatus(Some(Box::new(record))))
            }
            LocalCommand::CancelMaintenance { operation_id } => {
                let mut record = self.maintenance_record(&operation_id)?;
                anyhow::ensure!(
                    record.cancellable(),
                    "Maintenance shutdown has begun; cancellation is no longer available"
                );
                m::request_cancel(&self.config.state_dir, &operation_id)?;
                if let Ok(_lock) = m::lock(&self.config.state_dir) {
                    record.phase = Phase::Cancelled;
                    m::write(&record)?;
                }
                Ok(ResponseBody::MaintenanceStatus(Some(Box::new(record))))
            }
            LocalCommand::CommitMaintenance { operation_id } => {
                let record = self.maintenance_record(&operation_id)?;
                anyhow::ensure!(
                    record.old_epoch == self.identity.boot_id && record.phase == Phase::Stopping,
                    "Invalid maintenance commit"
                );
                anyhow::ensure!(
                    !m::cancelled(&record)?,
                    "Maintenance cancelled before shutdown"
                );
                verify_target(&record.target, &record.manifest_sha256)?;
                self.check_maintenance_snapshot(&record)?;
                anyhow::ensure!(
                    managed_processes(&self.config)? == record.managed_processes,
                    "Managed components changed during maintenance preparation"
                );
                let actual: std::collections::BTreeSet<_> = self
                    .instances()
                    .await?
                    .into_iter()
                    .filter_map(|i| i.pid.map(|pid| (i.name, pid)))
                    .collect();
                let expected = record
                    .instances
                    .iter()
                    .map(|i| (i.name.clone(), i.process.pid))
                    .collect();
                anyhow::ensure!(
                    actual == expected,
                    "Running instances changed after maintenance preparation"
                );
                for instance in &record.instances {
                    let identity = if instance.kind == InstanceKind::Server {
                        self.verify_server(&instance.name).await?
                    } else {
                        astra_ipc::control::identity(&instance.endpoint(&record.config.state_dir))
                            .await?
                    };
                    let text = bounded_text(&instance.config)?;
                    anyhow::ensure!(
                        identity.process == instance.process
                            && identity.applied.as_ref().is_some_and(
                                |a| a.digest == astra_ipc::control::config_digest(&text)
                            ),
                        "Instance {} changed before shutdown",
                        instance.name
                    );
                }
                // Acknowledgement is not proof of exit: the helper also waits for
                // the original OS process identity and all captured cores to die.
                self.command(Command::Shutdown, &operation_id).await
            }
            LocalCommand::MaintenanceTun {
                operation_id,
                instance,
                ticket,
            } => {
                let record = self.maintenance_record(&operation_id)?;
                anyhow::ensure!(
                    record.phase == Phase::Restoring
                        && record.new_epoch.as_deref() == Some(&self.identity.boot_id)
                        && record.instances.iter().any(|i| i.name == instance
                            && i.runtime.as_ref().is_some_and(|r| r.tun == Some(true))),
                    "Invalid maintenance TUN restore"
                );
                let action = match ticket {
                    Some(ticket) => old::Action::WaitElevation {
                        name: instance,
                        ticket,
                    },
                    None => old::Action::PrepareElevation {
                        name: instance,
                        expected_pid: None,
                    },
                };
                let reply = self.call(action).await?;
                self.record_maintenance_processes(&operation_id).await?;
                Ok(ResponseBody::Local(serde_json::from_str(&reply.text)?))
            }
            LocalCommand::AbortMaintenance { operation_id } => {
                let record = self.maintenance_record(&operation_id)?;
                anyhow::ensure!(
                    record.new_process.as_ref()
                        == Some(&astra_platform::lifecycle::capture(std::process::id())?),
                    "Cleanup is restricted to this operation's new backend"
                );
                self.command(Command::Shutdown, &operation_id).await
            }
            LocalCommand::RestoreMaintenance {
                operation_id,
                dependencies_only,
            } => {
                let record = self.maintenance_record(&operation_id)?;
                anyhow::ensure!(
                    record.phase == Phase::Restoring
                        && self.config.bin_dir == record.target
                        && record.new_epoch.as_deref() == Some(&self.identity.boot_id),
                    "Invalid maintenance restore target"
                );
                anyhow::ensure!(
                    record.new_process.as_ref()
                        == Some(&astra_platform::lifecycle::capture(std::process::id())?),
                    "New backend process identity mismatch"
                );
                self.check_maintenance_snapshot(&record)?;
                let native = record
                    .instances
                    .iter()
                    .filter(|i| {
                        i.kind != InstanceKind::Server
                            && (!dependencies_only || i.kind == InstanceKind::Rule)
                    })
                    .map(|i| i.name.clone())
                    .collect();
                let result = self.call(old::Action::MaintenanceRestore(native)).await;
                self.record_maintenance_processes(&operation_id).await?;
                result?;
                if dependencies_only {
                    return Ok(ResponseBody::Done {
                        message: "Maintenance dependencies restored".into(),
                    });
                }
                for instance in &record.instances {
                    if instance.kind == InstanceKind::Server {
                        let result = self.start_server(&instance.name, &instance.config).await;
                        self.record_maintenance_processes(&operation_id).await?;
                        result?;
                    }
                    if let Some(runtime) = &instance.runtime {
                        use astra_model::runtime::Command as R;
                        let mut commands = vec![
                            R::SetMode(runtime.mode),
                            R::AllowLan(runtime.allow_lan.unwrap_or(false)),
                        ];
                        commands.extend(runtime.selected.iter().map(|(group, member)| R::Select {
                            group: group.clone(),
                            member: member.clone(),
                        }));
                        commands.push(R::SystemProxy(runtime.system_proxy.unwrap_or(false)));
                        // Unix TUN is restored by the already-authorized helper's
                        // narrow elevation handoff after ordinary cores are ready.
                        #[cfg(windows)]
                        commands.push(R::Tun(runtime.tun.unwrap_or(false)));
                        for command in commands {
                            self.call(old::Action::Runtime {
                                name: instance.name.clone(),
                                command,
                                expected_pid: None,
                            })
                            .await?;
                        }
                    }
                }
                Ok(ResponseBody::Done {
                    message: "Exact running instance set restored".into(),
                })
            }
            LocalCommand::FinishMaintenance { operation_id } => {
                let record = self.maintenance_record(&operation_id)?;
                anyhow::ensure!(
                    record.phase == Phase::Ready
                        && record.new_epoch.as_deref() == Some(&self.identity.boot_id),
                    "Invalid maintenance completion"
                );
                let actual: std::collections::BTreeSet<_> = self
                    .instances()
                    .await?
                    .into_iter()
                    .filter(|i| i.pid.is_some())
                    .map(|i| i.name)
                    .collect();
                let expected = record.instances.iter().map(|i| i.name.clone()).collect();
                anyhow::ensure!(
                    actual == expected,
                    "Restored instance set differs from maintenance snapshot"
                );
                Ok(ResponseBody::Done {
                    message: "Maintenance verified".into(),
                })
            }
            _ => anyhow::bail!("Unsupported maintenance command"),
        }
    }
    fn maintenance_record(&self, id: &str) -> Result<Record> {
        let record = m::read(&self.config.state_dir)?.context("No maintenance operation")?;
        anyhow::ensure!(
            record.operation_id == id,
            "Conflict: maintenance operation changed"
        );
        Ok(record)
    }
    fn check_maintenance_snapshot(&self, record: &Record) -> Result<()> {
        for instance in &record.instances {
            anyhow::ensure!(
                store::revision(&bounded_text(&instance.config)?) == instance.config_revision,
                "Configuration changed during maintenance: {}",
                instance.name
            );
        }
        Ok(())
    }
    async fn record_maintenance_processes(&self, id: &str) -> Result<()> {
        let mut record = self.maintenance_record(id)?;
        for instance in self.instances().await? {
            if let Some(pid) = instance.pid {
                // The control handshake can observe an elevated core even when
                // macOS does not allow the caller to inspect its process table.
                let endpoint = if instance.kind == InstanceKind::Server {
                    self.server_endpoint(&instance.name)
                } else {
                    self.config
                        .state_dir
                        .join(format!("{}.sock", instance.name))
                };
                let identity = if instance.kind == InstanceKind::Server {
                    self.verify_server(&instance.name).await?
                } else {
                    astra_ipc::control::identity(&endpoint).await?
                };
                anyhow::ensure!(identity.process.pid == pid, "Maintenance process changed");
                record.candidates.insert(instance.name, identity.process);
            }
        }
        m::write(&record)
    }
}

pub(crate) fn managed_processes(
    config: &astra_management::config::ManagerConfig,
) -> Result<Vec<astra_platform::lifecycle::ProcessIdentity>> {
    if !config.web.enabled {
        return Ok(Vec::new());
    }
    let state = config
        .web
        .state_dir
        .clone()
        .unwrap_or_else(|| config.state_dir.join("web"));
    let bytes = astra_platform::storage::read_bounded(&state.join("process.json"), 4096)
        .context("The managed web gateway has no process identity; wait for readiness or maintain it manually")?;
    let identity = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        astra_platform::lifecycle::matches_live(&identity)?,
        "Managed web gateway is not running"
    );
    Ok(vec![identity])
}
