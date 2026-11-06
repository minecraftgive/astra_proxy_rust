//! Bounded frontend-independent operation execution on a Tokio LocalSet.
use crate::{
    operations::{self, JobResult, OperationInput},
    protocol::*,
    state::Prompt,
    waker::Waker,
};
use astra_ipc::progress::{self, Progress};
use std::path::{Path, PathBuf};
use tokio::{sync::watch, task::JoinHandle};

#[derive(Clone)]
pub struct Origin {
    pub selected: Option<String>,
    pub view_context: (usize, Kind, Option<String>),
    pub overlay_epoch: u64,
}
impl Origin {
    pub fn apply_context(
        &self,
        action: &Action,
        current: &Self,
        log_open: bool,
    ) -> operations::ApplyContext {
        let same = self.selected == current.selected && self.view_context == current.view_context
            || match action {
                Action::Create { name, .. } => current.selected.as_ref() == Some(name),
                Action::Add(instance) => current.selected.as_ref() == Some(&instance.name),
                _ => false,
            };
        let tui_log = matches!(action, Action::Logs(name) if name == "@tui" || name == "@gui");
        operations::ApplyContext {
            same,
            log_target_current: !tui_log
                || (self.overlay_epoch == current.overlay_epoch && log_open),
            background: true,
        }
    }
}

pub struct Job {
    pub id: u64,
    pub action: Action,
    pub selected: Option<String>,
    pub progress: watch::Receiver<Option<Progress>>,
    pub handle: JoinHandle<JobResult>,
    pub progress_wake: JoinHandle<()>,
    pub view_context: (usize, Kind, Option<String>),
    pub overlay_epoch: u64,
}
pub struct Jobs {
    pub socket: PathBuf,
    pub next: u64,
    pub jobs: Vec<Job>,
    pub changed: bool,
    pub log: Option<std::sync::Arc<crate::tui_log::Log>>,
    waker: Waker,
}
impl Drop for Jobs {
    fn drop(&mut self) {
        for job in &self.jobs {
            job.handle.abort();
            job.progress_wake.abort();
        }
    }
}
impl Jobs {
    pub fn new(socket: &Path, waker: Waker) -> Self {
        Self {
            socket: socket.into(),
            next: 0,
            jobs: vec![],
            changed: false,
            waker,
            log: socket
                .parent()
                .and_then(|p| crate::tui_log::Log::open(&p.join("tui.log")).ok())
                .map(std::sync::Arc::new),
        }
    }
    pub fn check(&self, action: &Action) -> anyhow::Result<()> {
        anyhow::ensure!(self.jobs.len() < 8, "任务队列已满，请等待现有任务完成");
        if let Action::Save { name, .. } = action {
            anyhow::ensure!(
                !self.jobs.iter().any(
                    |j| matches!(&j.action, Action::Save { name: other, .. } if other == name)
                ),
                "此配置正在保存；可继续编辑，完成后再保存"
            );
        }
        Ok(())
    }
    pub fn start(
        &mut self,
        input: OperationInput,
        action: Action,
        prompt: Option<(Prompt, String)>,
        origin: Origin,
    ) -> anyhow::Result<u64> {
        self.check(&action)?;
        let input = input.with_waker(self.waker.clone());
        self.next += 1;
        let id = self.next;
        let (sender, receiver) = watch::channel(None);
        let mut signals = receiver.clone();
        let waker = self.waker.clone();
        let progress_wake = tokio::spawn(async move {
            while signals.changed().await.is_ok() {
                waker.wake();
            }
        });
        let socket = self.socket.clone();
        let request = action.clone();
        let waker = self.waker.clone();
        let handle = tokio::task::spawn_local(async move {
            let result =
                progress::scope(sender, operations::perform(&socket, input, request, prompt)).await;
            waker.wake();
            result
        });
        self.jobs.push(Job {
            id,
            action,
            selected: origin.selected,
            progress: receiver,
            handle,
            progress_wake,
            view_context: origin.view_context,
            overlay_epoch: origin.overlay_epoch,
        });
        Ok(id)
    }
    pub fn cancel(&mut self, id: u64) {
        if let Some(index) = self.jobs.iter().position(|j| j.id == id) {
            let job = self.jobs.remove(index);
            job.handle.abort();
            job.progress_wake.abort();
            self.waker.wake();
        }
    }
    pub fn take_changed(&mut self) -> bool {
        std::mem::take(&mut self.changed)
    }
}

pub enum Event {
    Progress {
        id: u64,
        value: Progress,
    },
    Done {
        id: u64,
        action: Action,
        origin: Origin,
        result: Result<Box<JobResult>, String>,
    },
}
impl Jobs {
    /// Drain ready events without waiting for an unfinished operation.
    pub async fn poll(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        let mut index = 0;
        while index < self.jobs.len() {
            let job = &mut self.jobs[index];
            if job.progress.has_changed().unwrap_or(false) {
                if let Some(value) = job.progress.borrow_and_update().clone() {
                    events.push(Event::Progress { id: job.id, value });
                }
            }
            if !job.handle.is_finished() {
                index += 1;
                continue;
            }
            let job = self.jobs.remove(index);
            job.progress_wake.abort();
            let origin = Origin {
                selected: job.selected,
                view_context: job.view_context,
                overlay_epoch: job.overlay_epoch,
            };
            events.push(Event::Done {
                id: job.id,
                action: job.action,
                origin,
                result: job
                    .handle
                    .await
                    .map(Box::new)
                    .map_err(|error| error.to_string()),
            });
        }
        events
    }
}

#[cfg(test)]
#[path = "test/jobs_test.rs"]
mod tests;
