//! Rendering-free memory composition: the interface process itself plus the
//! running proxy, rule and server cores, each read as resident memory.
//!
//! Nothing here samples by itself. A frontend registers a watcher for every
//! debug view it shows and reports whether that view is on screen; rounds run
//! only while at least one watcher is visible and stop (cancelling the round in
//! flight) as soon as none is.
use crate::{
    i18n::t,
    protocol::{Action, Kind, Row},
};
use anyhow::{Context, Result};
use astra_management::v2::MonitorSnapshot;
use futures_util::FutureExt;
use std::{
    collections::{BTreeMap, VecDeque},
    path::Path,
    time::{Duration, Instant},
};
use tokio::task::JoinHandle;

/// One composition round every this often while visible.
pub const INTERVAL: Duration = Duration::from_secs(2);
/// Rounds kept for the history chart (three minutes at `INTERVAL`).
pub const LIMIT: usize = 90;
/// A core that does not answer in time is shown with an error, not waited for.
const ROUND_LIMIT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Module {
    Interface,
    Proxy,
    Rule,
    Server,
}
impl Module {
    pub const ALL: [Module; 4] = [Self::Interface, Self::Proxy, Self::Rule, Self::Server];
    pub fn label(self) -> &'static str {
        t(match self {
            Self::Interface => "Interface",
            Self::Proxy => "Proxy core",
            Self::Rule => "Rule core",
            Self::Server => "Server core",
        })
    }
    fn of(kind: Kind) -> Self {
        match kind {
            Kind::Proxy => Self::Proxy,
            Kind::Rule => Self::Rule,
            Kind::Server => Self::Server,
        }
    }
    fn index(self) -> usize {
        self as usize
    }
}

/// One process in a round.
#[derive(Clone, Debug, PartialEq)]
pub struct Part {
    pub module: Module,
    pub name: String,
    pub pid: Option<u32>,
    /// Resident memory; `None` when the process could not be read.
    pub bytes: Option<u64>,
    pub error: Option<String>,
}

/// One module's total and the processes it consists of.
#[derive(Clone, Debug, PartialEq)]
pub struct Summary<'a> {
    pub module: Module,
    pub bytes: Option<u64>,
    /// Fraction of the round's readable total, 0.0..=1.0.
    pub share: f64,
    pub parts: Vec<&'a Part>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Composition {
    pub parts: Vec<Part>,
}
impl Composition {
    pub fn total(&self) -> u64 {
        self.parts.iter().filter_map(|part| part.bytes).sum()
    }
    /// `None` when the module has no readable process in this round.
    pub fn module_bytes(&self, module: Module) -> Option<u64> {
        self.parts
            .iter()
            .filter(|part| part.module == module)
            .filter_map(|part| part.bytes)
            .reduce(|a, b| a + b)
    }
    pub fn share(&self, bytes: u64) -> f64 {
        match self.total() {
            0 => 0.0,
            total => bytes as f64 / total as f64,
        }
    }
    /// Modules with at least one process this round, interface first. The
    /// interface, proxy and rule rows always appear so an absent core reads as
    /// "not running" rather than vanishing.
    pub fn summaries(&self) -> Vec<Summary<'_>> {
        Module::ALL
            .into_iter()
            .filter_map(|module| {
                let parts: Vec<_> = self
                    .parts
                    .iter()
                    .filter(|part| part.module == module)
                    .collect();
                if parts.is_empty() && module == Module::Server {
                    return None;
                }
                let bytes = self.module_bytes(module);
                Some(Summary {
                    module,
                    bytes,
                    share: bytes.map_or(0.0, |bytes| self.share(bytes)),
                    parts,
                })
            })
            .collect()
    }
}

/// Running cores the manager can sample, by module then name.
pub fn targets(rows: &[Row]) -> Vec<(Module, String, u32)> {
    let mut targets: Vec<_> = rows
        .iter()
        .filter_map(|row| {
            Some((
                Module::of(row.instance.kind),
                row.instance.name.clone(),
                row.pid?,
            ))
        })
        .collect();
    targets.sort();
    targets
}

pub struct View {
    pub waker: crate::waker::Waker,
    /// Registered debug views and whether each is currently on screen.
    watchers: BTreeMap<u64, bool>,
    sampler: astra_platform::monitor::Sampler,
    pid: u32,
    name: String,
    pub latest: Option<Composition>,
    /// Per-module bytes for each round, indexed like `Module::ALL`.
    pub history: VecDeque<[Option<f64>; 4]>,
    pending: Option<JoinHandle<Vec<Part>>>,
    next: Instant,
    pub paused: bool,
}

