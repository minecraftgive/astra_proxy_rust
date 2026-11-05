//! Network tools: DNS / TCP / ICMP probes through a chosen egress (system route,
//! a local adapter, or an outbound / group of the profile). Opened from node tests.
use crate::{
    i18n::t,
    protocol::{self, Action},
    tr,
};
use astra_model::nettool::Attempt;
use astra_model::nettool::Egress;
use astra_model::nettool::Event;
use astra_model::nettool::Probe;
use astra_model::nettool::Request;
use astra_model::nettool::Summary;
use astra_model::nettool::MAX_COUNT;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use std::path::{Path, PathBuf};
use tokio::{sync::mpsc, task::JoinHandle};

const RECORDS: [&str; 8] = ["A", "AAAA", "CNAME", "MX", "TXT", "NS", "HTTPS", "SOA"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Dns,
    Tcp,
    Icmp,
}
impl Kind {
    const ALL: [Kind; 3] = [Kind::Dns, Kind::Tcp, Kind::Icmp];
    fn label(self) -> &'static str {
        match self {
            Kind::Dns => "DNS query",
            Kind::Tcp => "TCP ping",
            Kind::Icmp => "ICMP ping",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Egress,
    Kind,
    Target,
    Record,
    Server,
    Count,
}

pub struct View {
    name: String,
    socket: PathBuf,
    egresses: Vec<Egress>,
    egress: usize,
    /// Type-ahead text for the egress field.
    egress_query: String,
    kind: Kind,
    target: String,
    record: String,
    server: String,
    count: u32,
    field: Field,
    task: Option<JoinHandle<()>>,
    receiver: Option<mpsc::Receiver<anyhow::Result<Event>>>,
    note: Option<String>,
    attempts: Vec<Attempt>,
    summary: Option<Summary>,
    error: Option<String>,
}
impl Drop for View {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}
impl View {
    /// `outbounds` comes from the node-test catalog (DIRECT, nodes and groups).
    pub fn new(socket: &Path, name: String, outbounds: &[String]) -> Self {
        let interfaces = crate::network_interfaces::connected().unwrap_or_default();
        let egresses = std::iter::once(Egress::System)
            .chain(interfaces.into_iter().map(Egress::Interface))
            .chain(outbounds.iter().cloned().map(Egress::Outbound))
            .collect();
        Self {
            name,
            socket: socket.to_owned(),
            egresses,
            egress: 0,
            egress_query: String::new(),
            kind: Kind::Tcp,
            target: "www.gstatic.com:443".into(),
            record: "A".into(),
            server: "1.1.1.1".into(),
            count: 4,
            field: Field::Egress,
            task: None,
            receiver: None,
            note: None,
            attempts: vec![],
            summary: None,
            error: None,
        }
    }
    /// Replace the outbound choices (the catalog loads after a direct open), keeping the selection.
    pub fn set_outbounds(&mut self, outbounds: &[String]) {
        let selected = self.egresses[self.egress].clone();
        self.egresses
            .retain(|egress| !matches!(egress, Egress::Outbound(_)));
        self.egresses
            .extend(outbounds.iter().cloned().map(Egress::Outbound));
        self.egress = self
            .egresses
            .iter()
            .position(|egress| *egress == selected)
            .unwrap_or(0);
    }
    pub fn running(&self) -> bool {
        self.receiver.is_some()
    }
    fn fields(&self) -> Vec<Field> {
        if self.kind == Kind::Dns {
            vec![
                Field::Egress,
                Field::Kind,
                Field::Target,
                Field::Record,
                Field::Server,
                Field::Count,
            ]
        } else {
            vec![Field::Egress, Field::Kind, Field::Target, Field::Count]
        }
    }
    fn set_kind(&mut self, kind: Kind) {
        if kind == self.kind {
            return;
        }
        // Keep the host the user typed; only the port belongs to TCP.
        let host = host_part(&self.target).to_owned();
        self.target = match kind {
            Kind::Tcp => format!("{host}:443"),
            Kind::Dns | Kind::Icmp => host,
        };
        if kind == Kind::Dns && self.count == 4 {
            self.count = 1;
        } else if self.kind == Kind::Dns && self.count == 1 {
            self.count = 4;
        }
        self.kind = kind;
    }
    fn request(&self) -> Result<Request, String> {
        let target = self.target.trim();
        if target.is_empty() {
            return Err(t("Enter a target").into());
        }
        let probe = match self.kind {
            Kind::Dns => Probe::Dns {
                name: target.into(),
                record: self.record.trim().into(),
                server: self.server.trim().into(),
            },
            Kind::Icmp => Probe::Icmp {
                host: target.into(),
            },
            Kind::Tcp => {
                let (host, port) = target
                    .rsplit_once(':')
                    .filter(|(host, _)| !host.is_empty())
                    .ok_or_else(|| t("TCP target needs host:port").to_owned())?;
                Probe::Tcp {
                    host: host.trim_matches(['[', ']']).into(),
                    port: port
                        .parse()
                        .ok()
                        .filter(|port| *port != 0)
                        .ok_or_else(|| t("TCP target needs host:port").to_owned())?,
                }
            }
        };
        Ok(Request {
            config: String::new(),
            egress: self.egresses[self.egress].clone(),
            probe,
            count: self.count,
            interval_ms: if self.kind == Kind::Dns { 200 } else { 1000 },
            timeout_ms: 3000,
        })
    }
    fn start(&mut self) {
        let request = match self.request() {
            Ok(request) => request,
            Err(error) => {
                self.error = Some(error);
                return;
            }
        };
        self.stop();
        self.note = None;
        self.attempts.clear();
        self.summary = None;
        self.error = None;
        let socket = self.socket.clone();
        let wait =
            std::time::Duration::from_millis(request.timeout_ms * 2 + request.interval_ms + 15_000);
        let action = Action::NetTool {
            name: self.name.clone(),
            request,
        };
        let (sender, receiver) = mpsc::channel(64);
        self.task = Some(tokio::spawn(async move {
            let result = async {
                let mut stream = crate::protocol::subscribe(&socket, action).await?;
                loop {
                    let reply: protocol::Reply =
                        tokio::time::timeout(wait, astra_ipc::receive_management(&mut stream))
                            .await??;
                    if let Some(error) = reply.error {
                        anyhow::bail!("{error}");
                    }
                    let event: Event = serde_json::from_str(&reply.text)?;
                    let done = matches!(event, Event::Done);
                    sender.send(Ok(event)).await?;
                    crate::ui::wake();
                    if done {
                        break;
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
    fn stop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        if self.receiver.take().is_some() {
            self.error = Some(t("Stopped").into());
            self.summary = Some(Summary::from_attempts(&self.attempts));
        }
    }
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        let Some(mut receiver) = self.receiver.take() else {
            return changed;
        };
        loop {
            let event = receiver.try_recv();
            changed |= !matches!(&event, Err(mpsc::error::TryRecvError::Empty));
            match event {
                Ok(Ok(Event::Done)) => {
                    self.task = None;
                    return changed;
                }
                Ok(Ok(Event::Started { note, .. })) => self.note = Some(note),
                Ok(Ok(Event::Attempt(attempt))) => self.attempts.push(attempt),
                Ok(Ok(Event::Summary(summary))) => self.summary = Some(summary),
                Ok(Err(error)) => {
                    self.error = Some(clean(&format!("{error:#}")));
                    self.task = None;
                    return changed;
                }
                Err(mpsc::error::TryRecvError::Empty) => {
                    self.receiver = Some(receiver);
                    return changed;
                }
                Err(_) => {
                    self.error = Some(t("Diagnostics interrupted").into());
                    self.task = None;
                    return changed;
                }
            }
        }
    }
    /// Returns false when the key should close the page (Esc while idle).
    pub fn key(&mut self, key: KeyEvent) -> bool {
        let fields = self.fields();
        let index = fields.iter().position(|f| *f == self.field).unwrap_or(0);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc if self.running() => self.stop(),
            KeyCode::Esc => return false,
            KeyCode::Enter if self.running() => self.stop(),
            KeyCode::Enter => self.start(),
            KeyCode::Up | KeyCode::BackTab => {
                self.field = fields[(index + fields.len() - 1) % fields.len()];
                self.egress_query.clear();
            }
            KeyCode::Down | KeyCode::Tab => {
                self.field = fields[(index + 1) % fields.len()];
                self.egress_query.clear();
            }
            KeyCode::Left | KeyCode::Right | KeyCode::PageUp | KeyCode::PageDown => {
                let step: isize = match key.code {
                    KeyCode::Left => -1,
                    KeyCode::Right => 1,
                    KeyCode::PageUp => -10,
                    _ => 10,
                };
                self.cycle(step);
            }
            KeyCode::Char('u') if ctrl => {
                if let Some(text) = self.text_mut() {
                    text.clear();
                }
                self.egress_query.clear();
            }
            KeyCode::Backspace => {
                if self.field == Field::Egress {
                    self.egress_query.pop();
                    self.find_egress();
                } else if self.field == Field::Count {
                    self.count = (self.count / 10).max(1);
                } else if let Some(text) = self.text_mut() {
                    text.pop();
                }
            }
            KeyCode::Char(c) if !ctrl => match self.field {
                Field::Egress => {
                    self.egress_query.push(c);
                    self.find_egress();
                }
                Field::Kind => {
                    if let Some(kind) = match c.to_ascii_lowercase() {
                        'd' => Some(Kind::Dns),
                        't' => Some(Kind::Tcp),
                        'i' | 'p' => Some(Kind::Icmp),
                        _ => None,
                    } {
                        self.set_kind(kind);
                    }
                }
                Field::Count => {
                    if let Some(digit) = c.to_digit(10) {
                        let next = if self.count >= 10 {
                            digit
                        } else {
                            self.count * 10 + digit
                        };
                        self.count = next.clamp(1, MAX_COUNT);
                    }
                }
                _ => {
                    if let Some(text) = self.text_mut() {
                        if text.chars().count() < 255 {
                            text.push(c);
                        }
                    }
                }
            },
            _ => {}
        }
        true
    }
    fn text_mut(&mut self) -> Option<&mut String> {
        match self.field {
            Field::Target => Some(&mut self.target),
            Field::Record => Some(&mut self.record),
            Field::Server => Some(&mut self.server),
            _ => None,
        }
    }
    fn cycle(&mut self, step: isize) {
        let wrap =
            |value: usize, len: usize| (value as isize + step).rem_euclid(len as isize) as usize;
        match self.field {
            Field::Egress => {
                self.egress_query.clear();
                if step.abs() == 1 {
                    self.egress = wrap(self.egress, self.egresses.len());
                } else {
                    self.egress = (self.egress as isize + step)
                        .clamp(0, self.egresses.len() as isize - 1)
                        as usize;
                }
            }
            Field::Kind => {
                let index = Kind::ALL.iter().position(|k| *k == self.kind).unwrap_or(0);
                self.set_kind(Kind::ALL[wrap(index, Kind::ALL.len())]);
            }
            Field::Record => {
                let index = RECORDS
                    .iter()
                    .position(|r| r.eq_ignore_ascii_case(self.record.trim()))
                    .map(|i| wrap(i, RECORDS.len()))
                    .unwrap_or(0);
                self.record = RECORDS[index].into();
            }
            Field::Count => {
                self.count = (self.count as isize + step).clamp(1, MAX_COUNT as isize) as u32;
            }
            Field::Target | Field::Server => {}
        }
    }
    fn find_egress(&mut self) {
        let query = self.egress_query.to_lowercase();
        if query.is_empty() {
            return;
        }
        if let Some(index) = self
            .egresses
            .iter()
            .position(|e| egress_label(e).to_lowercase().contains(&query))
        {
            self.egress = index;
        }
    }

    pub fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let fields = self.fields();
        let parts = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Length(fields.len() as u16 + 1),
                Constraint::Min(1),
                Constraint::Length(2),
            ])
            .split(area);
        frame.render_widget(
            Paragraph::new(tr!("Network tools · {}", clean(&self.name)))
                .block(Block::default().borders(Borders::BOTTOM)),
            parts[0],
        );
        let lines: Vec<Line> = fields
            .iter()
            .map(|field| {
                let (label, value) = match field {
                    Field::Egress => (
                        t("Egress"),
                        format!(
                            "‹ {} ›  {}/{}{}",
                            egress_label(&self.egresses[self.egress]),
                            self.egress + 1,
                            self.egresses.len(),
                            if self.egress_query.is_empty() {
                                String::new()
                            } else {
                                format!("  /{}", self.egress_query)
                            }
                        ),
                    ),
                    Field::Kind => (t("Test"), format!("‹ {} ›", t(self.kind.label()))),
                    Field::Target => (
                        t("Target"),
                        format!(
                            "{}{}",
                            self.target,
                            if self.target.is_empty() {
                                match self.kind {
                                    Kind::Tcp => "host:port",
                                    Kind::Dns => "example.com",
                                    Kind::Icmp => "host / IP",
                                }
                            } else {
                                ""
                            }
                        ),
                    ),
                    Field::Record => (t("Record"), format!("‹ {} ›", self.record)),
                    Field::Server => (
                        t("DNS server"),
                        if self.server.is_empty() {
                            "1.1.1.1 · udp:// tcp:// tls://".into()
                        } else {
                            self.server.clone()
                        },
                    ),
                    Field::Count => (t("Count"), format!("‹ {} ›", self.count)),
                };
                let selected = *field == self.field;
                Line::styled(
                    format!(
                        "{} {:<12} {}",
                        if selected { ">" } else { " " },
                        label,
                        clean(&value)
                    ),
                    if selected {
                        crate::theme::selected()
                    } else {
                        Style::default()
                    },
                )
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), parts[1]);

        let mut output: Vec<Line> = Vec::new();
        if let Some(note) = &self.note {
            output.push(Line::styled(
                clean(note),
                Style::default().fg(Color::DarkGray),
            ));
        }
        for attempt in &self.attempts {
            output.push(match (&attempt.error, attempt.rtt_ms) {
                (None, Some(ms)) => Line::styled(
                    format!(
                        "#{:<3} {:>9.2} ms  {}",
                        attempt.seq,
                        ms,
                        clean(&attempt.detail)
                    ),
                    Style::default().fg(Color::Green),
                ),
                (error, _) => Line::styled(
                    format!(
                        "#{:<3} {:>12}  {}",
                        attempt.seq,
                        t("failed"),
                        clean(error.as_deref().unwrap_or_default())
                    ),
                    Style::default().fg(Color::Red),
                ),
            });
        }
        if self.running() {
            output.push(Line::raw(t("Testing…")));
        }
        if let Some(summary) = &self.summary {
            let stats = match (summary.min_ms, summary.avg_ms, summary.max_ms) {
                (Some(min), Some(avg), Some(max)) => {
                    format!(" · min/avg/max {min:.2}/{avg:.2}/{max:.2} ms")
                }
                _ => String::new(),
            };
            output.push(Line::styled(
                format!(
                    "{}{stats}",
                    tr!(
                        "{} sent · {} received · {}% loss",
                        summary.sent,
                        summary.received,
                        summary.loss_percent()
                    )
                ),
                Style::default().fg(Color::Yellow),
            ));
        }
        if let Some(error) = &self.error {
            output.push(Line::styled(clean(error), Style::default().fg(Color::Red)));
        }
        if output.is_empty() {
            output.push(Line::raw(t(
                "Pick an egress (system route, adapter or outbound), then press Enter",
            )));
        }
        // Follow the newest lines.
        let height = parts[2].height.saturating_sub(1) as usize;
        let skip = output.len().saturating_sub(height);
        frame.render_widget(
            Paragraph::new(output.into_iter().skip(skip).collect::<Vec<_>>())
                .block(Block::default().borders(Borders::TOP).title(t("Results"))),
            parts[2],
        );
        frame.render_widget(
            Paragraph::new(t(
                "↑↓ field · ←→ choose · type to edit / find egress · Ctrl-U clear\nEnter run / stop · Esc back to node tests",
            )),
            parts[3],
        );
    }
}

fn egress_label(egress: &Egress) -> String {
    match egress {
        Egress::System => t("System route").into(),
        Egress::Interface(name) => tr!("Adapter · {}", name),
        Egress::Outbound(tag) => tr!("Outbound · {}", tag),
    }
}
fn host_part(target: &str) -> &str {
    let target = target.trim();
    if let Some(rest) = target.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    match target.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') && port.parse::<u16>().is_ok() => host,
        _ => target,
    }
}
fn clean(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

#[cfg(test)]
#[path = "test/nettools_test.rs"]
mod tests;
