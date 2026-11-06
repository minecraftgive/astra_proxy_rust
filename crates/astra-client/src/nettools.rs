//! Network tools: DNS / TCP / ICMP probes through a chosen egress (system route,
//! a local adapter, or an outbound / group of the profile). Opened from node tests.
use crate::input::{KeyCode, KeyEvent, KeyModifiers};
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
use std::path::{Path, PathBuf};
use tokio::{sync::mpsc, task::JoinHandle};

pub const RECORDS: [&str; 8] = ["A", "AAAA", "CNAME", "MX", "TXT", "NS", "HTTPS", "SOA"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Dns,
    Tcp,
    Icmp,
}
impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Dns, Kind::Tcp, Kind::Icmp];
    pub fn label(self) -> &'static str {
        match self {
            Kind::Dns => "DNS query",
            Kind::Tcp => "TCP ping",
            Kind::Icmp => "ICMP ping",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Egress,
    Kind,
    Target,
    Record,
    Server,
    Count,
}

pub struct View {
    pub waker: crate::waker::Waker,
    pub name: String,
    pub socket: PathBuf,
    pub egresses: Vec<Egress>,
    pub egress: usize,
    /// Type-ahead text for the egress field.
    pub egress_query: String,
    pub kind: Kind,
    pub target: String,
    pub record: String,
    pub server: String,
    pub count: u32,
    pub field: Field,
    pub task: Option<JoinHandle<()>>,
    pub receiver: Option<mpsc::Receiver<anyhow::Result<Event>>>,
    pub note: Option<String>,
    pub attempts: Vec<Attempt>,
    pub summary: Option<Summary>,
    pub error: Option<String>,
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
    /// `outbounds` comes from the node-test catalog (DIRECT, nodes and groups).
    pub fn new(socket: &Path, name: String, outbounds: &[String]) -> Self {
        let interfaces = crate::network_interfaces::connected().unwrap_or_default();
        let egresses = std::iter::once(Egress::System)
            .chain(interfaces.into_iter().map(Egress::Interface))
            .chain(outbounds.iter().cloned().map(Egress::Outbound))
            .collect();
        Self {
            waker: Default::default(),
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
    pub fn fields(&self) -> Vec<Field> {
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
    pub fn set_kind(&mut self, kind: Kind) {
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
    pub fn request(&self) -> Result<Request, String> {
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
    pub fn start(&mut self) {
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
        let waker = self.waker.clone();
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
                    waker.wake();
                    if done {
                        break;
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
    pub fn stop(&mut self) {
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
    pub fn text_mut(&mut self) -> Option<&mut String> {
        match self.field {
            Field::Target => Some(&mut self.target),
            Field::Record => Some(&mut self.record),
            Field::Server => Some(&mut self.server),
            _ => None,
        }
    }
    pub fn cycle(&mut self, step: isize) {
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
    pub fn find_egress(&mut self) {
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
}

pub fn egress_label(egress: &Egress) -> String {
    match egress {
        Egress::System => t("System route").into(),
        Egress::Interface(name) => tr!("Adapter · {}", name),
        Egress::Outbound(tag) => tr!("Outbound · {}", tag),
    }
}
pub fn host_part(target: &str) -> &str {
    let target = target.trim();
    if let Some(rest) = target.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    match target.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') && port.parse::<u16>().is_ok() => host,
        _ => target,
    }
}
pub fn clean(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

#[cfg(test)]
#[path = "test/nettools_test.rs"]
mod tests;
