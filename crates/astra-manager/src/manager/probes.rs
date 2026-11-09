//! Header exit probes through `--probe-serve` cores. A worker keeps the profile
//! loaded and idles on stdin between requests, so periodic rounds do not start
//! a core each time; it ends once no request arrives within its keep-alive.
//! Probes use their own slots and never hold the node test slot.
use super::DiagnosticPlan;
use anyhow::{Context, Result};
use astra_model::diagnostics::{Probe, ProbeRequest, ProbeServe};
use std::{
    collections::HashMap,
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout},
    sync::Semaphore,
};

/// A probe round waits at most this long for its core.
const ROUND_LIMIT: Duration = Duration::from_secs(15);
/// Longest idle period a client may ask a worker to survive.
pub(crate) const MAX_KEEP_ALIVE: Duration = Duration::from_secs(2 * 3600);

/// Which loaded configuration a worker answers for.
#[derive(Clone, PartialEq, Eq)]
struct Key {
    executable: PathBuf,
    config_path: PathBuf,
    text: String,
}

struct Worker {
    key: Key,
    _child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}
impl Worker {
    async fn spawn(plan: &DiagnosticPlan, key: Key) -> Result<Self> {
        let mut child = super::core_process(plan.executable.clone(), &plan.runtime_dir)
            .arg("--probe-serve")
            .arg(&plan.config_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let input = child.stdin.take().context("Missing probe input")?;
        let output = BufReader::new(child.stdout.take().context("Missing probe output")?);
        Ok(Self {
            key,
            _child: child,
            input,
            output,
        })
    }
    async fn initialize(&mut self) -> Result<()> {
        let mut first = serde_json::to_vec(&ProbeServe {
            config: self.key.text.clone(),
        })?;
        first.push(b'\n');
        self.input.write_all(&first).await?;
        Ok(())
    }
    async fn close(mut self) -> Result<()> {
        super::network::terminate(&mut self._child).await
    }
    async fn ask(&mut self, request: &ProbeRequest) -> Result<Probe> {
        let mut line = serde_json::to_vec(request)?;
        line.push(b'\n');
        self.input.write_all(&line).await?;
        self.input.flush().await?;
        let mut reply = String::new();
        let read = (&mut self.output)
            .take(64 * 1024)
            .read_line(&mut reply)
            .await?;
        anyhow::ensure!(
            read > 0 && reply.ends_with('\n'),
            "Update proxy core to use exit probes"
        );
        serde_json::from_str(&reply).context("Update proxy core to use exit probes")
    }
}

#[derive(Default)]
struct Slot {
    worker: Option<Worker>,
    used: Option<Instant>,
}

#[derive(Clone)]
pub(crate) struct Probes {
    slots: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<Slot>>>>>,
    running: Arc<Semaphore>,
}
impl Default for Probes {
    fn default() -> Self {
        Self {
            slots: Default::default(),
            running: Arc::new(Semaphore::new(4)),
        }
    }
}
impl Probes {
    fn slot(&self, name: &str) -> Arc<tokio::sync::Mutex<Slot>> {
        self.slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(name.to_owned())
            .or_default()
            .clone()
    }
    /// Run one probe. With `keep_alive` the worker stays loaded for the next
    /// request; without it the core exits right after answering.
    pub(crate) async fn probe(
        &self,
        name: &str,
        mut plan: DiagnosticPlan,
        request: ProbeRequest,
        keep_alive: Option<Duration>,
    ) -> Result<Probe> {
        let _permit = self
            .running
            .try_acquire()
            .context("Exit probes are busy; retry shortly")?;
        plan.refresh().await?;
        let key = Key {
            executable: plan.executable.clone(),
            config_path: plan.config_path.clone(),
            text: plan.text.clone(),
        };
        let slot = self.slot(name);
        let mut guard = slot.lock().await;
        // A changed configuration or core build needs a freshly loaded worker.
        if guard.worker.as_ref().is_some_and(|w| w.key != key) {
            if let Some(worker) = guard.worker.take() {
                worker.close().await?;
            }
        }
        let result = tokio::time::timeout(ROUND_LIMIT, async {
            if guard.worker.is_none() {
                guard.worker = Some(Worker::spawn(&plan, key).await?);
                // Store ownership before the first await that writes to the
                // process. A timeout still leaves a child available to reap.
                guard.worker.as_mut().expect("worker").initialize().await?;
            }
            guard.worker.as_mut().expect("worker").ask(&request).await
        })
        .await
        .unwrap_or_else(|_| Err(anyhow::anyhow!("Exit probe timed out")));
        match keep_alive {
            Some(idle) if result.is_ok() => {
                let used = Instant::now();
                guard.used = Some(used);
                let slot = slot.clone();
                let idle = idle.min(MAX_KEEP_ALIVE);
                tokio::spawn(async move {
                    tokio::time::sleep(idle).await;
                    let mut slot = slot.lock().await;
                    // A later request extended the lease.
                    if slot.used == Some(used) {
                        if let Some(worker) = slot.worker.take() {
                            let _ = worker.close().await;
                        }
                    }
                });
            }
            _ => {
                if let Some(worker) = guard.worker.take() {
                    worker.close().await?;
                }
            }
        }
        result
    }
}

#[cfg(all(test, unix))]
#[path = "../test/manager_probes_test.rs"]
mod tests;
