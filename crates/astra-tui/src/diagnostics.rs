use crate::{
    i18n::t,
    protocol::{self, Action},
    tr,
};
use astra_ipc::diagnostics::{Event, Report};
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
                } else {
                    let mut stream = astra_ipc::platform::LocalStream::connect(&socket).await?;
                    astra_ipc::send_management(
                        &mut stream,
                        &protocol::Request {
                            version: astra_ipc::API_VERSION,
                            action,
                        },
                    )
                    .await?;
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
            }
        }));
        self.receiver = Some(receiver);
    }
    pub fn poll(&mut self) {
        let Some(mut receiver) = self.receiver.take() else {
            return;
        };
        loop {
            match receiver.try_recv() {
                Ok(Ok(Update::Targets(targets))) => {
                    self.checked = targets.iter().cloned().collect();
                    self.targets = targets;
                    self.cursor = 0;
                    self.loading = false;
                    self.task = None;
                    return;
                }
                Ok(Ok(Update::Event(Event::Done))) => {
                    self.finished = Some(Instant::now());
                    self.active.clear();
                    self.task = None;
                    return;
                }
                Ok(Ok(Update::Event(event))) => self.apply(event),
                Ok(Err(error)) => {
                    self.finished = Some(Instant::now());
                    self.error = Some(format!("{error:#}"));
                    self.active.clear();
                    self.loading = false;
                    self.task = None;
                    return;
                }
                Err(mpsc::error::TryRecvError::Empty) => {
                    self.receiver = Some(receiver);
                    return;
                }
                Err(_) => {
                    self.finished = Some(Instant::now());
                    self.error = Some(t("Diagnostics interrupted").into());
                    self.active.clear();
                    self.loading = false;
                    self.task = None;
                    return;
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
        match key.code {
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
                    Style::default().fg(Color::Cyan)
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
                "↑↓ select · Space toggle · a all/none · c clear\nEnter/t test selected · x stop · Esc back",
            )),
            parts[3],
        );
    }
}
fn clean(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_ipc::diagnostics::Probe;
    use ratatui::{backend::TestBackend, Terminal};
    #[tokio::test]
    async fn updates_latency_and_results_without_waiting_for_done_and_cancel_retains_results() {
        let mut view = View::start(Path::new("unused-stream.sock"), "proxy".into());
        view.task.take().unwrap().abort();
        let (sender, receiver) = mpsc::channel(16);
        view.receiver = Some(receiver);
        view.loading = false;
        view.targets = vec!["slow".into(), "fast".into()];
        view.started = Some(Instant::now());
        for event in [
            Event::Started {
                targets: view.targets.clone(),
                url: "test".into(),
                running_config: true,
            },
            Event::Running {
                outbound: "slow".into(),
            },
            Event::Running {
                outbound: "fast".into(),
            },
            Event::Connected {
                outbound: "slow".into(),
                connect_ms: 15,
            },
            Event::Finished {
                probe: Probe {
                    outbound: "fast".into(),
                    direct: false,
                    connect_ms: Some(2),
                    elapsed_ms: 3,
                    ip: Some("192.0.2.1".parse().unwrap()),
                    error: None,
                },
            },
        ] {
            sender.send(Ok(Update::Event(event))).await.unwrap();
        }
        view.poll();
        assert!(
            view.receiver.is_some(),
            "updates must remain visible before completion"
        );
        assert_eq!(view.tcp["slow"], 15);
        assert_eq!(view.report.as_ref().unwrap().probes.len(), 1);
        let mut terminal = Terminal::new(TestBackend::new(120, 20)).unwrap();
        terminal
            .draw(|frame| view.render(frame, frame.area()))
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            screen.contains("15 ms") && screen.contains("192.0.2.1") && screen.contains("1/2"),
            "{screen}"
        );
        view.key(KeyEvent::from(KeyCode::Char('x')));
        assert!(view.receiver.is_none() && view.error.is_some());
        assert_eq!(view.report.as_ref().unwrap().probes.len(), 1);
    }
    #[tokio::test]
    async fn selection_and_full_page_keep_long_names_on_one_row() {
        let mut view = View::start(Path::new("unused-diagnostics.sock"), "proxy".into());
        view.task.take().unwrap().abort();
        view.receiver = None;
        view.loading = false;
        let long = format!("node-{}", "龘".repeat(120));
        view.targets = vec!["DIRECT".into(), long.clone(), "third".into()];
        view.checked = view.targets.iter().cloned().collect();
        view.key(KeyEvent::from(KeyCode::Char('a')));
        assert!(view.checked.is_empty());
        view.key(KeyEvent::from(KeyCode::Down));
        view.key(KeyEvent::from(KeyCode::Char(' ')));
        assert_eq!(
            view.checked.iter().cloned().collect::<Vec<_>>(),
            vec![long.clone()]
        );
        view.report = Some(Report {
            url: "https://api.ip.sb/ip".into(),
            running_config: true,
            probes: vec![Probe {
                outbound: long,
                direct: false,
                connect_ms: Some(12),
                elapsed_ms: 100,
                ip: Some("192.0.2.1".parse().unwrap()),
                error: None,
            }],
        });
        for (width, height) in [(120, 30), (80, 18), (20, 8), (5, 3)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|f| view.render(f, f.area())).unwrap();
            let lines: Vec<String> = terminal
                .backend()
                .buffer()
                .content
                .chunks(width as usize)
                .map(|row| row.iter().map(|c| c.symbol()).collect())
                .collect();
            if width >= 80 {
                assert_eq!(lines.iter().filter(|line| line.contains('龘')).count(), 1);
                assert!(lines.iter().any(|l| l.contains("12 ms")));
                assert!(lines.iter().any(|l| l.contains("third")));
            }
        }
        view.key(KeyEvent::from(KeyCode::Char('c')));
        view.key(KeyEvent::from(KeyCode::Enter));
        assert!(view.error.is_some());
        assert!(view.receiver.is_none());
    }
}
