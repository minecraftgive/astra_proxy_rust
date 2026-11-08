//! Frontend coordination for the independent maintenance helper.
use crate::{waker::Waker, windows::Context};
use anyhow::{ensure, Context as _, Result};
use astra_management::{
    maintenance::{self as m, Phase, Record},
    v2::*,
};
use futures_util::FutureExt;
use sha2::{Digest, Sha256};
use std::{path::Path, process::Stdio};

#[derive(Default)]
pub struct State {
    pub record: Option<Record>,
    pub error: Option<String>,
    pub log: Option<String>,
    pub staged: Option<String>,
    pub cancel: bool,
    pub terminal: bool,
    lease: Option<m::FrontendLease>,
    acknowledged: Option<String>,
    validated_ready: Option<String>,
    validation: Option<(String, tokio::task::JoinHandle<Result<()>>)>,
    painted: bool,
    painted_ready: Option<String>,
    pub authorization: Option<(Record, &'static str)>,
    observed_at: Option<std::time::Instant>,
    preparing: Option<tokio::task::JoinHandle<Result<Record>>>,
    target: Option<Context>,
    following: bool,
    completed: bool,
    startup_updated: bool,
    pub recovery: Option<&'static str>,
}
impl State {
    pub fn register(&mut self, context: &Context) -> Result<()> {
        if crate::protocol::target().is_none() {
            self.lease = Some(m::register_frontend(&context.home, &context.executable)?);
        }
        Ok(())
    }
    pub fn needs_checkpoint(&self) -> Option<String> {
        self.record
            .as_ref()
            .filter(|r| {
                r.phase == Phase::Prepared && self.acknowledged.as_ref() != Some(&r.checkpoint_id)
            })
            .map(|r| r.checkpoint_id.clone())
    }
    pub fn acknowledge(&mut self, id: &str) -> Result<()> {
        let record = self
            .record
            .as_ref()
            .context("Missing maintenance checkpoint")?;
        ensure!(record.checkpoint_id == id, "Maintenance checkpoint changed");
        m::acknowledge(record)?;
        self.acknowledged = Some(id.into());
        Ok(())
    }

