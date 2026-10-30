//! Network tools: DNS / TCP / ICMP probes through a chosen egress (system route,
//! a local adapter, or an outbound / group of the profile). Opened from node tests.
use crate::{
    i18n::t,
    protocol::{self, Action},
    tr,
};
use astra_ipc::nettool::{Attempt, Egress, Event, Probe, Request, Summary, MAX_COUNT};
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
                let mut stream = astra_ipc::platform::connect_trusted(&socket).await?;
                astra_ipc::send_management(
                    &mut stream,
                    &protocol::Request {
                        version: astra_ipc::API_VERSION,
                        action,
                    },
                )
                .await?;
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
                    if done {
                        break;
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
    fn stop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        if self.receiver.take().is_some() {
            self.error = Some(t("Stopped").into());
            self.summary = Some(Summary::from_attempts(&self.attempts));
        }
    }
    pub fn poll(&mut self) {
        let Some(mut receiver) = self.receiver.take() else {
            return;
        };
        loop {
            match receiver.try_recv() {
                Ok(Ok(Event::Done)) => {
                    self.task = None;
                    return;
                }
                Ok(Ok(Event::Started { note, .. })) => self.note = Some(note),
                Ok(Ok(Event::Attempt(attempt))) => self.attempts.push(attempt),
                Ok(Ok(Event::Summary(summary))) => self.summary = Some(summary),
                Ok(Err(error)) => {
                    self.error = Some(clean(&format!("{error:#}")));
                    self.task = None;
                    return;
                }
                Err(mpsc::error::TryRecvError::Empty) => {
                    self.receiver = Some(receiver);
                    return;
                }
                Err(_) => {
                    self.error = Some(t("Diagnostics interrupted").into());
                    self.task = None;
                    return;
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
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::from(code)
    }
    fn view() -> View {
        let mut view = View::new(
            Path::new("unused-nettool.sock"),
            "proxy".into(),
            &["DIRECT".into(), "HK 01".into(), "Auto".into()],
        );
        view.egresses = vec![
            Egress::System,
            Egress::Interface("Wi-Fi".into()),
            Egress::Outbound("DIRECT".into()),
            Egress::Outbound("HK 01".into()),
        ];
        view
    }
    #[tokio::test]
    async fn form_builds_requests_for_each_probe_and_egress() {
        let mut view = view();
        // Type-ahead jumps to the matching outbound.
        for c in "hk".chars() {
            view.key(key(KeyCode::Char(c)));
        }
        assert_eq!(view.egresses[view.egress], Egress::Outbound("HK 01".into()));
        view.key(key(KeyCode::Right));
        assert_eq!(view.egress, 0, "←→ wraps around");
        view.key(key(KeyCode::Left));
        view.key(key(KeyCode::Left));
        assert_eq!(
            view.egresses[view.egress],
            Egress::Outbound("DIRECT".into())
        );
        let request = view.request().unwrap();
        assert_eq!(
            request.probe,
            Probe::Tcp {
                host: "www.gstatic.com".into(),
                port: 443
            }
        );
        view.key(key(KeyCode::Down));
        view.key(key(KeyCode::Char('d')));
        assert_eq!(view.kind, Kind::Dns);
        assert_eq!(view.target, "www.gstatic.com");
        assert_eq!(view.fields().len(), 6);
        view.key(key(KeyCode::Down));
        view.key(key(KeyCode::Down));
        view.key(key(KeyCode::Right));
        let request = view.request().unwrap();
        assert!(
            matches!(&request.probe, Probe::Dns { record, server, .. } if record == "AAAA" && server == "1.1.1.1")
        );
        assert_eq!(request.count, 1);
        view.field = Field::Kind;
        view.key(key(KeyCode::Char('i')));
        assert_eq!(view.fields().len(), 4);
        view.field = Field::Count;
        for c in "120".chars() {
            view.key(key(KeyCode::Char(c)));
        }
        assert!(view.count <= MAX_COUNT);
        view.field = Field::Target;
        view.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert!(view.request().is_err());
        view.key(key(KeyCode::Enter));
        assert!(view.error.is_some() && !view.running());
        assert!(
            !view.key(key(KeyCode::Esc)),
            "Esc while idle leaves the page"
        );
    }
    #[tokio::test]
    async fn streamed_results_render_and_stop_keeps_partial_summary() {
        let mut view = view();
        let (sender, receiver) = mpsc::channel(8);
        view.receiver = Some(receiver);
        for event in [
            Event::Started {
                egress: "iface:Wi-Fi".into(),
                probe: "TCP host:443".into(),
                note: "adapter Wi-Fi (index 7, 192.0.2.10)".into(),
                running_config: true,
            },
            Event::Attempt(Attempt {
                seq: 1,
                rtt_ms: Some(12.5),
                detail: "connected to 192.0.2.1:443".into(),
                error: None,
            }),
            Event::Attempt(Attempt {
                seq: 2,
                rtt_ms: None,
                detail: String::new(),
                error: Some("timed out after 3000 ms".into()),
            }),
        ] {
            sender.send(Ok(event)).await.unwrap();
        }
        view.poll();
        assert!(view.running());
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|f| view.render(f, f.area())).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            screen.contains("12.50 ms")
                && screen.contains("192.0.2.10")
                && screen.contains("timed out"),
            "{screen}"
        );
        assert!(view.key(key(KeyCode::Esc)), "Esc while running only stops");
        assert!(!view.running());
        let summary = view.summary.clone().unwrap();
        assert_eq!((summary.sent, summary.received), (2, 1));
        for (width, height) in [(40, 10), (5, 3)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|f| view.render(f, f.area())).unwrap();
        }
    }
    #[tokio::test]
    async fn late_outbounds_keep_the_selected_egress() {
        let mut view = view();
        view.egress = 1;
        view.set_outbounds(&["DIRECT".into(), "JP".into()]);
        assert_eq!(
            view.egresses[view.egress],
            Egress::Interface("Wi-Fi".into())
        );
        assert_eq!(view.egresses.last(), Some(&Egress::Outbound("JP".into())));
        view.egress = view.egresses.len() - 1;
        view.set_outbounds(&[]);
        assert_eq!(view.egresses[view.egress], Egress::System);
    }
    #[test]
    fn host_part_strips_ports_but_not_ipv6() {
        assert_eq!(host_part("example.com:443"), "example.com");
        assert_eq!(host_part("[2001:db8::1]:443"), "2001:db8::1");
        assert_eq!(host_part("2001:db8::1"), "2001:db8::1");
        assert_eq!(host_part("1.1.1.1"), "1.1.1.1");
    }
}
