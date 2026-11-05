//! Durable request identity and repair of committed updates. Recovery never replays activation.
use super::*;
use crate::protocol::updates::{
    UpdateIntent, UpdateOperation, UpdatePhase, UpdateRequest, UpdateStatus,
};

#[cfg(test)]
#[path = "../../test/manager_updates_transactions_recovery_test.rs"]
mod recovery_tests;

fn operation_path(home: &Path, id: &str) -> Result<PathBuf> {
    anyhow::ensure!(
        !id.is_empty()
            && id.len() <= 128
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "Invalid update operation ID"
    );
    Ok(home.join("updates/operations").join(format!("{id}.json")))
}

fn read_optional<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}
impl Updates {
    pub(in crate::manager) fn load(home: &Path, bin: &Path) -> Result<Self> {
        let root = home.join("updates");
        let mut status = UpdateStatus {
            runtime_directory: bin.into(),
            ..Default::default()
        };
        status.next_launch =
            read_optional::<Option<astra_update::CurrentRelease>>(&root.join("current.json"))?
                .flatten();
        if let Some(pending) = read_optional::<serde_json::Value>(&root.join("pending.json"))? {
            status.staged_release_id = pending
                .get("release_id")
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            status.staged_compatible = pending.get("compatible_host").and_then(|v| v.as_bool());
        }
        let entries = match fs::read_dir(root.join("operations")) {
            Ok(entries) => Some(entries),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        if let Some(entries) = entries {
            for entry in entries {
                let entry = entry?;
                if entry.path().extension().is_none_or(|e| e != "json") {
                    continue;
                }
                let operation: UpdateOperation = serde_json::from_slice(&fs::read(entry.path())?)?;
                anyhow::ensure!(
                    operation_path(home, &operation.request.operation_id)? == entry.path(),
                    "Update journal identity mismatch"
                );
                // An unfinished operation must stay visible even if a later staged import exists.
                let replace = status.operation.as_ref().is_none_or(|old| {
                    (old.phase.terminal() && !operation.phase.terminal())
                        || (old.phase.terminal() == operation.phase.terminal()
                            && operation.updated_at_ms > old.updated_at_ms)
                });
                if replace {
                    status.operation = Some(operation);
                }
            }
        }
        if let Some(operation) = &mut status.operation {
            if !operation.phase.terminal() {
                operation.phase = UpdatePhase::Uncertain;
                operation.error = Some("The manager restarted during this operation; inspect and recover its recorded participants before another activation".into());
            }
        }
        Ok(Self {
            status,
            ..Default::default()
        })
    }
}
impl Manager {
    pub(in crate::manager) fn lookup_update_operation(
        home: &Path,
        mut status: UpdateStatus,
        id: &str,
    ) -> Result<Reply> {
        let path = operation_path(home, id)?;
        if status
            .operation
            .as_ref()
            .is_none_or(|operation| operation.request.operation_id != id)
        {
            let operation: UpdateOperation =
                read_optional(&path)?.context("Unknown update operation")?;
            anyhow::ensure!(
                operation.request.operation_id == id,
                "Update journal identity mismatch"
            );
            status.operation = Some(operation);
        }
        Ok(Reply::text(serde_json::to_string(&status)?))
    }
    pub(in crate::manager) fn cached_update_status(&self) -> UpdateStatus {
        let mut status = self.updates.status.clone();
        status.runtime_directory = self.bin.clone();
        status.capabilities.experimental_enabled =
            std::env::var_os("ASTRA_HOSTED_EXPERIMENTAL").is_some();
        status.capabilities.ordinary_user = ensure_unprivileged_updates().is_ok();
        status.capabilities.operation_running = self.updates.tracking;
        status.capabilities.rollback_target = self.updates.previous_bin.clone();
        status.capabilities.recovery_operation_id = status
            .operation
            .as_ref()
            .filter(|op| !op.phase.terminal() && !self.updates.tracking)
            .map(|op| op.request.operation_id.clone());
        let reason = if !status.capabilities.experimental_enabled {
            Some("Online activation is disabled in this host session; packages can be staged")
        } else if !status.capabilities.ordinary_user {
            Some("Online activation requires an ordinary-user manager")
        } else if status.capabilities.operation_running
            || status.capabilities.recovery_operation_id.is_some()
        {
            Some("An update is running or requires recovery")
        } else {
            None
        };
        status.capabilities.activation_allowed = reason.is_none();
        status.capabilities.activation_blocked_reason = reason.map(str::to_owned);
        status
    }
    pub(in crate::manager) fn publish_update_status(&self) {
        if let Some(hub) = &self.publisher {
            hub.publish_updates(self.cached_update_status());
        }
    }
    fn persist_update_operation(&mut self) -> Result<()> {
        if let Some(operation) = &mut self.updates.status.operation {
            operation.updated_at_ms = astra_ipc::progress::now_ms();
            let path = operation_path(&self.home, &operation.request.operation_id)?;
            private_dir(path.parent().unwrap())?;
            let result = atomic_write(&path, &serde_json::to_vec_pretty(operation)?);
            self.publish_update_status();
            result?;
        }
        Ok(())
    }
    pub(super) fn update_checkpoint(
        &mut self,
        phase: UpdatePhase,
        change: impl FnOnce(&mut UpdateOperation),
    ) -> Result<()> {
        if !self.updates.tracking {
            return Ok(());
        }
        if let Some(operation) = &mut self.updates.status.operation {
            operation.phase = phase;
            change(operation);
        }
        astra_ipc::progress::report(
            phase.label(),
            "Update operation recorded by the resident manager",
        );
        self.persist_update_operation()
    }
    fn operation_reply(&self) -> Result<Reply> {
        let status = self.publisher.as_ref().map_or_else(
            || self.cached_update_status(),
            |hub| hub.update_status_snapshot(),
        );
        Ok(Reply::text(serde_json::to_string(&status)?))
    }
    pub(in crate::manager) async fn update_transaction(
        &mut self,
        request: UpdateRequest,
    ) -> Result<Reply> {
        let path = operation_path(&self.home, &request.operation_id)?;
        if let Some(current) = self
            .updates
            .status
            .operation
            .as_ref()
            .filter(|op| op.request.operation_id == request.operation_id)
        {
            anyhow::ensure!(
                current.request == request,
                "Operation ID is already bound to another update target"
            );
            return self.operation_reply();
        }
        if let Some(previous) = read_optional::<UpdateOperation>(&path)? {
            anyhow::ensure!(
                previous.request == request,
                "Operation ID is already bound to another update target"
            );
            // Retrying an acknowledged/partially acknowledged request is an observation, not a replay.
            let mut status = self.cached_update_status();
            status.operation = Some(previous);
            return Ok(Reply::text(serde_json::to_string(&status)?));
        }
        anyhow::ensure!(
            self.updates
                .status
                .operation
                .as_ref()
                .is_none_or(|op| op.phase.terminal()),
            "An earlier update requires recovery; query update status first"
        );
        anyhow::ensure!(
            self.blocked.is_empty() || matches!(&request.intent, UpdateIntent::Stage { .. }),
            "Unverified managed processes prevent online activation; existing generations retained"
        );
        let target = match &request.intent {
            UpdateIntent::Stage { .. } => None,
            UpdateIntent::Activate { release_id } => {
                Some(ReleaseStore::new(&self.home).release_dir(release_id)?)
            }
            UpdateIntent::Rollback { target_directory } => {
                anyhow::ensure!(
                    self.updates.previous_bin.as_ref() == Some(target_directory),
                    "Rollback target changed; refresh update status"
                );
                Some(target_directory.clone())
            }
        };
        self.updates.status.operation = Some(UpdateOperation {
            request,
            phase: UpdatePhase::Validating,
            before_directory: self.bin.clone(),
            target_directory: target,
            release_id: None,
            runtime_committed: false,
            next_launch_saved: false,
            pending_record_saved: false,
            participants: vec![],
            uncertain_preparations: vec![],
            error: None,
            updated_at_ms: astra_ipc::progress::now_ms(),
        });
        self.updates.tracking = true;
        if let Err(error) = self.persist_update_operation() {
            return self.finish_update_attempt(Err(
                error.context("Update journal could not be created; no runtime work started")
            ));
        }
        let result = self.execute_update_operation().await;
        self.finish_update_attempt(result)
    }
    fn finish_update_attempt(&mut self, result: Result<()>) -> Result<Reply> {
        if let Err(error) = result {
            if let Some(operation) = &mut self.updates.status.operation {
                operation.error = Some(format!("{error:#}"));
                if !operation.uncertain_preparations.is_empty() {
                    operation.phase = UpdatePhase::Uncertain;
                } else if operation.phase != UpdatePhase::Uncertain {
                    operation.phase = if operation.runtime_committed
                        || operation.participants.iter().any(|p| p.resume_needed)
                    {
                        UpdatePhase::NeedsRecovery
                    } else {
                        UpdatePhase::Failed
                    };
                }
            }
        }
        self.updates.tracking = false;
        // Publish even when the filesystem is unavailable; never pretend runtime work was undone.
        if let Err(error) = self.persist_update_operation() {
            if let Some(operation) = &mut self.updates.status.operation {
                operation.error = Some(format!(
                    "{}; journal write failed: {error:#}",
                    operation
                        .error
                        .as_deref()
                        .unwrap_or("Runtime result recorded in memory")
                ));
                if operation.runtime_committed {
                    operation.phase = UpdatePhase::NeedsRecovery;
                }
            }
            self.publish_update_status();
        }
        self.operation_reply()
    }
    async fn execute_update_operation(&mut self) -> Result<()> {
        let operation = self
            .updates
            .status
            .operation
            .clone()
            .context("Missing update operation")?;
        let store = ReleaseStore::new(&self.home);
        if let UpdateIntent::Stage { package } = &operation.request.intent {
            let importer = store.clone();
            let package = package.clone();
            let installed = tokio::task::spawn_blocking(move || {
                importer.import(package, &ImportOptions::default())
            })
            .await??;
            self.updates.status.staged_release_id = Some(installed.manifest.release_id.clone());
            self.updates.status.staged_compatible = Some(installed.compatible_host);
            atomic_write(
                &store.root().join("pending.json"),
                &serde_json::to_vec_pretty(&json!({
                    "release_id":installed.manifest.release_id,"directory":installed.directory,
                    "compatible_host":installed.compatible_host,"state":"staged"
                }))?,
            )?;
            return self.update_checkpoint(UpdatePhase::Staged, |op| {
                op.release_id = Some(installed.manifest.release_id);
                op.target_directory = Some(installed.directory);
                op.pending_record_saved = true;
            });
        }
        anyhow::ensure!(
            std::env::var_os("ASTRA_HOSTED_EXPERIMENTAL").is_some(),
            "Online activation is disabled; package remains staged"
        );
        ensure_unprivileged_updates()?;
        let target = operation
            .target_directory
            .clone()
            .context("Missing fixed update target")?;
        if let UpdateIntent::Activate { release_id } = &operation.request.intent {
            let importer = store.clone();
            let checked = target.clone();
            let installed = tokio::task::spawn_blocking(move || {
                importer.import(
                    checked,
                    &ImportOptions {
                        enforce_host_protocol: true,
                        ..Default::default()
                    },
                )
            })
            .await??;
            anyhow::ensure!(
                &installed.manifest.release_id == release_id,
                "Release identity changed"
            );
            self.update_checkpoint(UpdatePhase::Preparing, |op| {
                op.release_id = Some(release_id.clone())
            })?;
        } else {
            self.update_checkpoint(UpdatePhase::Preparing, |_| {})?;
        }
        self.activate_release(target).await?;
        self.complete_committed_update().await
    }
    async fn complete_committed_update(&mut self) -> Result<()> {
        self.update_checkpoint(UpdatePhase::SavingSelection, |_| {})?;
        let operation = self
            .updates
            .status
            .operation
            .clone()
            .context("Missing update operation")?;
        anyhow::ensure!(
            operation.runtime_committed,
            "No committed runtime transition to persist"
        );
        let target = operation
            .target_directory
            .context("Missing update target")?;
        let store = ReleaseStore::new(&self.home);
        if !operation.next_launch_saved {
            if target.starts_with(store.root().join("releases")) {
                let manifest: astra_update::ReleaseManifest =
                    serde_json::from_slice(&fs::read(target.join(astra_update::MANIFEST_FILE))?)?;
                let current = store.commit_current(&manifest.release_id)?;
                self.updates.status.next_launch = Some(current);
            } else {
                store.clear_current()?;
                self.updates.status.next_launch = None;
            }
            self.update_checkpoint(UpdatePhase::SavingSelection, |op| {
                op.next_launch_saved = true
            })?;
        }
        if !operation.pending_record_saved {
            atomic_write(
                &store.root().join("pending.json"),
                &serde_json::to_vec_pretty(&json!({
                    "release_id":operation.release_id,"directory":target,"operation_id":operation.request.operation_id,
                    "state":"active_workers_pending_host_restart"
                }))?,
            )?;
            self.update_checkpoint(UpdatePhase::SavingSelection, |op| {
                op.pending_record_saved = true
            })?;
        }
        self.update_checkpoint(UpdatePhase::Complete, |op| op.error = None)
    }
    pub(in crate::manager) async fn recover_update(&mut self, id: &str) -> Result<Reply> {
        anyhow::ensure!(
            self.blocked.is_empty(),
            "Unverified managed processes prevent update recovery; existing generations retained"
        );
        operation_path(&self.home, id)?;
        let current = self
            .updates
            .status
            .operation
            .clone()
            .filter(|op| op.request.operation_id == id);
        let operation = match current {
            Some(operation) => operation,
            None => read_optional::<UpdateOperation>(&operation_path(&self.home, id)?)?
                .context("Unknown update operation")?,
        };
        if operation.phase.terminal() {
            let mut status = self.cached_update_status();
            status.operation = Some(operation);
            return Ok(Reply::text(serde_json::to_string(&status)?));
        }
        anyhow::ensure!(
            self.updates
                .status
                .operation
                .as_ref()
                .is_none_or(|op| op.request.operation_id == id || op.phase.terminal()),
            "Another operation requires recovery"
        );
        self.updates.status.operation = Some(operation);
        self.updates.tracking = true;
        let result = self.repair_recorded_operation().await;
        self.finish_update_attempt(result)
    }
    async fn repair_recorded_operation(&mut self) -> Result<()> {
        let mut operation = self.updates.status.operation.clone().unwrap();
        if matches!(operation.request.intent, UpdateIntent::Stage { .. }) {
            return self.execute_update_operation().await;
        }
        // Observe every participant before issuing a repair. Generation numbers alone are not identities.
        let mut preparations = Vec::new();
        for preparation in &operation.uncertain_preparations {
            let status: Status = serde_json::from_value(
                hosted::request(&preparation.endpoint, HostCommand::Status).await?,
            )?;
            anyhow::ensure!(
                !preparation.host_boot_id.is_empty()
                    && status.boot_id == preparation.host_boot_id
                    && status.active == preparation.previous_generation
                    && !status.busy,
                "Unacknowledged preparation cannot yet be reconciled; its host changed or is busy"
            );
            let unknown: Vec<_> = status
                .generations
                .into_iter()
                .filter(|g| !preparation.known_generations.contains(&g.generation))
                .collect();
            anyhow::ensure!(unknown.iter().all(|g| g.prepared && Some(&g.directory) == operation.target_directory.as_ref()),
                "Unacknowledged preparation has unexpected generations; inspect the host before maintenance");
            preparations.push((preparation, unknown));
        }
        let mut observed = Vec::new();
        for participant in &operation.participants {
            let status: Status = serde_json::from_value(
                hosted::request(&participant.endpoint, HostCommand::Status).await?,
            )?;
            anyhow::ensure!(
                !participant.host_boot_id.is_empty() && status.boot_id == participant.host_boot_id,
                "Host restarted; recorded generation IDs cannot be recovered automatically"
            );
            anyhow::ensure!(
                status.active == participant.candidate_generation
                    || (!operation.runtime_committed
                        && status.active == participant.previous_generation),
                "Active generation changed after this operation; inspect status before maintenance"
            );
            observed.push(status);
        }
        // A crash can happen after the last host switched but before the commit
        // checkpoint reached disk. The actual, boot-checked assignment is decisive;
        // never reverse a fully applied transition merely because its reply was lost.
        if !operation.runtime_committed
            && !operation.participants.is_empty()
            && operation.uncertain_preparations.is_empty()
            && operation
                .participants
                .iter()
                .zip(&observed)
                .all(|(participant, status)| status.active == participant.candidate_generation)
        {
            self.update_checkpoint(UpdatePhase::Resuming, |op| op.runtime_committed = true)?;
            operation.runtime_committed = true;
        }
        for (preparation, generations) in preparations {
            for generation in generations {
                hosted::request(
                    &preparation.endpoint,
                    HostCommand::Abort {
                        generation: generation.generation,
                    },
                )
                .await?;
            }
            self.update_checkpoint(UpdatePhase::Preparing, |op| {
                op.uncertain_preparations
                    .retain(|pending| pending.endpoint != preparation.endpoint);
            })?;
        }
        for (participant, status) in operation.participants.iter().zip(observed) {
            let mut must_resume = participant.resume_needed || status.paused;
            if !operation.runtime_committed {
                if status.active == participant.candidate_generation {
                    anyhow::ensure!(status.generations.iter().any(|generation|generation.generation == participant.previous_generation && !generation.failed),
                        "Previous generation is unavailable; no new generation was started during recovery");
                    self.update_checkpoint(UpdatePhase::RestoringPrevious, |operation| {
                        for pending in &mut operation.participants {
                            if pending.endpoint == participant.endpoint {
                                pending.resume_needed = true;
                            }
                        }
                    })?;
                    hosted::request(&participant.endpoint, HostCommand::Pause).await?;
                    must_resume = true;
                    hosted::request(
                        &participant.endpoint,
                        HostCommand::SelectExisting {
                            generation: participant.previous_generation,
                        },
                    )
                    .await?;
                }
                if status
                    .generations
                    .iter()
                    .any(|g| g.generation == participant.candidate_generation && g.prepared)
                {
                    hosted::request(
                        &participant.endpoint,
                        HostCommand::Abort {
                            generation: participant.candidate_generation,
                        },
                    )
                    .await?;
                }
            }
            if must_resume {
                hosted::request(&participant.endpoint, HostCommand::Resume).await?;
            }
            self.update_checkpoint(UpdatePhase::Resuming, |op| {
                for p in &mut op.participants {
                    if p.endpoint == participant.endpoint {
                        p.resume_needed = false;
                    }
                }
            })?;
        }
        if operation.runtime_committed {
            self.bin = operation
                .target_directory
                .clone()
                .context("Missing committed target")?;
            self.complete_committed_update().await
        } else {
            self.update_checkpoint(UpdatePhase::Failed, |op| op.error = Some("Previous workers restored; activation was not replayed. Start a new operation to try again".into()))
        }
    }
}

#[cfg(test)]
#[path = "../../test/manager_updates_transactions_test.rs"]
mod tests;
