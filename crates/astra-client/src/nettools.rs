//! Network tools: the essential probes (ICMP, TCP, DNS, trace, HTTP(S), public
//! IP, NAT type, Fake-IP lookup and a fixed full check) through a chosen egress,
//! plus the network link display. Shared by the GUI and TUI.
use crate::input::{KeyCode, KeyEvent, KeyModifiers};
use crate::{i18n::t, tr};
use astra_model::nettool::Attempt;
use astra_model::nettool::Egress;
use astra_model::nettool::Probe;
use astra_model::nettool::Request;
use astra_model::nettool::Summary;
use astra_model::nettool::MAX_COUNT;
use std::path::{Path, PathBuf};
use tokio::{sync::mpsc, task::JoinHandle};

pub mod chain;
mod form;
pub mod full_check;
mod jobs;
mod lookup;
pub mod nat;
mod result;
pub use chain::{LiveChain, PathStrip};
pub use form::{egress_allowed, target_ports};
pub use result::{display_width, nat_rows, unwrap, wrap_lines, OutputLine, OutputTone};

pub const RECORDS: [&str; 8] = ["A", "AAAA", "CNAME", "MX", "TXT", "NS", "HTTPS", "SOA"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Icmp,
    Tcp,
    Dns,
    Trace,
    Http,
    PublicIp,
    Nat,
    FakeIp,
    FullCheck,
}
impl Kind {
    pub const ALL: [Kind; 9] = [
        Kind::Icmp,
        Kind::Tcp,
        Kind::Dns,
        Kind::Trace,
        Kind::Http,
        Kind::PublicIp,
        Kind::Nat,
        Kind::FakeIp,
        Kind::FullCheck,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Kind::Icmp => "Ping",
            Kind::Tcp => "TCP 连接",
            Kind::Dns => "DNS 查询",
            Kind::Trace => "逐跳路径",
            Kind::Http => "HTTP(S)",
            Kind::PublicIp => "公网 IP",
            Kind::Nat => "NAT 类型",
            Kind::FakeIp => "Fake-IP",
            Kind::FullCheck => "完整检查",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Kind,
    Egress,
    Target,
    Record,
    Server,
    Resolver,
    Count,
    HttpMethod,
    Hops,
    PhysicalInterface,
    Family,
    NatAdvanced,
    Comparison,
    CompareRounds,
    DualStack,
    IncludeNat,
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
    pub resolver: String,
    pub count: u32,
    pub http_method: String,
    pub trace_hops: u8,
    /// NAT only: manually selected physical interface, cleared after each run.
    pub nat_interface: Option<String>,
    pub family: astra_model::nettool::AddressFamily,
    pub nat_advanced: bool,
    /// Full check: optional comparison egress index.
    pub comparison: Option<usize>,
    pub compare_rounds: bool,
    pub dual_stack: bool,
    pub include_nat: bool,
    pub field: Field,
    pub task: Option<JoinHandle<()>>,
    pub note: Option<String>,
    pub attempts: Vec<Attempt>,
    pub summary: Option<Summary>,
    pub error: Option<String>,
    pub scroll: usize,
    pub context: jobs::Context,
    pub operation_id: Option<String>,
    pub report_state: Option<astra_management::v2::NetworkState>,
    pub details: Vec<serde_json::Value>,
    pub path_stale: bool,
    client: crate::network_diagnostics::Client,
    updates: Option<mpsc::Receiver<jobs::Update>>,
    cancel: Option<tokio::sync::watch::Sender<bool>>,
    refresh: Option<JoinHandle<()>>,
    context_receiver: Option<mpsc::Receiver<jobs::Context>>,
    next_refresh: std::time::Instant,
    captured_runtime: Option<serde_json::Value>,
    captured_core: Option<serde_json::Value>,
}
impl Drop for View {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        if let Some(task) = self.refresh.take() {
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
        let egresses = std::iter::once(Egress::System)
            .chain(outbounds.iter().cloned().map(Egress::Outbound))
            .collect();
        Self {
            waker: Default::default(),
            name,
            socket: socket.to_owned(),
            egresses,
            egress: 0,
            egress_query: String::new(),
            kind: Kind::Icmp,
            target: "www.gstatic.com".into(),
            record: "A".into(),
            server: "1.1.1.1".into(),
            resolver: String::new(),
            count: 4,
            http_method: "GET".into(),
            trace_hops: 30,
            nat_interface: None,
            family: astra_model::nettool::AddressFamily::Ipv4,
            nat_advanced: true,
            comparison: None,
            compare_rounds: false,
            dual_stack: false,
            include_nat: false,
            field: Field::Kind,
            task: None,
            note: None,
            attempts: vec![],
            summary: None,
            error: None,
            scroll: 0,
            context: Default::default(),
            operation_id: None,
            report_state: None,
            details: vec![],
            path_stale: false,
            client: crate::network_diagnostics::Client::new(socket),
            updates: None,
            cancel: None,
            refresh: None,
            context_receiver: None,
            next_refresh: std::time::Instant::now(),
            captured_runtime: None,
            captured_core: None,
        }
    }
    /// Replace the outbound choices (the catalog loads after a direct open), keeping the selection.
    pub fn set_outbounds(&mut self, outbounds: &[String]) {
        let selected = self.egresses[self.egress].clone();
        let comparison = self.comparison.map(|i| self.egresses[i].clone());
        self.egresses
            .retain(|egress| !matches!(egress, Egress::Outbound(_)));
        self.egresses
            .extend(outbounds.iter().cloned().map(Egress::Outbound));
        // A disappeared selection must remain visible and fail validation at the
        // target Agent, never silently turn the next request into system traffic.
        if matches!(selected, Egress::Outbound(_)) && !self.egresses.contains(&selected) {
            self.egresses.push(selected.clone());
        }
        self.egress = self
            .egresses
            .iter()
            .position(|egress| *egress == selected)
            .unwrap_or(0);
        self.comparison = comparison.and_then(|c| self.egresses.iter().position(|e| *e == c));
    }
    pub fn running(&self) -> bool {
        self.updates.is_some()
    }
    pub fn select_egress(&mut self, index: usize) {
        if index < self.egresses.len() {
            self.egress = index;
            self.egress_query.clear();
            if self.comparison == Some(index) {
                self.comparison = None;
            }
        }
    }
    pub fn select_comparison(&mut self, index: Option<usize>) {
        self.comparison = index.filter(|i| *i < self.egresses.len() && *i != self.egress);
    }
    pub fn egress_choices(&self, query: &str) -> Vec<(usize, String)> {
        let query = query.to_lowercase();
        self.allowed_egresses()
            .into_iter()
            .filter(|(_, label)| label.to_lowercase().contains(&query))
            .collect()
    }
    /// The always-visible link: follows the selected egress, or the instance's
    /// own proxy path when the system route is selected and TUN captures it.
    pub fn live_chain(&self) -> LiveChain {
        chain::live_chain(
            self.context.runtime.as_ref(),
            self.context.network.as_ref(),
            &self.egresses[self.egress],
        )
    }
    fn reset_result(&mut self) {
        self.note = None;
        self.details.clear();
        self.scroll = 0;
        self.path_stale = false;
        self.captured_runtime = self.context.runtime.clone();
        self.captured_core = self.context.core.clone();
        self.operation_id = None;
        self.report_state = None;
        self.attempts.clear();
        self.summary = None;
        self.error = None;
    }
    pub fn start(&mut self) {
        if self.running() {
            return;
        }
        match self.kind {
            Kind::FakeIp => return self.start_fake_ip(),
            Kind::FullCheck => {
                return match self.full_check_request() {
                    Ok(request) => self.start_batch(request),
                    Err(error) => self.error = Some(error),
                }
            }
            _ => {}
        }
        let request = match self.request() {
            Ok(request) => request,
            Err(error) => {
                self.error = Some(error);
                return;
            }
        };
        self.reset_result();
        if self.kind == Kind::Nat {
            // A NAT interface choice is never carried over to another run.
            self.nat_interface = None;
        }
        let (sender, receiver) = mpsc::channel(32);
        let (cancel, control) = tokio::sync::watch::channel(false);
        let client = self.client.clone();
        let instance = self.name.clone();
        let waker = self.waker.clone();
        self.task = Some(tokio::spawn(async move {
            jobs::run(client, instance, request, sender, control, waker).await;
        }));
        self.cancel = Some(cancel);
        self.updates = Some(receiver);
    }
    pub fn stop(&mut self) {
        if let Some(cancel) = &self.cancel {
            cancel.send_replace(true);
            self.note = Some(
                if self.operation_id.is_none() && self.kind == Kind::FakeIp {
                    "正在停止读取核心映射".into()
                } else {
                    "正在取消；等待目标 Agent 确认探测已退出".into()
                },
            );
        }
    }
    pub fn poll(&mut self) -> bool {
        self.poll_jobs()
    }
    /// Returns false when the key should close the page (Esc while idle).
    pub fn key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl {
            match key.code {
                KeyCode::Up => self.scroll = self.scroll.saturating_add(10),
                KeyCode::Down => self.scroll = self.scroll.saturating_sub(10),
                KeyCode::Char('y') => self.copy_result(false),
                KeyCode::Char('e') => self.copy_result(true),
                KeyCode::Char('u') => {
                    if let Some(text) = self.text_mut() {
                        text.clear();
                    }
                    self.egress_query.clear();
                }
                _ => {}
            }
            return true;
        }
        let fields = self.fields();
        let index = fields.iter().position(|f| *f == self.field).unwrap_or(0);
        match key.code {
            KeyCode::Esc if self.running() => self.stop(),
            KeyCode::Esc => return false,
            KeyCode::Enter if self.running() => self.stop(),
            KeyCode::Enter => self.start(),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_add(10),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::Up | KeyCode::BackTab => {
                self.field = fields[(index + fields.len() - 1) % fields.len()];
                self.egress_query.clear();
            }
            KeyCode::Down | KeyCode::Tab => {
                self.field = fields[(index + 1) % fields.len()];
                self.egress_query.clear();
            }
            KeyCode::Left => self.cycle(-1),
            KeyCode::Right => self.cycle(1),
            KeyCode::Backspace => {
                if self.field == Field::Egress {
                    self.egress_query.pop();
                    self.find_egress();
                } else if matches!(self.field, Field::Count | Field::Hops) {
                    let current = self.field_value(self.field).parse::<u32>().unwrap_or(1);
                    let _ = self.set_text(self.field, (current / 10).max(1).to_string());
                } else if let Some(text) = self.text_mut() {
                    text.pop();
                }
            }
            KeyCode::Char(c) => match self.field {
                Field::Egress => {
                    self.egress_query.push(c);
                    self.find_egress();
                }
                Field::Kind if c == ' ' => self.cycle(1),
                Field::Kind => {
                    if let Some(kind) = match c.to_ascii_lowercase() {
                        'p' | 'i' => Some(Kind::Icmp),
                        't' => Some(Kind::Tcp),
                        'd' => Some(Kind::Dns),
                        'r' | 'j' => Some(Kind::Trace),
                        'h' => Some(Kind::Http),
                        'a' => Some(Kind::PublicIp),
                        'n' => Some(Kind::Nat),
                        'f' => Some(Kind::FakeIp),
                        'c' => Some(Kind::FullCheck),
                        _ => None,
                    } {
                        self.set_kind(kind);
                    }
                }
                Field::Count | Field::Hops => {
                    if let Some(digit) = c.to_digit(10) {
                        let maximum = if self.field == Field::Count {
                            MAX_COUNT
                        } else {
                            30
                        };
                        let current = self.field_value(self.field).parse::<u32>().unwrap_or(1);
                        let appended = current * 10 + digit;
                        let next = if appended > maximum { digit } else { appended };
                        let _ = self.set_text(self.field, next.clamp(1, maximum).to_string());
                    }
                }
                field if !field.editable() && c == ' ' => self.cycle(1),
                _ => {
                    let max_chars = if self.kind == Kind::FakeIp { 4096 } else { 512 };
                    if let Some(text) = self.text_mut() {
                        if text.chars().count() < max_chars {
                            text.push(c);
                        }
                    }
                }
            },
            _ => {}
        }
        true
    }
    pub fn find_egress(&mut self) {
        let query = self.egress_query.to_lowercase();
        if query.is_empty() {
            return;
        }
        if let Some((index, _)) = self
            .allowed_egresses()
            .into_iter()
            .find(|(_, label)| label.to_lowercase().contains(&query))
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

#[cfg(test)]
#[path = "test/nettools_jobs_test.rs"]
mod jobs_tests;
