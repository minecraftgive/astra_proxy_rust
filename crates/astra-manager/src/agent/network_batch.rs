//! One Agent-owned batch; all child processes are joined before cancellation is acknowledged.
use super::{network_admission, AgentService};
use anyhow::{ensure, Context, Result};
use astra_management::v2::*;
use astra_model::{
    nettool::{Event, Probe},
    network_analysis::ProjectContext,
    network_state::Interface,
};
use futures_util::{stream::FuturesUnordered, StreamExt};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{mpsc, watch};

pub(crate) fn validate(batch: &NetworkBatchRequest) -> Result<()> {
    ensure!(
        !batch.projects.is_empty() && batch.projects.len() <= 32,
        crate::error::message(
            ErrorCode::InvalidRequest,
            "A diagnostic batch requires 1–32 projects"
        )
    );
    let mut ids = HashSet::new();
    let mut paths = HashMap::new();
    let mut nat = 0;
    for project in &batch.projects {
        let valid_id = |id: &str| {
            !id.is_empty()
                && id.len() <= 64
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        };
        ensure!(
            valid_id(&project.id) && valid_id(&project.path_id),
            crate::error::message(
                ErrorCode::InvalidRequest,
                "Invalid diagnostic project/path identifier"
            )
        );
        ensure!(
            ids.insert(&project.id),
            crate::error::message(
                ErrorCode::InvalidRequest,
                "Duplicate diagnostic project identifier"
            )
        );
        crate::network::validate(&project.request)?;
        let identity =
            serde_json::to_string(&(&project.request.egress, &project.request.resolver))?;
        if let Some(previous) = paths.insert(&project.path_id, identity.clone()) {
            ensure!(
                previous == identity,
                crate::error::message(
                    ErrorCode::InvalidRequest,
                    "A path identifier cannot refer to different exits or resolvers"
                )
            );
        }
        if matches!(project.request.probe, Probe::Nat { .. }) {
            nat += 1;
        }
        if batch.comparison && comparable(&project.request.probe) {
            ensure!(
                project.request.count == 1,
                crate::error::message(
                    ErrorCode::InvalidRequest,
                    "Comparison projects must use one sample per round"
                )
            );
        }
    }
    ensure!(
        paths.len() <= 3,
        crate::error::message(
            ErrorCode::InvalidRequest,
            "At most three diagnostic paths per batch"
        )
    );
    ensure!(
        nat <= 1,
        crate::error::message(
            ErrorCode::InvalidRequest,
            "At most one manually selected NAT project per batch"
        )
    );
    Ok(())
}
fn comparable(probe: &Probe) -> bool {
    matches!(probe, Probe::Dns { .. } | Probe::Tls { .. })
}

