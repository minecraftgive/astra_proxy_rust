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

fn stopped(row: &Row) -> bool {
    row.pid.is_none() && (row.state == "Stopped" || row.state.starts_with("Stopped ("))
}

fn selected(row: &Row, details: &HashMap<String, InstanceDetails>) -> bool {
    stopped(row)
        && details
            .get(&row.instance.name)
            .is_some_and(|detail| detail.auto_start && !detail.config_unavailable)
}

fn candidates(rows: &[Row], details: &HashMap<String, InstanceDetails>) -> VecDeque<String> {
    let mut roots = rows
        .iter()
        .filter(|row| selected(row, details))
        .map(|row| row.instance.name.clone())
        .collect();
    prioritize(&mut roots, rows, details);
    roots
}

fn prioritize(
    pending: &mut VecDeque<String>,
    rows: &[Row],
    details: &HashMap<String, InstanceDetails>,
) {
    // An explicitly selected bound Rule starts all its bound proxies. Give it
    // priority over individual proxies using the latest bindings, without
    // adding instances that were not selected by the initial snapshot.
    pending.make_contiguous().sort_by_key(|name| {
        if rows
            .iter()
            .any(|row| row.instance.name == *name && row.instance.kind == Kind::Rule)
        {
            if details
                .get(name)
                .is_some_and(|detail| !detail.bindings.is_empty())
            {
                0
            } else {
                2
            }
        } else {
            1
        }
    });
}

fn discard_covered_proxies(
    pending: &mut VecDeque<String>,
    row: &Row,
    details: &HashMap<String, InstanceDetails>,
) {
    if row.instance.kind == Kind::Rule {
        if let Some(detail) = details.get(&row.instance.name) {
            // Defer deduplication until dispatch: if the Rule is disabled or
            // its bindings change while queued, its proxies remain eligible.
            // Once dispatched, do not retry its plan through a bound proxy.
            pending.retain(|name| !detail.bindings.contains(name));
        }
    }
}

/// A file-backed record for launches that have no terminal to display errors.
pub(crate) fn log_startup(home: &std::path::Path, message: &str) -> Result<()> {
    use astra_platform::platform::OpenOptionsExt;
    use std::io::Write;
    astra_platform::storage::private_dir(home)
        .with_context(|| format!("Prepare startup log directory {}", home.display()))?;
    let path = home.join("startup.log");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("Open startup log {}", path.display()))?;
    let message: String = message
        .chars()
        .filter(|c| !c.is_control())
        .take(4096)
        .collect();
    writeln!(file, "[{}] {message}", chrono::Local::now().to_rfc3339())?;
    file.sync_data()?;
    Ok(())
}

/// Headless launch: never restore a UI session, enter raw mode or create a
/// missing configuration. The manager retains validation and rollback policy.
/// The same path is used by Windows sign-in, macOS launchd and Linux systemd;
/// failures are logged instead of opening a terminal or requesting elevation.
pub(crate) async fn run_silent(socket: PathBuf, context: crate::windows::Context) -> Result<()> {
    log_startup(&context.home, "Silent startup started")?;
    let result: Result<()> = async {
        let initial = call(&socket, Action::List)
            .await
            .context("Read instances for silent startup")?;
        let details =
            serde_json::from_str(&initial.text).context("Read startup instance details")?;
        let mut names = candidates(&initial.rows, &details);
        if names.is_empty() {
            log_startup(
                &context.home,
                "No stopped instances have auto_start enabled",
            )?;
        }
        let mut failures = Vec::new();
        while !names.is_empty() {
            // Starts can also start bound instances; always recheck current state.
            let current = call(&socket, Action::List)
                .await
                .context("Refresh instances for silent startup")?;
            let details =
                serde_json::from_str(&current.text).context("Refresh startup instance details")?;
            prioritize(&mut names, &current.rows, &details);
            let name = names.pop_front().unwrap();
            let selected = current
                .rows
                .iter()
                .find(|row| row.instance.name == name && selected(row, &details));
            let Some(row) = selected else {
                log_startup(
                    &context.home,
                    &format!(
                        "Skipped {name}: no longer stopped, registered or enabled for auto_start"
                    ),
                )?;
                continue;
            };
            discard_covered_proxies(&mut names, row, &details);
            log_startup(&context.home, &format!("Starting {name}"))?;
            // This dedicated request fails closed against older managers and
            // atomically rejects interactive elevation across the whole plan.
            // Do not fall back to Start, ForceStart or PrepareConfig here.
            match call(&socket, Action::StartSilent(name.clone())).await {
                Ok(_) => log_startup(&context.home, &format!("Started {name}"))?,
                Err(error) => {
                    let failure = format!("{name}: {error:#}");
                    log_startup(&context.home, &format!("Failed {failure}"))?;
                    failures.push(failure);
                }
            }
        }
        anyhow::ensure!(
            failures.is_empty(),
            "Silent startup failed: {}",
            failures.join("; ")
        );
        log_startup(&context.home, "Silent startup completed")?;
        Ok(())
    }
    .await;
    if let Err(error) = &result {
        log_startup(&context.home, &format!("Silent startup error: {error:#}"))?;
    }
    result
}

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
            && !ui.tasks.running()
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