impl Drop for View {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl View {
    pub fn new(waker: crate::waker::Waker) -> Self {
        let name = std::env::current_exe()
            .ok()
            .as_deref()
            .and_then(Path::file_stem)
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "astra".into());
        Self {
            waker,
            watchers: BTreeMap::new(),
            sampler: Default::default(),
            pid: std::process::id(),
            name,
            latest: None,
            history: VecDeque::new(),
            pending: None,
            next: Instant::now(),
            paused: false,
        }
    }
    /// Sampling runs only while this is true.
    pub fn active(&self) -> bool {
        !self.paused && self.watchers.values().any(|visible| *visible)
    }
    /// Register or update a debug view; a newly visible one samples at once.
    pub fn watch(&mut self, id: u64, visible: bool) {
        let was_active = self.active();
        self.watchers.insert(id, visible);
        self.transition(was_active);
    }
    /// Forget a debug view; true when no view remains and the state can go.
    pub fn unwatch(&mut self, id: u64) -> bool {
        let was_active = self.active();
        self.watchers.remove(&id);
        self.transition(was_active);
        self.watchers.is_empty()
    }
    pub fn set_paused(&mut self, paused: bool) {
        let was_active = self.active();
        self.paused = paused;
        self.transition(was_active);
    }
    pub fn clear(&mut self) {
        self.cancel();
        self.latest = None;
        self.history.clear();
        self.next = Instant::now();
    }
    fn transition(&mut self, was_active: bool) {
        match (was_active, self.active()) {
            (true, false) => self.cancel(),
            (false, true) => self.next = Instant::now(),
            _ => {}
        }
    }
    fn cancel(&mut self) {
        if let Some(pending) = self.pending.take() {
            pending.abort();
        }
    }

    /// Collect a finished round and start the next one when due; true when the
    /// shown composition changed. Must run inside a Tokio runtime.
    pub fn poll_ready(&mut self, socket: &Path, rows: &[Row]) -> bool {
        let mut changed = false;
        if self.pending.as_ref().is_some_and(|task| task.is_finished()) {
            let cores = match self.pending.take().unwrap().now_or_never() {
                Some(Ok(parts)) => parts,
                // Aborted or panicked: show the interface alone this round.
                _ => Vec::new(),
            };
            self.accept(cores);
            self.next = Instant::now() + INTERVAL;
            changed = true;
        }
        if self.active() && self.pending.is_none() && Instant::now() >= self.next {
            let targets = if socket.as_os_str().is_empty() {
                Vec::new()
            } else {
                targets(rows)
            };
            let socket = socket.to_owned();
            let waker = self.waker.clone();
            self.pending = Some(tokio::spawn(async move {
                let parts = futures_util::future::join_all(
                    targets
                        .into_iter()
                        .map(|(module, name, pid)| sample_core(&socket, module, name, pid)),
                )
                .await;
                waker.wake();
                parts
            }));
        }
        changed
    }

    /// Add this process to the cores' readings and record the round.
    pub fn accept(&mut self, cores: Vec<Part>) {
        let interface = match self.sampler.sample(self.pid) {
            Ok(sample) => Part {
                module: Module::Interface,
                name: self.name.clone(),
                pid: Some(self.pid),
                bytes: Some(sample.memory_bytes),
                error: None,
            },
            Err(error) => Part {
                module: Module::Interface,
                name: self.name.clone(),
                pid: Some(self.pid),
                bytes: None,
                error: Some(error.to_string()),
            },
        };
        let mut parts = vec![interface];
        parts.extend(cores);
        parts.sort_by(|a, b| (a.module, &a.name).cmp(&(b.module, &b.name)));
        let composition = Composition { parts };
        let mut row = [None; 4];
        for module in Module::ALL {
            row[module.index()] = composition.module_bytes(module).map(|n| n as f64);
        }
        self.history.push_back(row);
        while self.history.len() > LIMIT {
            self.history.pop_front();
        }
        self.latest = Some(composition);
    }

    /// History of one module, gaps where it had no readable process.
    pub fn series(&self, module: Module) -> Vec<Option<f64>> {
        self.history.iter().map(|row| row[module.index()]).collect()
    }
}

async fn sample_core(socket: &Path, module: Module, name: String, pid: u32) -> Part {
    let result: Result<MonitorSnapshot> = async {
        let reply = tokio::time::timeout(
            ROUND_LIMIT,
            crate::protocol::call(socket, Action::Monitor { name: name.clone() }),
        )
        .await
        .context("Monitor request timed out")??;
        if let Some(error) = reply.error {
            anyhow::bail!(error);
        }
        serde_json::from_str(&reply.text).context("Invalid monitor response")
    }
    .await;
    match result {
        Ok(snapshot) => Part {
            module,
            name,
            pid: snapshot.pid.or(Some(pid)),
            bytes: snapshot
                .memory_bytes
                .as_deref()
                .and_then(|value| value.parse().ok()),
            error: snapshot.error,
        },
        Err(error) => Part {
            module,
            name,
            pid: Some(pid),
            bytes: None,
            error: Some(format!("{error:#}")),
        },
    }
}

#[cfg(test)]
#[path = "test/memory_test.rs"]
mod tests;
