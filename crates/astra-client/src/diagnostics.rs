use crate::input::{KeyCode, KeyEvent};
use crate::{
    i18n::t,
    protocol::{self, Action},
};
use astra_model::diagnostics::Event;
use astra_model::diagnostics::Report;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    time::Instant,
};
use tokio::{sync::mpsc, task::JoinHandle};

pub enum Update {
    Targets(Vec<String>),
    Event(Event),
}
pub struct View {
    pub waker: crate::waker::Waker,
    pub name: String,
    pub socket: PathBuf,
    pub task: Option<JoinHandle<()>>,
    pub receiver: Option<mpsc::Receiver<anyhow::Result<Update>>>,
    pub targets: Vec<String>,
    pub checked: BTreeSet<String>,
    pub cursor: usize,
    pub report: Option<Report>,
    pub error: Option<String>,
    pub loading: bool,
    pub submitted: BTreeSet<String>,
    pub active: BTreeSet<String>,
    pub tcp: BTreeMap<String, u64>,
    pub started: Option<Instant>,
    pub finished: Option<Instant>,
    pub tools: Option<crate::nettools::View>,
    /// Opened directly as network tools (Overview / palette): leaving tools leaves the page.
    pub tools_only: bool,
    pub closed: bool,
}
impl Drop for View {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}
impl View {
    pub fn with_waker(mut self, waker: crate::waker::Waker) -> Self {
        self.waker = waker;
        self
    }
    pub fn start(socket: &Path, name: String) -> Self {
        Self::start_with_waker(socket, name, Default::default())
    }
    pub fn start_with_waker(socket: &Path, name: String, waker: crate::waker::Waker) -> Self {
        let mut view = Self {
            waker,
            name,
            socket: socket.to_owned(),
            task: None,
            receiver: None,
            targets: vec![],
            checked: BTreeSet::new(),
            cursor: 0,
            report: None,
            error: None,
            loading: true,
            submitted: BTreeSet::new(),
            active: BTreeSet::new(),
            tcp: BTreeMap::new(),
            started: None,
            finished: None,
            tools: None,
            tools_only: false,
            closed: false,
        };
        view.request(true);
        view
    }
    pub fn request(&mut self, list: bool) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        let socket = self.socket.clone();
        let action = if list {
            Action::DiagnosticTargets(self.name.clone())
        } else {
            Action::DiagnosticsStream {
                name: self.name.clone(),
                targets: self
                    .targets
                    .iter()
                    .filter(|t| self.checked.contains(*t))
                    .cloned()
                    .collect(),
            }
        };
        let (sender, receiver) = mpsc::channel(64);
        if !list {
            self.submitted = self.checked.clone();
            self.active.clear();
            self.tcp.clear();
            self.started = Some(Instant::now());
            self.finished = None;
        }
        self.error = None;
        self.loading = list;
        let waker = self.waker.clone();
        self.task = Some(tokio::spawn(async move {
            let result = async {
                if list {
                    let reply = protocol::call(&socket, action).await?;
                    sender
                        .send(Ok(Update::Targets(serde_json::from_str(&reply.text)?)))
                        .await?;
                    waker.wake();
                } else {
                    let mut stream = crate::protocol::subscribe(&socket, action).await?;
                    loop {
                        let reply: protocol::Reply = tokio::time::timeout(
                            std::time::Duration::from_secs(45),
                            astra_ipc::receive_management(&mut stream),
                        )
                        .await??;
                        if let Some(error) = reply.error {
                            anyhow::bail!("{error}");
                        }
                        let event: Event = serde_json::from_str(&reply.text)?;
                        let done = matches!(event, Event::Done);
                        sender.send(Ok(Update::Event(event))).await?;
                        waker.wake();
                        if done {
                            break;
                        }
                    }
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;
            if let Err(error) = result {
                let _ = sender.send(Err(error)).await;
                waker.wake();
            }
        }));
        self.receiver = Some(receiver);
    }
    /// Show network tools right away; outbound choices appear once the catalog loads.
    pub fn open_tools(&mut self) {
        self.tools_only = true;
        self.open_tools_page();
    }
    pub fn open_tools_page(&mut self) {
        self.tools = Some(
            crate::nettools::View::new(&self.socket, self.name.clone(), &self.targets)
                .with_waker(self.waker.clone()),
        );
    }
    /// True once the page asked to be closed (Esc from directly opened tools).
    pub fn closed(&self) -> bool {
        self.closed
    }
    /// Esc belongs to the network tools sub-page while it is open.
    pub fn handles_escape(&self) -> bool {
        self.tools.is_some()
    }
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        if let Some(tools) = &mut self.tools {
            changed |= tools.poll();
        }
        let Some(mut receiver) = self.receiver.take() else {
            return changed;
        };
        loop {
            let event = receiver.try_recv();
            changed |= !matches!(&event, Err(mpsc::error::TryRecvError::Empty));
            match event {
                Ok(Ok(Update::Targets(targets))) => {
                    if let Some(tools) = &mut self.tools {
                        tools.set_outbounds(&targets);
                    }
                    self.checked = targets.iter().cloned().collect();
                    self.targets = targets;
                    self.cursor = 0;
                    self.loading = false;
                    self.task = None;
                    return changed;
                }
                Ok(Ok(Update::Event(Event::Done))) => {
                    self.finished = Some(Instant::now());
                    self.active.clear();
                    self.task = None;
                    return changed;
                }
                Ok(Ok(Update::Event(event))) => self.apply(event),
                Ok(Err(error)) => {
                    self.finished = Some(Instant::now());
                    self.error = Some(format!("{error:#}"));
                    self.active.clear();
                    self.loading = false;
                    self.task = None;
                    return changed;
                }
                Err(mpsc::error::TryRecvError::Empty) => {
                    self.receiver = Some(receiver);
                    return changed;
                }
                Err(_) => {
                    self.finished = Some(Instant::now());
                    self.error = Some(t("Diagnostics interrupted").into());
                    self.active.clear();
                    self.loading = false;
                    self.task = None;
                    return changed;
                }
            }
        }
    }
    pub fn apply(&mut self, event: Event) {
        match event {
            Event::Started {
                targets,
                url,
                running_config,
            } => {
                self.submitted = targets.into_iter().collect();
                self.report = Some(Report {
                    url,
                    running_config,
                    probes: vec![],
                });
            }
            Event::Running { outbound } => {
                self.active.insert(outbound);
            }
            Event::Connected {
                outbound,
                connect_ms,
            } => {
                self.tcp.insert(outbound, connect_ms);
            }
            Event::Finished { probe } => {
                self.active.remove(&probe.outbound);
                if let Some(ms) = probe.connect_ms {
                    self.tcp.insert(probe.outbound.clone(), ms);
                }
                if let Some(report) = &mut self.report {
                    report.probes.retain(|p| p.outbound != probe.outbound);
                    report.probes.push(probe);
                }
            }
            Event::Done => {}
        }
    }
    pub fn toggle_target(&mut self, index: usize) {
        if let Some(tag) = self.targets.get(index) {
            self.cursor = index;
            if !self.checked.remove(tag) {
                self.checked.insert(tag.clone());
            }
        }
    }
    pub fn select_all(&mut self, selected: bool) {
        self.checked = if selected {
            self.targets.iter().cloned().collect()
        } else {
            Default::default()
        };
    }
    pub fn run_selected(&mut self) {
        if self.receiver.is_some() {
            return;
        }
        if self.targets.is_empty() {
            self.request(true);
        } else if self.checked.is_empty() {
            self.error = Some(t("Select at least one node").into());
        } else {
            self.report = None;
            self.request(false);
        }
    }
    pub fn stop(&mut self) {
        if self.receiver.is_none() || self.loading {
            return;
        }
        self.finished = Some(Instant::now());
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.receiver = None;
        self.active.clear();
        self.error = Some(t("Diagnostics interrupted").into());
    }
    pub fn rows(&self) -> Vec<TargetRow> {
        self.targets
            .iter()
            .map(|tag| {
                let probe = self
                    .report
                    .as_ref()
                    .and_then(|r| r.probes.iter().find(|p| &p.outbound == tag));
                let (state, success) = if let Some(probe) = probe {
                    (
                        t(if probe.connect_ms.is_some() {
                            "Connected"
                        } else {
                            "Connection failed"
                        }),
                        Some(probe.connect_ms.is_some()),
                    )
                } else if self.receiver.is_some() && !self.loading && self.submitted.contains(tag) {
                    (
                        t(if self.tcp.contains_key(tag) {
                            "Querying exit IP…"
                        } else if self.active.contains(tag) {
                            "Testing…"
                        } else {
                            "Queued"
                        }),
                        None,
                    )
                } else {
                    ("—", None)
                };
                TargetRow {
                    name: clean(tag),
                    checked: self.checked.contains(tag),
                    state: state.into(),
                    success,
                    latency_ms: probe
                        .and_then(|p| p.connect_ms)
                        .or_else(|| self.tcp.get(tag).copied()),
                    ip: probe.and_then(|p| p.ip).map(|ip| ip.to_string()),
                    error: probe.and_then(|p| p.error.clone()),
                }
            })
            .collect()
    }
    pub fn progress(&self) -> String {
        if self.loading {
            return t("Loading nodes…").into();
        }
        let completed = self.report.as_ref().map_or(0, |r| r.probes.len());
        self.started
            .map(|start| {
                crate::tr!(
                    "Progress {}/{} · Running {} · Queued {} · {}s",
                    completed,
                    self.submitted.len(),
                    self.active.len(),
                    self.submitted
                        .len()
                        .saturating_sub(completed + self.active.len()),
                    self.finished
                        .unwrap_or_else(Instant::now)
                        .duration_since(start)
                        .as_secs()
                )
            })
            .unwrap_or_else(|| t("Select nodes, then press Enter to test").into())
    }
    pub fn key(&mut self, key: KeyEvent) {
        if let Some(tools) = &mut self.tools {
            if !tools.key(key) {
                self.tools = None;
                self.closed = self.tools_only;
            }
            return;
        }
        match key.code {
            KeyCode::Char('n') => self.open_tools_page(),
            KeyCode::Up | KeyCode::Char('k') => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.cursor = (self.cursor + 1).min(self.targets.len().saturating_sub(1))
            }
            KeyCode::PageUp => self.cursor = self.cursor.saturating_sub(10),
            KeyCode::PageDown => {
                self.cursor = (self.cursor + 10).min(self.targets.len().saturating_sub(1))
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.targets.len().saturating_sub(1),
            KeyCode::Char(' ') => {
                if let Some(tag) = self.targets.get(self.cursor) {
                    if !self.checked.remove(tag) {
                        self.checked.insert(tag.clone());
                    }
                }
            }
            KeyCode::Char('a') => {
                if self.checked.len() == self.targets.len() {
                    self.checked.clear();
                } else {
                    self.checked = self.targets.iter().cloned().collect();
                }
            }
            KeyCode::Char('c') => self.checked.clear(),
            KeyCode::Char('x') if self.receiver.is_some() && !self.loading => {
                self.finished = Some(Instant::now());
                if let Some(task) = self.task.take() {
                    task.abort();
                }
                self.receiver = None;
                self.active.clear();
                self.error = Some(t("Diagnostics interrupted").into());
            }
            KeyCode::Enter | KeyCode::Char('t') if self.receiver.is_none() => {
                if self.targets.is_empty() {
                    self.request(true);
                } else if self.checked.is_empty() {
                    self.error = Some(t("Select at least one node").into());
                } else {
                    self.report = None;
                    self.request(false);
                }
            }
            _ => {}
        }
    }
}
pub fn clean(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

#[derive(Clone)]
pub struct TargetRow {
    pub name: String,
    pub checked: bool,
    pub state: String,
    pub success: Option<bool>,
    pub latency_ms: Option<u64>,
    pub ip: Option<String>,
    pub error: Option<String>,
}
