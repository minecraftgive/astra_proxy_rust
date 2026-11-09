//! A bounded dedicated SQLite executor. Filesystem/database waits never occupy a Tokio worker.
use crate::store::{FinishOutcome, Store};
use anyhow::Result;
use astra_management::v2::*;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};
type Job = Box<dyn FnOnce(&mut Store) + Send>;
#[derive(Clone, Copy)]
enum CallKind {
    Read,
    Write,
}
#[derive(Clone)]
pub struct Database {
    sender: tokio::sync::mpsc::Sender<Job>,
    failure: Arc<OnceLock<String>>,
}
impl Database {
    pub async fn closed(&self) -> anyhow::Error {
        self.sender.closed().await;
        self.worker_error()
    }
    fn worker_error(&self) -> anyhow::Error {
        anyhow::anyhow!("{}",self.failure.get().map(String::as_str).unwrap_or(
            "Management SQLite worker stopped unexpectedly; pending write outcomes may be unknown; worker was not restarted"))
    }
    pub fn open(path: PathBuf, event_limit: usize) -> Result<Self> {
        let (sender, mut jobs) = tokio::sync::mpsc::channel::<Job>(64);
        let (ready, wait) = std::sync::mpsc::sync_channel(1);
        let failure = Arc::new(OnceLock::new());
        let worker_failure = failure.clone();
        std::thread::Builder::new()
            .name("astra-management-db".into())
            .spawn(move || {
                let mut ready=Some(ready);
                // Catch only to publish the fatal state and reject queued work.
                // The connection and worker are never reused after a panic.
                let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
                    let mut store=Store::open(&path)?;
                    store.set_event_limit(event_limit);
                    let _=ready.take().unwrap().send(Ok(()));
                    while let Some(job)=jobs.blocking_recv() {job(&mut store);}
                    Ok(())
                }));
                let error=match result {
                    Ok(Err(error))=>Some(error),
                    Err(_)=>Some(anyhow::anyhow!("Management SQLite worker panicked; it was not restarted. Pending write outcomes may be unknown. Retain the database with any -wal and -shm files; inspect the process panic log before restarting management")),
                    Ok(Ok(()))=>None,
                };
                if let Some(error)=error {
                    let _=worker_failure.set(format!("{error:#}"));
                    if let Some(ready)=ready.take(){let _=ready.send(Err(error));}
                }
                // Publish the reason before closing so supervisor/callers do
                // not replace a fatal failure with a generic channel error.
                drop(jobs);
            })?;
        wait.recv().map_err(|error| {
            anyhow::anyhow!("Management SQLite worker exited before readiness: {error}")
        })??;
        Ok(Self { sender, failure })
    }
    async fn call<T: Send + 'static>(
        &self,
        work: impl FnOnce(&mut Store) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        self.dispatch(CallKind::Read, work).await
    }
    async fn write<T: Send + 'static>(
        &self,
        work: impl FnOnce(&mut Store) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        self.dispatch(CallKind::Write, work).await
    }
    async fn dispatch<T: Send + 'static>(
        &self,
        kind: CallKind,
        work: impl FnOnce(&mut Store) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        self.sender
            .send(Box::new(move |store| {
                let _ = sender.send(work(store));
            }))
            .await
            // A rejected send never handed this operation to the worker.
            .map_err(|_| crate::error::classified(ErrorCode::Internal, self.worker_error()))?;
        match receiver.await {
            Ok(result) => result.map_err(|error| {
                // Keep local storage failures explicitly classified so a
                // caller cannot mistake an untyped SQL/JSON error for remote
                // transport failure. Preserve existing markers and causes.
                if error
                    .chain()
                    .any(|cause| cause.is::<crate::error::AgentError>())
                {
                    error
                } else {
                    crate::error::classified(crate::error::code(&error), error)
                }
            }),
            // Once queued, a write may have committed before its receipt was
            // lost. Never claim rejection or replay it based on error text.
            Err(_) => Err(crate::error::classified(
                match kind {
                    CallKind::Read => ErrorCode::Internal,
                    CallKind::Write => ErrorCode::OutcomeUnknown,
                },
                self.closed().await,
            )),
        }
    }
    pub async fn begin(&self, id: &str, fingerprint: &str) -> Result<Option<ResponseBody>> {
        let id = id.to_owned();
        let fingerprint = fingerprint.to_owned();
        self.write(move |s| s.begin(&id, &fingerprint)).await
    }
    pub async fn finish(
        &self,
        id: &str,
        result: Option<&ResponseBody>,
        outcome: FinishOutcome,
    ) -> Result<()> {
        let id = id.to_owned();
        let result = result.cloned();
        self.write(move |s| s.finish(&id, result.as_ref(), outcome))
            .await
    }
    pub async fn task(&self, id: &str) -> Result<Task> {
        let id = id.to_owned();
        self.call(move |s| s.task(&id)).await
    }
    pub async fn tasks(&self, limit: u16) -> Result<Vec<Task>> {
        self.call(move |s| s.tasks(limit)).await
    }
    pub async fn events(&self, after: Option<&str>, limit: u16) -> Result<EventBatch> {
        let after = after.map(str::to_owned);
        self.call(move |s| s.events(after.as_deref(), limit)).await
    }
    pub async fn event(&self, id: Option<&str>, kind: &str, message: &str) -> Result<()> {
        let id = id.map(str::to_owned);
        let kind = kind.to_owned();
        let message = message.to_owned();
        self.write(move |s| s.event(id.as_deref(), &kind, &message))
            .await
    }
    pub async fn network_terminal(&self, id: &str) -> Result<bool> {
        let id = id.to_owned();
        self.call(move |s| s.network_terminal(&id)).await
    }
    pub async fn prune_network(&self) -> Result<()> {
        self.write(|s| s.prune_network()).await
    }
    pub async fn network_existing(&self, id: &str, fingerprint: &str) -> Result<Option<Task>> {
        let (id, fingerprint) = (id.to_owned(), fingerprint.to_owned());
        self.call(move |s| s.network_existing(&id, &fingerprint))
            .await
    }
    pub async fn begin_network(&self, id: &str, fingerprint: &str) -> Result<Task> {
        let (id, fingerprint) = (id.to_owned(), fingerprint.to_owned());
        self.write(move |s| s.begin_network(&id, &fingerprint))
            .await
    }
    pub async fn append_network(&self, id: &str, data: serde_json::Value) -> Result<()> {
        let id = id.to_owned();
        self.write(move |s| s.append_network(&id, data)).await
    }
    pub async fn network_truncated(&self, id: &str) -> Result<()> {
        let id = id.to_owned();
        self.write(move |s| s.network_truncated(&id)).await
    }
    pub async fn network_state(&self, id: &str, state: NetworkState, message: &str) -> Result<()> {
        let (id, message) = (id.to_owned(), message.to_owned());
        self.write(move |s| s.network_state(&id, state, &message))
            .await
    }
    pub async fn network_report(&self, id: &str, cursor: Option<&str>) -> Result<NetworkReport> {
        let (id, cursor) = (id.to_owned(), cursor.map(str::to_owned));
        // Reading a report also commits evidence-retention pruning.
        self.write(move |s| s.network_report(&id, cursor.as_deref()))
            .await
    }
    pub async fn draft(&self, id: &str) -> Result<Draft> {
        let id = id.to_owned();
        self.call(move |s| s.draft(&id)).await
    }
    pub async fn open_draft(&self, id: String, doc: Document) -> Result<Draft> {
        self.write(move |s| s.open_draft(id, doc)).await
    }
    pub async fn replace_draft(&self, draft: Draft, expected: &str) -> Result<Draft> {
        let expected = expected.to_owned();
        self.write(move |s| s.replace_draft(draft, &expected)).await
    }
    pub async fn discard(&self, id: &str, revision: &str) -> Result<()> {
        let id = id.to_owned();
        let revision = revision.to_owned();
        self.write(move |s| s.discard(&id, &revision)).await
    }
    pub async fn servers(&self) -> Result<Vec<(String, PathBuf)>> {
        self.call(|s| s.servers()).await
    }
    pub async fn add_server(&self, name: &str, path: &Path) -> Result<()> {
        let name = name.to_owned();
        let path = path.to_owned();
        self.write(move |s| s.add_server(&name, &path)).await
    }
    pub async fn identity(&self, name: &str) -> Result<String> {
        let name = name.to_owned();
        self.write(move |s| s.identity(&name)).await
    }
    pub async fn resolve(&self, reference: &str) -> Result<String> {
        let reference = reference.to_owned();
        self.call(move |s| s.resolve(&reference)).await
    }
    pub async fn forget_identity(&self, name: &str) -> Result<()> {
        let name = name.to_owned();
        self.write(move |s| s.forget_identity(&name)).await
    }
    pub async fn remove_server(&self, name: &str) -> Result<()> {
        let name = name.to_owned();
        self.write(move |s| s.remove_server(&name)).await
    }
    pub async fn set_server_config(&self, name: &str, path: &Path) -> Result<()> {
        let name = name.to_owned();
        let path = path.to_owned();
        self.write(move |s| s.set_server_config(&name, &path)).await
    }
    pub async fn remember_agent(&self, agent: Agent) -> Result<()> {
        self.write(move |s| s.remember_agent(agent)).await
    }
    pub async fn snapshot(&self, id: &str, body: &str) -> Result<()> {
        let id = id.to_owned();
        let body = body.to_owned();
        self.write(move |s| s.snapshot(&id, &body)).await
    }
    pub async fn remembered_agents(&self) -> Result<Vec<Agent>> {
        self.call(|s| s.remembered_agents()).await
    }
    pub async fn route(
        &self,
        operation: &str,
        agent: &str,
        request: &str,
        state: &str,
    ) -> Result<()> {
        let operation = operation.to_owned();
        let agent = agent.to_owned();
        let request = request.to_owned();
        let state = state.to_owned();
        self.write(move |s| s.route(&operation, &agent, &request, &state))
            .await
    }
}

#[cfg(test)]
#[path = "test/database_test.rs"]
mod tests;
