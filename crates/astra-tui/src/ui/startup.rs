//! One startup pass per primary window, using the normal asynchronous start path.
use super::*;
use std::collections::VecDeque;

pub(super) struct Instances {
    awaiting_snapshot: bool,
    pending: VecDeque<String>,
    refresh: Option<tokio::task::JoinHandle<Result<Reply>>>,
}

impl Drop for Instances {
    fn drop(&mut self) {
        if let Some(refresh) = self.refresh.take() {
            refresh.abort();
        }
    }
}

use astra_client::startup_instances::*;
pub(crate) use astra_client::startup_instances::{log_startup, run_silent};

impl Instances {
    pub(super) fn resumed() -> Self {
        Self {
            awaiting_snapshot: false,
            pending: VecDeque::new(),
            refresh: None,
        }
    }

    pub(super) fn new(launch: &crate::windows::Launch) -> Self {
        Self {
            awaiting_snapshot: launch.window_id.is_none(),
            pending: VecDeque::new(),
            refresh: None,
        }
    }

    fn prepare(&mut self, ui: &Ui, ready: bool) -> bool {
        if !ready {
            return false;
        }
        if self.awaiting_snapshot {
            self.awaiting_snapshot = false;
            self.pending = candidates(&ui.rows, &ui.details);
        }
        // Serial dispatch avoids overflowing the bounded job queue. A config
        // created by PrepareConfig or a failed start must remain reviewable.
        let ready = !self.pending.is_empty()
            && !ui.client.tasks.running()
            && ui.recovery.is_none()
            && ui.prompt.is_none()
            && ui.editor.is_none();
        if !ready {
            // Foreground actions can invalidate a snapshot fetched earlier.
            if let Some(refresh) = self.refresh.take() {
                refresh.abort();
            }
        }
        ready
    }

    fn next_action(
        &mut self,
        rows: &[Row],
        details: &HashMap<String, InstanceDetails>,
    ) -> Option<Action> {
        prioritize(&mut self.pending, rows, details);
        while let Some(name) = self.pending.pop_front() {
            if let Some(row) = rows
                .iter()
                .find(|row| row.instance.name == name && selected(row, details))
            {
                discard_covered_proxies(&mut self.pending, row, details);
                return Some(Action::Start(name));
            }
        }
        None
    }

    pub(super) async fn poll(
        &mut self,
        ui: &mut Ui,
        socket: &std::path::Path,
        ready: bool,
    ) -> Option<Action> {
        if !self.prepare(ui, ready) {
            return None;
        }
        if self.refresh.is_none() {
            // Watch feeds can lag a completed start (and its bound instances).
            // Fetch a fresh list before every dispatch without blocking input;
            // keep this private snapshot out of the subscription-owned UI rows.
            let socket = socket.to_owned();
            self.refresh = Some(tokio::spawn(async move {
                let result = call(&socket, Action::List).await;
                super::wake();
                result
            }));
            return None;
        }
        if !self.refresh.as_ref().unwrap().is_finished() {
            return None;
        }
        let result = self.refresh.take().unwrap().await;
        match result
            .map_err(anyhow::Error::from)
            .and_then(|result| result)
            .and_then(|reply| {
                let details = serde_json::from_str(&reply.text)
                    .context("Refresh startup instance details")?;
                Ok((reply, details))
            }) {
            Ok((reply, details)) => self.next_action(&reply.rows, &details),
            Err(error) => {
                // Do not act on stale state when the manager is unavailable.
                self.pending.clear();
                ui.fail(format!("{error:#}"), Resume::Request(Action::List));
                None
            }
        }
    }
}

#[cfg(test)]
#[path = "../test/ui_startup_test.rs"]
mod tests;
