//! A bounded dedicated SQLite executor. Filesystem/database waits never occupy a Tokio worker.
use crate::store::Store;
use anyhow::Result;
use astra_management::v2::*;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};
type Job = Box<dyn FnOnce(&mut Store) + Send>;
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
        let (sender, receiver) = tokio::sync::oneshot::channel();
        self.sender
            .send(Box::new(move |store| {
                let _ = sender.send(work(store));
            }))
            .await
            .map_err(|_| self.worker_error())?;
        match receiver.await {
            Ok(result) => result,
            Err(_) => Err(self.closed().await),
        }
    }
    pub async fn begin(&self, id: &str, fingerprint: &str) -> Result<Option<ResponseBody>> {
        let id = id.to_owned();
        let fingerprint = fingerprint.to_owned();
        self.call(move |s| s.begin(&id, &fingerprint)).await
    }
    pub async fn finish(
        &self,
        id: &str,
        result: Option<&ResponseBody>,
        error: Option<&str>,
    ) -> Result<()> {
        let id = id.to_owned();
        let result = result.cloned();
        let error = error.map(str::to_owned);
        self.call(move |s| s.finish(&id, result.as_ref(), error.as_deref()))
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
        self.call(move |s| s.event(id.as_deref(), &kind, &message))
            .await
    }
    pub async fn draft(&self, id: &str) -> Result<Draft> {
        let id = id.to_owned();
        self.call(move |s| s.draft(&id)).await
    }
    pub async fn open_draft(&self, id: String, doc: Document) -> Result<Draft> {
        self.call(move |s| s.open_draft(id, doc)).await
    }
    pub async fn replace_draft(&self, draft: Draft, expected: &str) -> Result<Draft> {
        let expected = expected.to_owned();
        self.call(move |s| s.replace_draft(draft, &expected)).await
    }
    pub async fn discard(&self, id: &str, revision: &str) -> Result<()> {
        let id = id.to_owned();
        let revision = revision.to_owned();
        self.call(move |s| s.discard(&id, &revision)).await
    }
    pub async fn servers(&self) -> Result<Vec<(String, PathBuf)>> {
        self.call(|s| s.servers()).await
    }
    pub async fn add_server(&self, name: &str, path: &Path) -> Result<()> {
        let name = name.to_owned();
        let path = path.to_owned();
        self.call(move |s| s.add_server(&name, &path)).await
    }
    pub async fn identity(&self, name: &str) -> Result<String> {
        let name = name.to_owned();
        self.call(move |s| s.identity(&name)).await
    }
    pub async fn resolve(&self, reference: &str) -> Result<String> {
        let reference = reference.to_owned();
        self.call(move |s| s.resolve(&reference)).await
    }
    pub async fn forget_identity(&self, name: &str) -> Result<()> {
        let name = name.to_owned();
        self.call(move |s| s.forget_identity(&name)).await
    }
    pub async fn remove_server(&self, name: &str) -> Result<()> {
        let name = name.to_owned();
        self.call(move |s| s.remove_server(&name)).await
    }
    pub async fn set_server_config(&self, name: &str, path: &Path) -> Result<()> {
        let name = name.to_owned();
        let path = path.to_owned();
        self.call(move |s| s.set_server_config(&name, &path)).await
    }
    pub async fn remember_agent(&self, agent: Agent) -> Result<()> {
        self.call(move |s| s.remember_agent(agent)).await
    }
    pub async fn snapshot(&self, id: &str, body: &str) -> Result<()> {
        let id = id.to_owned();
        let body = body.to_owned();
        self.call(move |s| s.snapshot(&id, &body)).await
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
        self.call(move |s| s.route(&operation, &agent, &request, &state))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    #[tokio::test]
    async fn short_contention_recovers_without_blocking_the_async_runtime() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("management.sqlite");
        let database = Database::open(path.clone(), 100).unwrap();
        let blocker = rusqlite::Connection::open(path).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        let (started, worker_started) = tokio::sync::oneshot::channel();
        let write = database.call(move |store| {
            let _ = started.send(());
            store.begin("wait-once", "input")
        });
        tokio::pin!(write);
        tokio::select! {
            result=&mut write=>panic!("write bypassed external lock: {result:?}"),
            _=worker_started=>{},
        }
        assert!(tokio::time::timeout(Duration::from_millis(50), &mut write)
            .await
            .is_err());
        blocker.execute_batch("ROLLBACK").unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(2), write)
            .await
            .unwrap()
            .unwrap()
            .is_none());
        assert!(matches!(
            database.begin("wait-once", "input").await.unwrap(),
            Some(ResponseBody::Task(_))
        ));
    }
    #[tokio::test]
    async fn worker_panic_is_fatal_and_pending_operation_is_never_replayed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("management.sqlite");
        let database = Database::open(path.clone(), 100).unwrap();
        database
            .begin("accepted-before-panic", "input")
            .await
            .unwrap();
        let error = database
            .call::<()>(|_| panic!("injected SQLite worker failure"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("worker panicked"));
        let closed = tokio::time::timeout(Duration::from_secs(1), database.closed())
            .await
            .unwrap();
        assert!(closed.to_string().contains("not restarted"));
        assert!(database
            .begin("must-not-run", "input")
            .await
            .unwrap_err()
            .to_string()
            .contains("worker panicked"));
        drop(database);
        // Only an explicit new management startup may open another worker.
        let restarted = Database::open(path, 100).unwrap();
        assert!(matches!(
            restarted
                .begin("accepted-before-panic", "input")
                .await
                .unwrap(),
            Some(ResponseBody::Task(Task {
                state: TaskState::OutcomeUnknown,
                ..
            }))
        ));
        assert!(restarted.task("must-not-run").await.is_err());
    }
}