impl AgentService {
    pub(super) async fn submit_network_batch(
        &self,
        id: String,
        instance: String,
        batch: NetworkBatchRequest,
        fingerprint: String,
    ) -> Result<Task> {
        validate(&batch)?;
        let mut interfaces = HashMap::new();
        for project in &batch.projects {
            if let Probe::Nat { request } = &project.request.probe {
                interfaces.insert(
                    project.id.clone(),
                    self.validate_nat_interface(request).await?,
                );
            }
        }
        let exclusive = !interfaces.is_empty();
        let native = self
            .native
            .get()
            .cloned()
            .context("Native actor not initialized")?;
        let store = self.store.clone();
        let scheduler = self.diagnostics.clone();
        let agent_id = self.identity.id.clone();
        self.diagnostics.submit_mode(id, fingerprint, exclusive, move |output, cancel| async move {
            let instance = store.resolve(&instance).await?;
            let (stop, local_cancel)=watch::channel(*cancel.borrow());
            let mut parent_cancel=cancel;
            let runner = Runner { native, scheduler, agent_id, instance, output, cancel:local_cancel, stop:stop.clone(), comparison: batch.comparison };
            let run=async {
                if !interfaces.is_empty() {
                    for project in batch.projects.iter().filter(|p|!matches!(p.request.probe,Probe::Nat{..})) {
                        runner.status(project,1,"queued","等待 NAT 检测完成").await?;
                    }
                }
            // NAT has both parent scheduler permits. Finish and reap it before
            // ordinary projects start, so even this batch cannot overlap NAT.
            for project in batch.projects.iter().filter(|p| matches!(p.request.probe, Probe::Nat { .. })) {
                runner.project(project, 1, interfaces.get(&project.id)).await?;
            }
            let ordinary: Vec<_> = batch.projects.iter().filter(|p| !matches!(p.request.probe, Probe::Nat { .. })).collect();
            let comparison_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
            runner.round(&ordinary, 1).await?;
            if batch.comparison {
                let comparisons: Vec<_> = ordinary.into_iter().filter(|p| comparable(&p.request.probe)).collect();
                if tokio::time::Instant::now() < comparison_deadline && !*runner.cancel.borrow() {
                    runner.round(&comparisons, 2).await?;
                } else {
                    for project in comparisons {
                        runner.status(project, 2, "skipped", "Comparison window expired or batch cancelled; no extra probe started").await?;
                    }
                }
            }
            Ok(())
            };
            tokio::pin!(run);
            tokio::select! {
                biased;
                _=crate::network::cancelled(&mut parent_cancel)=>{stop.send_replace(true);run.await},
                result=&mut run=>result,
            }
        }).await
    }
}
struct Runner {
    native: crate::manager::NativeClient,
    scheduler: Arc<crate::network::Scheduler>,
    agent_id: String,
    instance: String,
    output: mpsc::Sender<Event>,
    cancel: watch::Receiver<bool>,
    stop: watch::Sender<bool>,
    comparison: bool,
}
impl Runner {
    async fn status(
        &self,
        project: &NetworkBatchProject,
        round: u8,
        state: &str,
        message: &str,
    ) -> Result<()> {
        self.output
            .send(Event::ProjectStatus {
                project_id: project.id.clone(),
                path_id: project.path_id.clone(),
                round,
                state: state.into(),
                message: message.chars().take(1500).collect(),
            })
            .await
            .context("Batch result receiver closed")
    }
    async fn round(&self, projects: &[&NetworkBatchProject], round: u8) -> Result<()> {
        let mut work = FuturesUnordered::new();
        for project in projects {
            work.push(self.project(project, round, None));
        }
        let mut cleanup_error = None;
        while let Some(result) = work.next().await {
            if let Err(error) = result {
                self.stop.send_replace(true);
                // A result-forwarding failure must not hide a later child's
                // unconfirmed cleanup, which keeps the scheduler blocked.
                if cleanup_error.is_none()
                    || error
                        .chain()
                        .any(|cause| cause.is::<crate::network::CleanupUnconfirmed>())
                {
                    cleanup_error = Some(error);
                }
            }
        }
        // Do not abandon another owned child on any error.
        cleanup_error.map_or(Ok(()), Err)
    }
    async fn project(
        &self,
        project: &NetworkBatchProject,
        round: u8,
        before: Option<&Interface>,
    ) -> Result<()> {
        self.status(
            project,
            round,
            "queued",
            "Waiting for a diagnostic project slot",
        )
        .await?;
        let mut cancel = self.cancel.clone();
        let permit = tokio::select! {
            biased;
            _ = crate::network::cancelled(&mut cancel) => {
                self.status(project, round, "cancelled", "Cancelled before probe started").await?;
                return Ok(());
            }
            permit = self.scheduler.project_permit() => permit?,
        };
        let result = async {
            if let (Some(before), Probe::Nat { request }) = (before, &project.request.probe) {
                let snapshot = astra_platform::network_snapshot::Sampler::default()
                    .sample(&self.agent_id)
                    .await;
                let after =
                    network_admission::physical_interface(request, &snapshot, &self.agent_id)?;
                ensure!(
                    network_admission::same_interface(before, &after),
                    "NAT physical interface changed while queued; choose it again"
                );
            }
            if *cancel.borrow() {
                return Ok(());
            }
            self.status(project, round, "running", "Diagnostic project running")
                .await?;
            let (output, mut events) = mpsc::channel(16);
            let request = project.request.clone().into_core()?;
            let run =
                self.native
                    .network_diagnostic(&self.instance, request, output, cancel.clone());
            let forward = async {
                let mut index = 0_u32;
                while let Some(event) = events.recv().await {
                    index += 1;
                    let context = ProjectContext {
                        project_id: project.id.clone(),
                        path_id: project.path_id.clone(),
                        round,
                        index,
                        sampled_at_ms: SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis()
                            .to_string(),
                        comparison_requested: self.comparison,
                    };
                    // Even if a reader fails, keep draining while the owned
                    // process exits; the scheduler cancels on storage failure.
                    let _ = self
                        .output
                        .send(Event::Project {
                            project: context,
                            detail: Box::new(event),
                        })
                        .await;
                }
            };
            let (result, ()) = tokio::join!(run, forward);
            result
        }
        .await;
        drop(permit);
        let state = if result.is_err() {
            "failed"
        } else if *cancel.borrow() {
            "cancelled"
        } else {
            "complete"
        };
        let message = result.as_ref().err().map_or_else(
            || "Project finished; individual attempts may contain failures".into(),
            |error| format!("{error:#}"),
        );
        let status = self.status(project, round, state, &message).await;
        if result.as_ref().err().is_some_and(|e| {
            e.chain()
                .any(|cause| cause.is::<crate::network::CleanupUnconfirmed>())
        }) {
            result
        } else {
            status
        }
    }
}

#[cfg(test)]
#[path = "../test/network_batch_test.rs"]
mod tests;
