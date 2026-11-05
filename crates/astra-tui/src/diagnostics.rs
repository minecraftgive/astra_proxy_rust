use crate::{
    i18n::t,
    protocol::{self, Action},
    tr,
};
use astra_model::diagnostics::Event;
use astra_model::diagnostics::Report;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
    Frame,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    time::Instant,
};
use tokio::{sync::mpsc, task::JoinHandle};

enum Update {
    Targets(Vec<String>),
    Event(Event),
}
pub struct View {
    pub name: String,
    socket: PathBuf,
    task: Option<JoinHandle<()>>,
    receiver: Option<mpsc::Receiver<anyhow::Result<Update>>>,
    targets: Vec<String>,
    checked: BTreeSet<String>,
    cursor: usize,
    report: Option<Report>,
    error: Option<String>,
    loading: bool,
    submitted: BTreeSet<String>,
    active: BTreeSet<String>,
    tcp: BTreeMap<String, u64>,
    started: Option<Instant>,
    finished: Option<Instant>,
    tools: Option<crate::nettools::View>,
    /// Opened directly as network tools (Overview / palette): leaving tools leaves the page.
    tools_only: bool,
    closed: bool,
}
impl Drop for View {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}
impl View {
    pub fn start(socket: &Path, name: String) -> Self {
        let mut view = Self {
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
    fn request(&mut self, list: bool) {
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
        self.task = Some(tokio::spawn(async move {
            let result = async {
                if list {
                    let reply = protocol::call(&socket, action).await?;
                    sender
                        .send(Ok(Update::Targets(serde_json::from_str(&reply.text)?)))
                        .await?;
                    crate::ui::wake();
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
                        crate::ui::wake();
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
                crate::ui::wake();
            }
        }));
        self.receiver = Some(receiver);
    }
    /// Show network tools right away; outbound choices appear once the catalog loads.
    pub fn open_tools(&mut self) {
        self.tools_only = true;
        self.open_tools_page();
    }
    fn open_tools_page(&mut self) {
        self.tools = Some(crate::nettools::View::new(
            &self.socket,
            self.name.clone(),
            &self.targets,
        ));
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
    fn apply(&mut self, event: Event) {
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
    pub fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        if let Some(tools) = &self.tools {
            frame.render_widget(ratatui::widgets::Clear, area);
            tools.render(frame, area);
            return;
        }
        let parts = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(1),
                Constraint::Length(3),
                Constraint::Length(2),
            ])
            .split(area);
        frame.render_widget(
            Paragraph::new(tr!("Connectivity / Exit IP · {}", clean(&self.name))).block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .title(t("Node tests")),
            ),
            parts[0],
        );
        let visible = parts[1].height.saturating_sub(1).max(1) as usize;
        let offset = self.cursor.saturating_sub(visible.saturating_sub(1));
        let rows = self
            .targets
            .iter()
            .enumerate()
            .skip(offset)
            .take(visible)
            .map(|(index, tag)| {
                let probe = self
                    .report
                    .as_ref()
                    .and_then(|r| r.probes.iter().find(|p| &p.outbound == tag));
                let name = format!(
                    "{} {} {}",
                    if index == self.cursor { ">" } else { " " },
                    if self.checked.contains(tag) {
                        "[x]"
                    } else {
                        "[ ]"
                    },
                    clean(tag)
                );
                let latency = probe
                    .and_then(|p| p.connect_ms)
                    .or_else(|| self.tcp.get(tag).copied())
                    .map(|ms| format!("{ms} ms"))
                    .unwrap_or_else(|| "—".into());
                let state = if let Some(p) = probe {
                    t(if p.connect_ms.is_some() {
                        "Connected"
                    } else {
                        "Connection failed"
                    })
                } else if self.receiver.is_some() && !self.loading && self.submitted.contains(tag) {
                    if self.tcp.contains_key(tag) {
                        t("Querying exit IP…")
                    } else if self.active.contains(tag) {
                        t("Testing…")
                    } else {
                        t("Queued")
                    }
                } else {
                    "—"
                };
                let ip = probe
                    .and_then(|p| p.ip)
                    .map(|ip| ip.to_string())
                    .unwrap_or_else(|| "—".into());
                Row::new([
                    Cell::from(name),
                    Cell::from(latency),
                    Cell::from(state),
                    Cell::from(ip),
                ])
                .height(1)
                .style(if index == self.cursor {
                    crate::theme::selected()
                } else {
                    Style::default()
                })
            });
        let table = Table::new(
            rows,
            [
                Constraint::Percentage(42),
                Constraint::Length(12),
                Constraint::Length(16),
                Constraint::Min(10),
            ],
        )
        .header(
            Row::new([t("Node"), t("TCP latency"), t("Connectivity"), t("Exit IP")])
                .style(Style::default().fg(Color::Yellow)),
        );
        frame.render_widget(table, parts[1]);
        let completed = self.report.as_ref().map_or(0, |r| r.probes.len());
        let progress = self.started.map(|start| {
            tr!(
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
        });
        let status = if let Some(error) = &self.error {
            format!("{}\n{}", progress.as_deref().unwrap_or(""), clean(error))
        } else if self.loading {
            t("Loading nodes…").into()
        } else {
            let note = self
                .report
                .as_ref()
                .map(|r| {
                    t(if r.running_config {
                        "Using running configuration"
                    } else {
                        "Using saved configuration · instance stopped"
                    })
                })
                .unwrap_or(t("Select nodes, then press Enter to test"));
            let error = self
                .targets
                .get(self.cursor)
                .and_then(|tag| {
                    self.report
                        .as_ref()?
                        .probes
                        .iter()
                        .find(|p| &p.outbound == tag)
                })
                .and_then(|p| p.error.as_deref())
                .map(clean)
                .unwrap_or_default();
            format!(
                "{}\n{error}",
                progress.unwrap_or_else(|| format!(
                    "{note}  ({}/{})",
                    self.checked.len(),
                    self.targets.len()
                ))
            )
        };
        frame.render_widget(
            Paragraph::new(status).block(Block::default().borders(Borders::TOP)),
            parts[2],
        );
        frame.render_widget(
            Paragraph::new(t(
                "↑↓ select · Space toggle · a all/none · c clear\nEnter/t test selected · x stop · n network tools · Esc back",
            )),
            parts[3],
        );
    }
}
fn clean(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

#[cfg(test)]
#[path = "test/diagnostics_test.rs"]
mod tests;
