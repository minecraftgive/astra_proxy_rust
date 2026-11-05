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

fn candidates(rows: &[Row], details: &HashMap<String, InstanceDetails>) -> VecDeque<String> {
    // A proxy starts its rule dependencies itself. Explicitly starting a bound
    // Rule also expands its proxies, which could retry a failed proxy later.
    let mut roots: Vec<_> = rows
        .iter()
        .filter(|row| stopped(row))
        .filter(|row| {
            row.instance.kind != Kind::Rule
                || !details.get(&row.instance.name).is_some_and(|details| {
                    details.bindings.iter().any(|name| {
                        rows.iter()
                            .any(|bound| bound.instance.name == *name && bound.pid.is_none())
                    })
                })
        })
        .collect();
    roots.sort_by_key(|row| row.instance.kind == Kind::Rule);
    roots
        .into_iter()
        .map(|row| row.instance.name.clone())
        .collect()
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
        let (preferences, error) =
            settings::load(&context.home.join(settings::FILE), &Session::default());
        if let Some(error) = error {
            anyhow::bail!("Silent startup cannot load preferences: {error}");
        }
        if !preferences.auto_start_instances {
            log_startup(
                &context.home,
                "Automatic instance startup is disabled; manager is ready",
            )?;
            return Ok(());
        }
        let initial = call(&socket, Action::List)
            .await
            .context("Read instances for silent startup")?;
        let details =
            serde_json::from_str(&initial.text).context("Read startup instance details")?;
        let names = candidates(&initial.rows, &details);
        let mut failures = Vec::new();
        for name in names {
            // Starts can also start bound instances; always recheck current state.
            let current = call(&socket, Action::List)
                .await
                .context("Refresh instances for silent startup")?;
            if !current
                .rows
                .iter()
                .any(|row| row.instance.name == name && stopped(row))
            {
                log_startup(
                    &context.home,
                    &format!("Skipped {name}: no longer stopped or registered"),
                )?;
                continue;
            }
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
    pub(super) fn new(enabled: bool, launch: &crate::windows::Launch) -> Self {
        Self {
            awaiting_snapshot: enabled && launch.window_id.is_none(),
            pending: VecDeque::new(),
            refresh: None,
        }
    }

    fn prepare(&mut self, ui: &Ui, ready: bool) -> bool {
        // Turning this off also cancels starts that have not been dispatched.
        // Turning it on applies to the next primary window, not an existing one.
        if !ui.preferences.auto_start_instances {
            self.awaiting_snapshot = false;
            self.pending.clear();
            if let Some(refresh) = self.refresh.take() {
                refresh.abort();
            }
            return false;
        }
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

    fn next_action(&mut self, rows: &[Row]) -> Option<Action> {
        while let Some(name) = self.pending.pop_front() {
            if rows
                .iter()
                .any(|row| row.instance.name == name && stopped(row))
            {
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
        {
            Ok(reply) => self.next_action(&reply.rows),
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
