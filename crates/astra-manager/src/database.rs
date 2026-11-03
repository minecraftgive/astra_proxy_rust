//! A bounded dedicated SQLite executor. Filesystem/database waits never occupy a Tokio worker.
use crate::store::Store;
use anyhow::Result;
use astra_management::v2::*;
use std::path::{Path, PathBuf};
type Job = Box<dyn FnOnce(&mut Store) + Send>;
#[derive(Clone)]
pub struct Database {
    sender: tokio::sync::mpsc::Sender<Job>,
}
impl Database {
    pub fn open(path: PathBuf, event_limit: usize) -> Result<Self> {
        let (sender, mut jobs) = tokio::sync::mpsc::channel::<Job>(64);
        let (ready, wait) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("astra-management-db".into())
            .spawn(move || match Store::open(&path) {
                Ok(mut store) => {
                    store.set_event_limit(event_limit);
                    let _ = ready.send(Ok(()));
                    while let Some(job) = jobs.blocking_recv() {
                        job(&mut store);
                    }
                }
                Err(error) => {
                    let _ = ready.send(Err(error));
                }
            })?;
        wait.recv()??;
        Ok(Self { sender })
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
            .map_err(|_| anyhow::anyhow!("database worker stopped"))?;
        receiver.await?
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
