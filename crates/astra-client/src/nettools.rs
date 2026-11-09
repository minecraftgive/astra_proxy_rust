//! Network tools: DNS / TCP / ICMP probes through a chosen egress (system route,
//! a local adapter, or an outbound / group of the profile). Opened from node tests.
use crate::input::{KeyCode, KeyEvent, KeyModifiers};
use crate::{i18n::t, protocol, tr};
use astra_model::nettool::Attempt;
use astra_model::nettool::Egress;
use astra_model::nettool::Event;
use astra_model::nettool::Probe;
use astra_model::nettool::Request;
use astra_model::nettool::Summary;
use astra_model::nettool::MAX_COUNT;
use std::path::{Path, PathBuf};
use tokio::{sync::mpsc, task::JoinHandle};

mod conclusions;
mod jobs;
mod lookup;
mod plan;
mod presentation;
mod result_format;
mod tools;
pub use conclusions::{OutputLine, OutputTone};
pub use presentation::{display_width, wrap_lines, Section};

pub const RECORDS: [&str; 8] = ["A", "AAAA", "CNAME", "MX", "TXT", "NS", "HTTPS", "SOA"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Dns,
    Tcp,
    Icmp,
    FakeIp,
    Report,
    Tls,
    Http,
    PublicIp,
    Stun,
    Nat,
    Trace,
    Ipv6,
}
impl Kind {
    pub const ALL: [Kind; 12] = [
        Kind::Dns,
        Kind::Tcp,
        Kind::Icmp,
        Kind::Tls,
        Kind::Http,
        Kind::PublicIp,
        Kind::Stun,
        Kind::Nat,
        Kind::Trace,
        Kind::Ipv6,
        Kind::FakeIp,
        Kind::Report,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Kind::Dns => "DNS query",
            Kind::Tcp => "TCP connect",
            Kind::Icmp => "ICMP ping",
            Kind::FakeIp => "Fake-IP lookup",
            Kind::Report => "恢复后台任务",
            Kind::Tls => "TLS 证书",
            Kind::Http => "HTTP",
            Kind::PublicIp => "HTTPS 公网 IP",
            Kind::Stun => "UDP/STUN 出口",
            Kind::Nat => "NAT 检测",
            Kind::Trace => "逐跳路径",
            Kind::Ipv6 => "IPv6 可达性",
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
    Resolver,
    Count,
    Family,
    PhysicalInterface,
    HttpMethod,
    ServerName,
    TrustComparison,
    NatAdvanced,
    Hops,
    Samples,
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
    pub family: Option<astra_model::nettool::AddressFamily>,
    pub nat_interface: Option<String>,
    pub http_method: String,
    pub server_name: String,
    pub compare_trust: bool,
    pub nat_advanced: bool,
    pub trace_hops: u8,
    pub trace_samples: u8,
    pub plan: plan::Editor,
    pub field: Field,
    pub task: Option<JoinHandle<()>>,
    pub receiver: Option<mpsc::Receiver<anyhow::Result<Event>>>,
    pub note: Option<String>,
    pub attempts: Vec<Attempt>,
    pub summary: Option<Summary>,
    pub error: Option<String>,
    pub section: Section,
    pub scroll: usize,
    /// Presentation only; does not change the probe or discard collected evidence.
    pub show_advanced: bool,
    pub context: jobs::Context,
    pub operation_id: Option<String>,
    pub report_state: Option<astra_management::v2::NetworkState>,
    pub details: Vec<serde_json::Value>,
    pub path_stale: bool,
    pub probe_path: Option<String>,
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
            kind: Kind::Tcp,
            target: "www.gstatic.com:443".into(),
            record: "A".into(),
            server: "1.1.1.1".into(),
            resolver: String::new(),
            count: 4,
            family: None,
            nat_interface: None,
            http_method: "GET".into(),
            server_name: String::new(),
            compare_trust: false,
            nat_advanced: true,
            trace_hops: 30,
            trace_samples: 3,
            plan: Default::default(),
            field: Field::Egress,
            task: None,
            receiver: None,
            note: None,
            attempts: vec![],
            summary: None,
            error: None,
            section: Section::Tools,
            scroll: 0,
            show_advanced: false,
            context: Default::default(),
            operation_id: None,
            report_state: None,
            details: vec![],
            path_stale: false,
            probe_path: None,
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
    }
    pub fn running(&self) -> bool {
        self.receiver.is_some() || self.updates.is_some()
    }
    pub fn select_egress(&mut self, index: usize) {
        if index < self.egresses.len() {
            self.egress = index;
            self.egress_query.clear();
        }
    }
    pub fn egress_choices(&self, query: &str) -> Vec<(usize, String)> {
        let query = query.to_lowercase();
        self.egresses
            .iter()
            .enumerate()
            .map(|(i, e)| (i, egress_label(e)))
            .filter(|(_, label)| label.to_lowercase().contains(&query))
            .collect()
    }
    pub fn output(&self) -> Vec<String> {
        self.output_rows().into_iter().map(|row| row.text).collect()
    }
    fn detailed_output(&self) -> Vec<String> {
        if self.section != Section::Tools {
            return self.section_output();
        }
        let mut output = vec![];
        if self.kind == Kind::Nat {
            output.push("NAT 将通过手动选择的物理网卡直接访问测试服务；服务会看到该网络的公网出口 IP。未选择物理网卡不能运行。".into());
        }
        if let Some(id) = &self.operation_id {
            output.push(format!("任务 {id} · {:?}", self.report_state));
        }
        if let Some(notice) = &self.context.notice {
            output.push(notice.clone());
        }
        if self.path_stale {
            output.push("实例运行选择已变化；本次探测仍使用启动时的出口".into());
        }
        output.extend(self.details.iter().flat_map(presentation::event_lines));
        if let Some(note) = &self.note {
            output.push(clean(note));
        }
        for attempt in &self.attempts {
            output.push(match (&attempt.error, attempt.rtt_ms) {
                (None, Some(ms)) => format!(
                    "#{:<3} 耗时 {:>9.2} ms  {}",
                    attempt.seq,
                    ms,
                    clean(&attempt.detail)
                ),
                (error, _) => format!(
                    "#{:<3} {:>12}  {}",
                    attempt.seq,
                    t("failed"),
                    clean(error.as_deref().unwrap_or_default())
                ),
            });
        }
        if self.running() {
            output.push(t("Testing…").into());
        }
        if let Some(summary) = &self.summary {
            let stats = match (summary.min_ms, summary.avg_ms, summary.max_ms) {
                (Some(min), Some(avg), Some(max)) => {
                    format!(" · min/avg/max {min:.2}/{avg:.2}/{max:.2} ms")
                }
                _ => String::new(),
            };
            output.push(format!(
                "{}{stats}",
                tr!(
                    "{} sent · {} received · {}% loss",
                    summary.sent,
                    summary.received,
                    summary.loss_percent()
                )
            ));
        }
        if let Some(error) = &self.error {
            output.push(clean(error));
        }
        if output.is_empty() {
            output.push(
                t("Pick an egress (system route, adapter or outbound), then press Enter").into(),
            );
        }
        output
    }
    pub fn start(&mut self) {
        if self.section != Section::Tools || self.running() {
            return;
        }
        if self.kind == Kind::FakeIp {
            self.start_fake_ip();
            return;
        }
        if self.kind == Kind::Report {
            self.resume_report();
            return;
        }
        let request = match self.request() {
            Ok(request) => request,
            Err(error) => {
                self.error = Some(error);
                return;
            }
        };
        self.note = None;
        self.details.clear();
        self.scroll = 0;
        self.path_stale = false;
        self.captured_runtime = self.context.runtime.clone();
        self.captured_core = self.context.core.clone();
        self.probe_path = None;
        self.operation_id = None;
        self.report_state = None;
        self.attempts.clear();
        self.summary = None;
        self.error = None;
        let (sender, receiver) = mpsc::channel(32);
        let (cancel, control) = tokio::sync::watch::channel(false);
        let client = self.client.clone();
        let instance = self.name.clone();
        let resolver = (!self.resolver.trim().is_empty()).then(|| self.resolver.trim().to_owned());
        let mut request: astra_management::v2::NetworkDiagnosticsRequest = request.into();
        request.resolver = resolver;
        request.family = self.family;
        if self.kind == Kind::Nat {
            self.nat_interface = None;
        }
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
            return;
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
        if self.receiver.take().is_some() {
            self.error = Some(t("Stopped").into());
            self.summary = Some(Summary::from_attempts(&self.attempts));
        }
    }
    pub fn poll(&mut self) -> bool {
        let mut changed = self.poll_jobs();
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
                #[allow(unreachable_patterns)]
                Ok(Ok(event)) => self
                    .details
                    .push(serde_json::to_value(event).unwrap_or_default()),
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
        if key.modifiers == KeyModifiers::CONTROL {
            match key.code {
                KeyCode::Char('d') => self.toggle_advanced(),
                KeyCode::Left | KeyCode::Right | KeyCode::Char('l') => {
                    let index = Section::ALL
                        .iter()
                        .position(|s| *s == self.section)
                        .unwrap_or(0);
                    let step = if key.code == KeyCode::Left { 3 } else { 1 };
                    self.section = Section::ALL[(index + step) % Section::ALL.len()];
                    self.scroll = 0;
                }
                KeyCode::Up => self.scroll = self.scroll.saturating_add(10),
                KeyCode::Down => self.scroll = self.scroll.saturating_sub(10),
                KeyCode::Char('p') => self.error = self.add_project().err(),
                KeyCode::Char('r') => {
                    self.error = self.add_presets().err();
                    self.section = Section::Findings;
                    self.scroll = 0;
                }
                KeyCode::Char('o') => self.stop_observing(),
                KeyCode::Char('y') => self.copy_result(false),
                KeyCode::Char('e') => self.copy_result(true),
                _ => return self.form_key(key),
            }
            return true;
        }
        self.form_key(key)
    }
    fn form_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_add(10);
                return true;
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_sub(10);
                return true;
            }
            _ => {}
        }
        if self.section == Section::Findings {
            return self.plan_key(key);
        }
        if self.section != Section::Tools {
            if self.running() && matches!(key.code, KeyCode::Esc | KeyCode::Enter) {
                self.stop();
                return true;
            }
            return !matches!(key.code, KeyCode::Esc);
        }
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
                } else if matches!(self.field, Field::Count | Field::Hops | Field::Samples) {
                    let current = self.field_value(self.field).parse::<u32>().unwrap_or(1);
                    let _ = self.set_text(self.field, (current / 10).max(1).to_string());
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
                        'f' => Some(Kind::FakeIp),
                        'r' => Some(Kind::Report),
                        's' => Some(Kind::Tls),
                        'h' => Some(Kind::Http),
                        'a' => Some(Kind::PublicIp),
                        'u' => Some(Kind::Stun),
                        'n' => Some(Kind::Nat),
                        'j' => Some(Kind::Trace),
                        '6' => Some(Kind::Ipv6),
                        _ => None,
                    } {
                        self.set_kind(kind);
                    }
                }
                Field::Count | Field::Hops | Field::Samples => {
                    if let Some(digit) = c.to_digit(10) {
                        let maximum = match self.field {
                            Field::Count => MAX_COUNT,
                            Field::Hops => 30,
                            _ => 3,
                        };
                        let current = self.field_value(self.field).parse::<u32>().unwrap_or(1);
                        let appended = current * 10 + digit;
                        let next = if appended > maximum { digit } else { appended };
                        let _ = self.set_text(self.field, next.clamp(1, maximum).to_string());
                    }
                }
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

#[cfg(test)]
#[path = "test/nettools_jobs_test.rs"]
mod jobs_tests;

#[cfg(test)]
#[path = "test/nettools_stage2_test.rs"]
mod stage2_tests;

#[cfg(test)]
#[path = "test/nettools_conclusions_test.rs"]
mod conclusions_tests;