    pub fn mark_ready(&mut self, context: &Context) -> Result<()> {
        self.painted = true;
        if self.record.as_ref().is_some_and(|r| {
            r.phase == Phase::Ready
                && self.validated_ready.as_deref() == Some(&r.checkpoint_id)
                && target_is_current(context, r)
                && self.painted_ready.as_deref() != Some(&r.checkpoint_id)
        }) {
            m::finish_switch(&context.home, &std::fs::canonicalize(&context.executable)?)?;
            self.painted_ready = self.record.as_ref().map(|r| r.checkpoint_id.clone());
        }
        Ok(())
    }
    pub fn active(&self) -> bool {
        self.preparing.is_some() || self.following || self.authorization.is_some()
    }
    pub fn failed(&self) -> bool {
        self.error.is_some()
            || self
                .record
                .as_ref()
                .is_some_and(|r| r.phase == Phase::Failed)
    }
    pub fn label(&self) -> String {
        if let Some(error) = &self.error {
            return format!(
                "{}: {error}",
                crate::i18n::t("Maintenance failed; manual action required")
            );
        }
        self.record
            .as_ref()
            .map(|r| {
                format!(
                    "{}{}",
                    crate::i18n::t(r.phase.label()),
                    r.error
                        .as_ref()
                        .map(|e| format!(": {e}"))
                        .unwrap_or_default()
                )
            })
            .unwrap_or_default()
    }
    /// Returns true only after the backend has completed this exact transition.
    /// Call after the frontend's durable session checkpoint.
    pub fn checkpointed(
        &mut self,
        context: &Context,
        target: &Context,
        waker: Waker,
        silent: bool,
    ) -> Result<bool> {
        ensure!(
            crate::protocol::target().is_none(),
            "Maintenance is available only on the local host"
        );
        if self.completed && self.target.as_ref().is_some_and(|t| t.bin == target.bin) {
            if !self.startup_updated {
                #[cfg(windows)]
                use crate::startup as registration;
                #[cfg(unix)]
                use crate::startup_unix as registration;
                for frontend in [
                    crate::preferences::StartupFrontend::Tui,
                    crate::preferences::StartupFrontend::Gui,
                ] {
                    let old = context.for_frontend(frontend)?;
                    if registration::enabled(&old)? {
                        let new = target.for_frontend(frontend)?;
                        ensure!(
                            new.executable.is_file(),
                            "The updated package lacks a registered frontend"
                        );
                        registration::set_enabled(&new, true, silent)?;
                    }
                }
                self.startup_updated = true;
            }
            return Ok(true);
        }
        if self.active() {
            return Ok(false);
        }
        self.error = None;
        self.target = Some(target.clone());
        let context = context.clone();
        let target = target.clone();
        self.preparing = Some(tokio::spawn(async move {
            let result = prepare(&context, &target).await;
            waker.wake();
            result
        }));
        Ok(false)
    }
    fn launch(&mut self, record: &Record, action: &'static str) -> Result<()> {
        self.observed_at = Some(std::time::Instant::now());
        if self.terminal
            && record
                .instances
                .iter()
                .any(|i| i.runtime.as_ref().is_some_and(|r| r.tun == Some(true)))
        {
            self.authorization = Some((record.clone(), action));
        } else {
            let mut child = spawn_helper(record, action, false)?;
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Ok(())
    }
    pub fn poll(&mut self, context: &Context) -> Result<Option<Context>> {
        if self
            .preparing
            .as_ref()
            .is_some_and(|task| task.is_finished())
        {
            match self
                .preparing
                .take()
                .unwrap()
                .now_or_never()
                .expect("finished maintenance preparation")?
            {
                Ok(record) => {
                    if let Err(error) = self.launch(&record, "run") {
                        m::request_cancel(&context.home, &record.operation_id)?;
                        if let Ok(_lock) = m::lock(&context.home) {
                            let mut cancelled = record.clone();
                            cancelled.phase = Phase::Cancelled;
                            m::write(&cancelled)?;
                        }
                        self.error = Some(format!("{error:#}"));
                        self.following = false;
                        self.record = Some(record);
                        return Ok(None);
                    }
                    self.record = Some(record);
                    self.following = true;
                }
                Err(error) => {
                    self.error = Some(format!("{error:#}"));
                    self.following = false;
                    return Ok(None);
                }
            }
        }
        if self.cancel && self.preparing.is_none() {
            self.cancel = false;
            if let Some(record) = m::read(&context.home)? {
                ensure!(
                    record.cancellable(),
                    "Shutdown has begun; cancellation is no longer available"
                );
                m::request_cancel(&context.home, &record.operation_id)?;
                if let Ok(_lock) = m::lock(&context.home) {
                    let mut cancelled = record;
                    cancelled.phase = Phase::Cancelled;
                    m::write(&cancelled)?;
                }
            }
        }
        if let Some(action) = self.recovery.take() {
            let record = m::read(&context.home)?.context("No maintenance operation to recover")?;
            ensure!(!record.phase.terminal(), "Maintenance already finished");
            self.launch(&record, action)?;
            self.error = None;
            self.completed = false;
            self.following = true;
            self.target = None;
        }
        {
            if let Some(record) = m::read(&context.home)? {
                if self
                    .record
                    .as_ref()
                    .is_some_and(|previous| previous.checkpoint_id != record.checkpoint_id)
                {
                    self.error = None;
                    self.completed = false;
                }
                if !record.phase.terminal() {
                    self.following = true;
                }
                self.observed_at.get_or_insert_with(std::time::Instant::now);
                if self.following {
                    if matches!(record.phase, Phase::Ready | Phase::Complete) {
                        let mut target = context.clone();
                        let name = context
                            .executable
                            .file_name()
                            .context("Unknown frontend executable")?;
                        if self.validated_ready.as_deref() != Some(&record.checkpoint_id) {
                            if self
                                .validation
                                .as_ref()
                                .is_some_and(|(id, _)| id != &record.checkpoint_id)
                            {
                                self.validation.take().unwrap().1.abort();
                            }
                            if self.validation.is_none() && self.error.is_none() {
                                let target = record.clone();
                                self.validation = Some((
                                    record.checkpoint_id.clone(),
                                    tokio::task::spawn_blocking(move || {
                                        let manifest = astra_update::verify_installation_directory(
                                            &target.target,
                                            &Default::default(),
                                        )?;
                                        ensure!(
                                            manifest.release_id == target.release_id
                                                && manifest_hash(&target.target)?
                                                    == target.manifest_sha256,
                                            "Completed maintenance package changed"
                                        );
                                        Ok(())
                                    }),
                                ));
                            }
                            if self
                                .validation
                                .as_ref()
                                .is_some_and(|(_, task)| task.is_finished())
                            {
                                let (_, task) = self.validation.take().unwrap();
                                match task
                                    .now_or_never()
                                    .expect("finished maintenance validation")?
                                {
                                    Ok(()) => {
                                        self.validated_ready = Some(record.checkpoint_id.clone())
                                    }
                                    Err(error) => self.error = Some(format!("{error:#}")),
                                }
                            }
                            if self.validated_ready.as_deref() != Some(&record.checkpoint_id) {
                                self.record = Some(record);
                                return Ok(None);
                            }
                        }
                        target.executable = record.target.join(name);
                        target.bin = record.target.clone();
                        {
                            ensure!(target.executable.is_file(), "Updated frontend is missing");
                            self.target = Some(target);
                            self.completed = true;
                        }
                        if target_is_current(context, &record) {
                            self.completed = false;
                            self.target = None;
                        }
                        self.following = record.phase == Phase::Ready;
                    } else if record.phase == Phase::Failed || record.phase == Phase::Cancelled {
                        self.following = false;
                        self.completed = false;
                        self.error = record.error.clone();
                    } else if self.preparing.is_none()
                        && self.authorization.is_none()
                        && (record.helper.as_ref().is_some_and(|p| {
                            astra_platform::lifecycle::matches_live(p).is_ok_and(|live| !live)
                        }) || (record.helper.is_none()
                            && self
                                .observed_at
                                .is_some_and(|at| at.elapsed().as_secs() >= 5)))
                    {
                        self.following = false;
                        self.error = Some("Maintenance helper exited; choose retry or restore the previous version".into());
                    }
                }
                self.record = Some(record);
            }
        }
        if self.painted {
            self.mark_ready(context)?;
        }
        Ok(if self.completed {
            self.target.clone()
        } else {
            None
        })
    }
}
pub fn manifest_hash(directory: &Path) -> Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(astra_platform::storage::read_bounded(
            &directory.join(astra_update::MANIFEST_FILE),
            astra_update::MANIFEST_LIMIT
        )?)
    ))
}
async fn prepare(context: &Context, target: &Context) -> Result<Record> {
    let socket = local_endpoint(&context.home)?;
    let ResponseBody::Hello(hello) =
        crate::protocol::request(&socket, RequestBody::Query(Query::Hello)).await?
    else {
        anyhow::bail!("Cannot identify local manager")
    };
    ensure!(hello.capabilities.iter().any(|c| c == m::CAPABILITY), "The running manager needs one manual maintenance upgrade before automatic maintenance is available");
    let digest = manifest_hash(&target.bin)?;
    let ResponseBody::MaintenanceStatus(Some(record)) = crate::protocol::request(
        &socket,
        RequestBody::Local(LocalCommand::PrepareMaintenance {
            target_directory: target.bin.clone(),
            expected_manifest_sha256: digest,
            expected_epoch: hello.epoch,
        }),
    )
    .await?
    else {
        anyhow::bail!("Invalid maintenance preparation response")
    };
    Ok(*record)
}
fn spawn_helper(record: &Record, action: &str, terminal: bool) -> Result<std::process::Child> {
    ensure!(
        matches!(action, "run" | "retry" | "previous"),
        "Invalid maintenance action"
    );
    // Always run the verified new helper; it can select the previous payload
    // explicitly without requiring old releases to understand this protocol.
    let helper = m::coordinator(record)?;
    let log_path = m::directory(&record.config.state_dir).join("operation.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    astra_platform::platform::set_private(&log_path, 0o600)?;
    let mut command = std::process::Command::new(helper);
    command
        .arg(format!("--maintenance-{action}"))
        .arg(&record.config.state_dir)
        .arg(&record.operation_id)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    if terminal {
        command
            .env("ASTRA_MAINTENANCE_TTY", "1")
            .stdin(Stdio::inherit());
    } else {
        astra_platform::platform::background_command(&mut command, false);
    }
    command.spawn().context("Launch maintenance helper")
}

pub fn selected(home: &Path) -> Result<Option<std::path::PathBuf>> {
    if m::pending(home)? {
        return Ok(None);
    }
    let path = m::directory(home).join("launch.json");
    let bytes = match astra_platform::storage::read_bounded(&path, 16384) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    #[derive(serde::Deserialize)]
    struct Selection {
        directory: std::path::PathBuf,
        manifest_sha256: String,
    }
    let selection: Selection = serde_json::from_slice(&bytes)?;
    astra_update::verify_installation_directory(&selection.directory, &Default::default())?;
    ensure!(
        manifest_hash(&selection.directory)? == selection.manifest_sha256,
        "Maintenance launch selection changed"
    );
    Ok(Some(selection.directory))
}

/// The TUI suspends raw mode for this bounded authorization phase. The
/// coordinator detaches after authorization and remains independent of the UI.
pub async fn authorize_terminal(record: Record, action: &'static str) -> Result<()> {
    let mut child = spawn_helper(&record, action, true)?;
    let mut prepared = action == "run";
    loop {
        if let Some(status) = child.try_wait()? {
            ensure!(
                status.success(),
                "Maintenance helper stopped; see the maintenance log"
            );
            return Ok(());
        }
        if let Some(current) = m::read(&record.config.state_dir)? {
            ensure!(
                current.operation_id == record.operation_id,
                "Maintenance operation changed"
            );
            prepared |= current.phase == Phase::Prepared;
            if prepared && !matches!(current.phase, Phase::Prepared) {
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                return Ok(());
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

pub fn local_endpoint(home: &Path) -> Result<std::path::PathBuf> {
    let saved = m::directory(home).join("manager.toml");
    if saved.is_file() {
        let config = astra_management::config::ManagerConfig::load(&saved)?;
        ensure!(
            config.state_dir == home,
            "Saved maintenance configuration uses different storage"
        );
        Ok(config.endpoint())
    } else {
        Ok(home.join("management.sock"))
    }
}

fn target_is_current(context: &Context, record: &Record) -> bool {
    std::fs::canonicalize(&context.executable)
        .is_ok_and(|path| path.parent() == Some(record.target.as_path()))
}
