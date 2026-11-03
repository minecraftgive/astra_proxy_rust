//! Update transactions are serialized by the existing manager actor.
use super::*;
use astra_management::host_control as hosted;
use astra_management::host_control::Command as HostCommand;
use astra_management::host_control::Status;
use astra_update::{ImportOptions, ReleaseStore};
use serde_json::json;

#[cfg(test)]
mod tests;
mod transactions;

#[derive(Default)]
pub(super) struct Updates {
    previous_bin: Option<PathBuf>,
    rule_generations: Vec<RuleGeneration>,
    rule_revisions: Option<HashMap<String, u64>>,
    pub(super) status: crate::protocol::updates::UpdateStatus,
    tracking: bool,
}
struct RuleGeneration {
    child: Child,
    socket: String,
    control: PathBuf,
    log: PathBuf,
}
struct Candidate {
    endpoint: PathBuf,
    generation: u64,
    previous: u64,
    host_boot_id: String,
}

async fn restore_previous(candidate: &Candidate) -> Result<()> {
    let status: Status =
        serde_json::from_value(hosted::request(&candidate.endpoint, HostCommand::Status).await?)?;
    anyhow::ensure!(
        status.boot_id == candidate.host_boot_id,
        "Network host restarted during update rollback"
    );
    anyhow::ensure!(
        status.active == candidate.previous || status.active == candidate.generation,
        "Active generation changed during update rollback"
    );
    if status.active == candidate.generation {
        if !status.paused {
            hosted::request(&candidate.endpoint, HostCommand::Pause).await?;
        }
        hosted::request(
            &candidate.endpoint,
            HostCommand::SelectExisting {
                generation: candidate.previous,
            },
        )
        .await?;
    }
    // An activated generation is no longer prepared. Keep its live sessions and
    // let the host reap it naturally after the last flow, rather than Abort it.
    if status
        .generations
        .iter()
        .any(|generation| generation.generation == candidate.generation && generation.prepared)
    {
        hosted::request(
            &candidate.endpoint,
            HostCommand::Abort {
                generation: candidate.generation,
            },
        )
        .await?;
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct SourceConfig {
    path: PathBuf,
    text: String,
    pid: u32,
}
#[derive(Debug, PartialEq, Eq)]
struct SourceSnapshot {
    configs: HashMap<String, SourceConfig>,
    revisions: HashMap<String, u64>,
}

impl Manager {
    async fn source_rule_revisions(&self) -> Result<HashMap<String, u64>> {
        let queries = self
            .registry
            .instances
            .iter()
            .filter_map(|instance| {
                let run = self.running.get(&instance.name)?;
                (instance.kind == Kind::Rule).then(|| {
                    (
                        instance.name.clone(),
                        self.socket(&instance.name),
                        run.child.id(),
                    )
                })
            })
            .map(|(name, socket, pid)| async move {
                let request = astra_model::RouteRequest {
                    destination_ip: None,
                    api_version: astra_ipc::API_VERSION,
                    id: 1,
                    target: astra_model::Target {
                        host: "astra-host-health.invalid".into(),
                        port: 443,
                    },
                    network: astra_model::Network::Tcp,
                    inbound: astra_model::Inbound::Socks5,
                    process: None,
                };
                let response = timeout(
                    Duration::from_millis(750),
                    control::explain_route(&socket, request),
                )
                .await
                .with_context(|| format!("Read source Rule Core revision: {name}"))??;
                anyhow::ensure!(
                    Some(response.pid) == pid && response.api_version == astra_ipc::API_VERSION,
                    "Source Rule Core identity changed: {name}"
                );
                Ok::<_, anyhow::Error>((name, response.revision))
            });
        futures_util::future::join_all(queries)
            .await
            .into_iter()
            .collect()
    }
    async fn source_snapshot(&self) -> Result<SourceSnapshot> {
        let revisions = self.source_rule_revisions().await?;
        let reads = self
            .registry
            .instances
            .iter()
            .filter_map(|instance| {
                self.running.get(&instance.name).map(|run| {
                    (
                        instance.clone(),
                        run.child.id(),
                        self.socket(&instance.name),
                    )
                })
            })
            .map(|(instance, pid, socket)| async move {
                let snapshot =
                    timeout(Duration::from_secs(2), control::config_snapshot(&socket)).await??;
                let path = fs::canonicalize(&instance.config)?;
                anyhow::ensure!(
                    Some(snapshot.pid) == pid && fs::canonicalize(&snapshot.path)? == path,
                    "Source instance identity/path changed: {}",
                    instance.name
                );
                anyhow::ensure!(
                    snapshot.text == read_config(&path)?,
                    "Configuration has unapplied changes; reload before updating: {}",
                    instance.name
                );
                Ok::<_, anyhow::Error>((
                    instance.name,
                    SourceConfig {
                        path,
                        text: snapshot.text,
                        pid: snapshot.pid,
                    },
                ))
            });
        let configs = futures_util::future::join_all(reads)
            .await
            .into_iter()
            .collect::<Result<_>>()?;
        anyhow::ensure!(
            self.source_rule_revisions().await? == revisions,
            "Source Rule Core changed while capturing update snapshot; retry"
        );
        Ok(SourceSnapshot { configs, revisions })
    }
    async fn verify_source_snapshot(&self, expected: &SourceSnapshot) -> Result<()> {
        anyhow::ensure!(&self.source_snapshot().await?==expected,"Configuration or source Rule revision changed during update; candidates were not activated");
        Ok(())
    }
    pub(super) async fn refresh_rule_generations(&mut self) -> Result<()> {
        if self
            .updates
            .status
            .operation
            .as_ref()
            .is_some_and(|operation| !operation.phase.terminal())
        {
            return Ok(()); // Preserve the exact generations needed by update recovery.
        }
        if self.updates.rule_generations.is_empty() {
            return Ok(());
        }
        // Existing two-second tick: GC is conservative if any host is unavailable.
        let (_, referenced, all_observed) = self.observe_update_hosts().await;
        if all_observed {
            self.collect_rule_generations(&referenced).await?;
        }
        if self.updates.rule_generations.is_empty() {
            return Ok(());
        }
        let revisions = self.source_rule_revisions().await?;
        self.updates.status.source_rule_revisions = revisions
            .iter()
            .map(|(name, revision)| (name.clone(), *revision))
            .collect();
        let changed = self
            .updates
            .rule_revisions
            .as_ref()
            .is_some_and(|baseline| baseline != &revisions);
        if changed {
            let previous = self.updates.previous_bin.clone();
            let result = self.activate_release(self.bin.clone()).await;
            self.updates.previous_bin = previous;
            result?;
        } else if self.updates.rule_revisions.is_none() {
            self.updates.status.effective_rule_revisions =
                self.updates.status.source_rule_revisions.clone();
            self.updates.rule_revisions = Some(revisions);
        }
        Ok(())
    }
    pub(super) async fn update_package(&mut self, package: PathBuf) -> Result<Reply> {
        // Legacy CLI keeps its documented gate. New UIs stage and activate separately.
        use crate::protocol::updates::{UpdateIntent, UpdatePhase, UpdateRequest};
        let reply = self
            .update_transaction(UpdateRequest::new(UpdateIntent::Stage { package }))
            .await?;
        if let Some(operation) = &self.updates.status.operation {
            if operation.phase == UpdatePhase::Failed {
                anyhow::bail!(
                    "{}",
                    operation
                        .error
                        .as_deref()
                        .unwrap_or("Package import failed")
                );
            }
        }
        if self
            .updates
            .status
            .operation
            .as_ref()
            .is_none_or(|op| op.phase != UpdatePhase::Staged)
            || self.updates.status.staged_compatible != Some(true)
            || std::env::var_os("ASTRA_HOSTED_EXPERIMENTAL").is_none()
        {
            return Ok(reply);
        }
        let release_id = self
            .updates
            .status
            .staged_release_id
            .clone()
            .context("No staged release")?;
        let reply = self
            .update_transaction(UpdateRequest::new(UpdateIntent::Activate { release_id }))
            .await?;
        if let Some(operation) = &self.updates.status.operation {
            if operation.phase == UpdatePhase::Failed {
                anyhow::bail!(
                    "{}",
                    operation.error.as_deref().unwrap_or("Activation failed")
                );
            }
        }
        Ok(reply)
    }

    pub(super) async fn activate_release(&mut self, directory: PathBuf) -> Result<()> {
        ensure_unprivileged_updates()?;
        let snapshot = self.source_snapshot().await?;
        self.updates.status.source_rule_revisions = snapshot
            .revisions
            .iter()
            .map(|(name, revision)| (name.clone(), *revision))
            .collect();
        let instances: Vec<_> = self
            .registry
            .instances
            .iter()
            .filter(|instance| {
                instance.kind == Kind::Proxy && self.running.contains_key(&instance.name)
            })
            .cloned()
            .collect();
        let mut hosts = Vec::new();
        for instance in &instances {
            let endpoint = hosted::endpoint(&self.socket(&instance.name));
            let status: Status = serde_json::from_value(
                hosted::request(&endpoint, HostCommand::Status)
                    .await
                    .context("当前实例尚未迁入常驻宿主架构；请先主动断开并完成首次迁移")?,
            )?;
            anyhow::ensure!(
                !status.paused && !status.busy && !status.generations.iter().any(|g| g.prepared),
                "Network host is paused, busy or already prepared"
            );
            if self.updates.tracking {
                anyhow::ensure!(!status.boot_id.is_empty(), "Network host cannot identify its process lifetime; maintenance upgrade required");
            }
            hosts.push((endpoint, status));
        }
        let journal = self.home.join("updates/runtime-transaction.json");
        private_dir(journal.parent().unwrap())?;
        atomic_write(
            &journal,
            &serde_json::to_vec(&json!({"phase":"preparing","before":self.bin,"after":directory}))?,
        )?;
        let mut rules = HashMap::new();
        let mut new_rules = Vec::new();
        let mut candidates: Vec<Candidate> = Vec::new();
        let prepared = async {
            for (instance,(endpoint,host_status)) in instances.iter().zip(&hosts) {
                let previous = host_status.active;
                let proxy:toml::Table=toml::from_str(&snapshot.configs[&instance.name].text)?;
                let source_rule=if proxy.get("standalone").and_then(toml::Value::as_bool)==Some(true) { None } else {
                    let wanted=astra_platform::platform::endpoint_path(proxy.get("rule_socket").and_then(toml::Value::as_str).context("missing rule_socket")?);
                    Some(self.registry.instances.iter().filter(|rule|rule.kind==Kind::Rule).find_map(|rule| {
                        let config:toml::Table=toml::from_str(&snapshot.configs.get(&rule.name)?.text).ok()?;
                        (astra_platform::platform::endpoint_path(config.get("socket")?.as_str()?)==wanted).then(||rule.name.clone())
                    }).context("managed seamless update requires a running registered Rule Core dependency")?)
                };
                let rule_socket = if let Some(name) = source_rule {
                    if !rules.contains_key(&name) {
                        let rule = self.instance(&name)?;
                        let source = snapshot.configs.get(&name).context("Update requires a running registered Rule Core")?;
                        let generation = self.start_update_rule(&directory,&rule,&source.text).await?;
                        rules.insert(name.clone(),generation.socket.clone());
                        new_rules.push(generation);
                    }
                    rules.get(&name).cloned()
                } else {
                    let config: toml::Table = toml::from_str(&snapshot.configs[&instance.name].text)?;
                    anyhow::ensure!(config.get("standalone").and_then(toml::Value::as_bool) == Some(true),
                        "managed seamless update requires a registered Rule Core dependency");
                    None
                };
                self.update_checkpoint(crate::protocol::updates::UpdatePhase::Preparing, |operation| {
                    operation.uncertain_preparations.push(crate::protocol::updates::UpdatePreparation {
                        endpoint: endpoint.clone(), host_boot_id: host_status.boot_id.clone(),
                        previous_generation: previous,
                        known_generations: host_status.generations.iter().map(|g| g.generation).collect(),
                    });
                })?;
                let generation = hosted::request(endpoint,HostCommand::Prepare {directory:directory.clone(),rule_socket}).await?
                    .as_u64().context("host returned no prepared generation")?;
                candidates.push(Candidate {endpoint:endpoint.clone(),generation,previous,host_boot_id:host_status.boot_id.clone()});
                if self.updates.tracking {
                    self.update_checkpoint(crate::protocol::updates::UpdatePhase::Preparing, |operation| {
                        operation.uncertain_preparations.retain(|pending| pending.endpoint != *endpoint);
                        operation.participants.push(crate::protocol::updates::UpdateParticipant {
                            endpoint: endpoint.clone(), host_boot_id: host_status.boot_id.clone(),
                            previous_generation: previous, candidate_generation: generation, resume_needed: false,
                        });
                    })?;
                }
            }
            self.verify_source_snapshot(&snapshot).await
        }.await;
        if let Err(error) = prepared {
            let mut cleanup = Vec::new();
            for candidate in &candidates {
                if let Err(error) = hosted::request(
                    &candidate.endpoint,
                    HostCommand::Abort {
                        generation: candidate.generation,
                    },
                )
                .await
                {
                    cleanup.push(format!("abort {}: {error:#}", candidate.endpoint.display()));
                }
            }
            if cleanup.is_empty() {
                for mut rule in new_rules {
                    if let Err(error) = stop_rule(&mut rule).await {
                        cleanup.push(format!("rule cleanup: {error:#}"));
                        self.updates.rule_generations.push(rule);
                    }
                }
            } else {
                self.updates.rule_generations.extend(new_rules);
            }
            let recorded = atomic_write(
                &journal,
                &serde_json::to_vec(
                    &json!({"phase":"aborted","error":format!("{error:#}"),"cleanup_errors":cleanup}),
                )?,
            );
            if !cleanup.is_empty() {
                let _ = self
                    .update_checkpoint(crate::protocol::updates::UpdatePhase::Uncertain, |_| {});
            }
            return Err(error).with_context(||format!("Preparation failed; previous workers remain active. Cleanup: {cleanup:?}; journal: {recorded:?}"));
        }
        // Stop only admission, never a listener or an existing flow. Once every
        // host is paused, all assignments can change before any is resumed.
        let activated = async {
            self.update_checkpoint(
                crate::protocol::updates::UpdatePhase::Activating,
                |operation| {
                    for participant in &mut operation.participants {
                        participant.resume_needed = true;
                    }
                },
            )?;
            for candidate in &candidates {
                hosted::request(&candidate.endpoint, HostCommand::Pause).await?;
            }
            self.verify_source_snapshot(&snapshot).await?;
            atomic_write(
                &journal,
                &serde_json::to_vec(
                    &json!({"phase":"activating","before":self.bin,"after":directory}),
                )?,
            )?;
            for candidate in &candidates {
                hosted::request(
                    &candidate.endpoint,
                    HostCommand::Activate {
                        generation: candidate.generation,
                    },
                )
                .await?;
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let mut rollback_errors = Vec::new();
        let mut restore_pending = std::collections::HashSet::new();
        let mut operation_record_error = None;
        if activated.is_err() {
            match self.update_checkpoint(
                crate::protocol::updates::UpdatePhase::RestoringPrevious,
                |_| {},
            ) {
                Ok(()) => {
                    for candidate in &candidates {
                        if let Err(error) = restore_previous(candidate).await {
                            restore_pending.insert(candidate.endpoint.clone());
                            rollback_errors
                                .push(format!("{}: {error:#}", candidate.endpoint.display()));
                        }
                    }
                }
                Err(error) => {
                    restore_pending.extend(
                        candidates
                            .iter()
                            .map(|candidate| candidate.endpoint.clone()),
                    );
                    rollback_errors.push(format!("rollback checkpoint: {error:#}"));
                }
            }
        } else {
            // Track the actual runtime state before later resume/disk operations.
            self.updates.previous_bin = Some(std::mem::replace(&mut self.bin, directory));
            self.updates.status.effective_rule_revisions = snapshot
                .revisions
                .iter()
                .map(|(name, revision)| (name.clone(), *revision))
                .collect();
            self.updates.rule_revisions = Some(snapshot.revisions);
            operation_record_error = self
                .update_checkpoint(
                    crate::protocol::updates::UpdatePhase::Resuming,
                    |operation| {
                        operation.runtime_committed = true;
                    },
                )
                .err();
        }
        // A durable failed rollback keeps admission paused: otherwise the host
        // could reap the previous generation before Recover can select it.
        // Existing sessions continue; participants already restored can resume.
        let mut resume_error = None;
        for candidate in &candidates {
            if self.updates.tracking && restore_pending.contains(&candidate.endpoint) {
                continue;
            }
            if let Err(error) = hosted::request(&candidate.endpoint, HostCommand::Resume).await {
                resume_error = Some(error);
            } else if let Err(error) = self.update_checkpoint(
                crate::protocol::updates::UpdatePhase::Resuming,
                |operation| {
                    for participant in &mut operation.participants {
                        if participant.endpoint == candidate.endpoint {
                            participant.resume_needed = false;
                        }
                    }
                },
            ) {
                operation_record_error = Some(error);
            }
        }
        // Keep rule processes for every version still referenced by a worker.
        self.updates.rule_generations.extend(new_rules);
        if let Err(error) = activated {
            let state = if rollback_errors.is_empty() {
                "previous_workers_restored"
            } else {
                "runtime_state_uncertain"
            };
            let recorded = atomic_write(
                &journal,
                &serde_json::to_vec(
                    &json!({"phase":state,"error":format!("{error:#}"),"rollback_errors":rollback_errors,
                "resume_error":resume_error.as_ref().map(|e|format!("{e:#}"))}),
                )?,
            );
            let phase = if rollback_errors.is_empty() && resume_error.is_none() {
                crate::protocol::updates::UpdatePhase::Failed
            } else {
                crate::protocol::updates::UpdatePhase::Uncertain
            };
            let _ = self.update_checkpoint(phase, |_| {});
            return Err(error).with_context(||format!("Activation failed; {state}. Rollback: {rollback_errors:?}; resume: {resume_error:?}; journal: {recorded:?}. Inspect update-status"));
        }
        let recorded = atomic_write(
            &journal,
            &serde_json::to_vec(&json!({"phase":"committed","directory":self.bin,
            "resume_error":resume_error.as_ref().map(|e|format!("{e:#}"))}))?,
        );
        if let Some(error) = resume_error {
            return Err(error).with_context(||format!("New workers are active at {}; failed to resume one ingress; journal: {recorded:?}",self.bin.display()));
        }
        recorded.context("New workers are already active; saving the runtime transaction journal failed. Next-launch selection has not been updated")?;
        if let Some(error) = operation_record_error {
            return Err(error).context("Workers are active; operation journal needs recovery");
        }
        Ok(())
    }

    async fn start_update_rule(
        &self,
        directory: &Path,
        instance: &Instance,
        source: &str,
    ) -> Result<RuleGeneration> {
        use tokio::io::AsyncWriteExt;
        let id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let route = self.home.join(format!("ur-{id}.sock"));
        let control = self.home.join(format!("uc-{id}.sock"));
        let logs = self.home.join("updates/rule-logs");
        private_dir(&logs)?;
        let log = logs.join(format!("{}-{id}.log", instance.name));
        let output = OpenOptions::new()
            .create_new(true)
            .append(true)
            .mode(0o600)
            .open(&log)?;
        set_private(&log, 0o600)?;
        let mut text: toml::Table = toml::from_str(source)?;
        text.insert("socket".into(), route.to_string_lossy().to_string().into());
        text.insert("watch_config".into(), false.into());
        let text = toml::to_string(&text)?;
        let mut command = astra_platform::platform::background_core(
            directory.join(astra_platform::platform::executable("astra-rule")),
        );
        let mut child = command
            .arg("--host-worker")
            .arg(&instance.config)
            .env("ASTRA_CONTROL_SOCKET", &control)
            .env("ASTRA_RUNTIME_DIR", &self.runtime_dir)
            .env("ASTRA_LOG_LEVEL", self.level(&instance.name).as_str())
            .stdin(Stdio::piped())
            .stdout(output.try_clone()?)
            .stderr(output)
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("Start rule generation; log: {}", log.display()))?;
        let ready = async {
            let mut stdin = child
                .stdin
                .take()
                .context("rule worker stdin unavailable")?;
            stdin.write_all(text.as_bytes()).await?;
            drop(stdin);
            timeout(Duration::from_secs(20), async {
                loop {
                    anyhow::ensure!(
                        child.try_wait()?.is_none(),
                        "candidate rule exited before ready"
                    );
                    if let Ok(status) = control::request(&control, Command::Status).await {
                        anyhow::ensure!(
                            child.id() == Some(status.pid),
                            "candidate Rule Core PID mismatch"
                        );
                        return Ok::<_, anyhow::Error>(());
                    }
                    sleep(Duration::from_millis(25)).await;
                }
            })
            .await??;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let mut generation = RuleGeneration {
            child,
            socket: route.to_string_lossy().into_owned(),
            control,
            log,
        };
        if let Err(error) = ready {
            let cleanup = stop_rule(&mut generation).await;
            return Err(error).with_context(|| {
                format!(
                    "Rule generation failed; log retained at {}; cleanup: {cleanup:?}",
                    generation.log.display()
                )
            });
        }
        Ok(generation)
    }

    async fn observe_update_hosts(
        &self,
    ) -> (
        serde_json::Map<String, serde_json::Value>,
        HashSet<String>,
        bool,
    ) {
        let mut hosts = serde_json::Map::new();
        let mut referenced = HashSet::new();
        let mut all_observed = true;
        let reads = self
            .registry
            .instances
            .iter()
            .filter(|instance| {
                instance.kind == Kind::Proxy && self.running.contains_key(&instance.name)
            })
            .map(|instance| {
                (
                    instance.name.clone(),
                    hosted::endpoint(&self.socket(&instance.name)),
                )
            })
            .map(|(name, endpoint)| async move {
                (
                    name,
                    timeout(
                        Duration::from_millis(750),
                        hosted::request(&endpoint, HostCommand::Status),
                    )
                    .await,
                )
            });
        for (name, result) in futures_util::future::join_all(reads).await {
            if let Ok(Ok(value)) = result {
                if let Ok(status) = serde_json::from_value::<Status>(value.clone()) {
                    for generation in status.generations {
                        referenced.insert(generation.rule_socket);
                    }
                    hosts.insert(name, value);
                    continue;
                }
            }
            all_observed = false;
            hosts.insert(name, json!({"state":"legacy_or_unavailable"}));
        }
        (hosts, referenced, all_observed)
    }
    async fn collect_rule_generations(&mut self, referenced: &HashSet<String>) -> Result<()> {
        let mut retained = Vec::new();
        let mut errors = Vec::new();
        for mut rule in self.updates.rule_generations.drain(..) {
            if referenced.contains(&rule.socket) {
                retained.push(rule);
            } else if let Err(error) = stop_rule(&mut rule).await {
                errors.push(format!("{}: {error:#}", rule.log.display()));
                retained.push(rule);
            }
        }
        self.updates.rule_generations = retained;
        anyhow::ensure!(
            errors.is_empty(),
            "Unreferenced rule cleanup deferred: {errors:?}"
        );
        Ok(())
    }
    pub(super) async fn update_status(&mut self) -> Result<Reply> {
        // Queries never collect generations, recover journals or touch the release store.
        Ok(Reply::text(serde_json::to_string(
            &self.cached_update_status(),
        )?))
    }
    pub(super) async fn rollback_update(&mut self) -> Result<Reply> {
        let directory = self
            .updates
            .previous_bin
            .clone()
            .context("没有本次宿主会话可回滚的在线更新")?;
        self.update_transaction(crate::protocol::updates::UpdateRequest::new(
            crate::protocol::updates::UpdateIntent::Rollback {
                target_directory: directory,
            },
        ))
        .await
    }
}

async fn stop_rule(rule: &mut RuleGeneration) -> Result<()> {
    if rule.child.try_wait()?.is_none() {
        rule.child.kill().await?;
    }
    astra_platform::platform::remove_socket(Path::new(&rule.socket));
    astra_platform::platform::remove_socket(&rule.control);
    Ok(())
}

fn require_unprivileged_update(elevated: bool) -> Result<()> {
    anyhow::ensure!(!elevated,
        "Online activation is refused for an elevated manager. Start astra-host as an ordinary user; only the TUN/network helper should be elevated. The package remains staged and existing connections are unchanged");
    Ok(())
}

fn ensure_unprivileged_updates() -> Result<()> {
    #[cfg(windows)]
    let elevated = astra_platform::elevation::is_elevated()?;
    #[cfg(unix)]
    let elevated = unsafe { libc::geteuid() == 0 };
    #[cfg(not(any(windows, unix)))]
    let elevated = true;
    require_unprivileged_update(elevated)
}

#[cfg(test)]
mod unit_tests {
    use super::*;
    #[test]
    fn online_activation_requires_an_ordinary_user_manager() {
        assert!(require_unprivileged_update(false).is_ok());
        let error = require_unprivileged_update(true).unwrap_err().to_string();
        assert!(error.contains("elevated manager"));
        assert!(error.contains("existing connections are unchanged"));
    }
}
