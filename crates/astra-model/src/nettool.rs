//! Network tool probes (DNS / TCP / ICMP) pinned to one egress. Shared by the core and manager.
use serde::{Deserialize, Serialize};

/// Where probe traffic leaves the machine.
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "name", rename_all = "snake_case")]
pub enum Egress {
    /// Plain OS sockets; the routing table decides.
    System,
    /// Sockets pinned to a local adapter (alias or index on Windows, name on Unix).
    Interface(String),
    /// An outbound or proxy group of the profile; `DIRECT` uses the profile's direct path.
    Outbound(String),
}
impl std::fmt::Display for Egress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Egress::System => f.write_str("system"),
            Egress::Interface(name) => write!(f, "iface:{name}"),
            Egress::Outbound(tag) => write!(f, "outbound:{tag}"),
        }
    }
}
impl std::str::FromStr for Egress {
    type Err = String;
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let text = text.trim();
        if text.is_empty() || text.eq_ignore_ascii_case("system") {
            return Ok(Egress::System);
        }
        if let Some(name) = text.strip_prefix("iface:").or(text.strip_prefix("if:")) {
            return non_empty(name).map(Egress::Interface);
        }
        if let Some(tag) = text.strip_prefix("outbound:").or(text.strip_prefix("out:")) {
            return non_empty(tag).map(Egress::Outbound);
        }
        Err(format!(
            "unknown egress {text}; use system, iface:NAME or outbound:TAG"
        ))
    }
}
fn non_empty(text: &str) -> Result<String, String> {
    let text = text.trim();
    if text.is_empty() {
        Err("egress name is empty".into())
    } else {
        Ok(text.to_owned())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum AddressFamily {
    #[default]
    Ipv4,
    Ipv6,
}
impl AddressFamily {
    pub fn matches(self, address: std::net::IpAddr) -> bool {
        matches!(
            (self, address),
            (Self::Ipv4, std::net::IpAddr::V4(_)) | (Self::Ipv6, std::net::IpAddr::V6(_))
        )
    }
}

#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Probe {
    /// `server` accepts `IP[:port]`, `udp://`, `tcp://` or `tls://` forms.
    Dns {
        name: String,
        record: String,
        server: String,
    },
    Tcp {
        host: String,
        port: u16,
    },
    Icmp {
        host: String,
    },
    // Append only: the first three discriminants belong to the legacy bincode
    // interface. New kinds are admitted exclusively by capability-gated jobs.
    TcpPorts {
        host: String,
        ports: Vec<u16>,
    },
    Tls {
        host: String,
        port: u16,
        server_name: Option<String>,
        compare_bundled: bool,
    },
    Http {
        url: String,
        method: String,
        compare_bundled: bool,
    },
    HttpsIp {
        url: String,
        compare_bundled: bool,
    },
    Stun {
        server: String,
    },
    Nat {
        request: crate::network_nat::Request,
    },
    Trace {
        host: String,
        max_hops: u8,
        probes_per_hop: u8,
    },
}
impl std::fmt::Display for Probe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Probe::Dns {
                name,
                record,
                server,
            } => write!(f, "DNS {record} {name} @{server}"),
            Probe::Tcp { host, port } if host.contains(':') => write!(f, "TCP [{host}]:{port}"),
            Probe::Tcp { host, port } => write!(f, "TCP {host}:{port}"),
            Probe::Icmp { host } => write!(f, "ICMP {host}"),
            Probe::TcpPorts { host, ports } => write!(f, "TCP {host} · {} ports", ports.len()),
            Probe::Tls { host, port, .. } => write!(f, "TLS {host}:{port}"),
            Probe::Http { method, .. } => write!(f, "HTTP {method}"),
            Probe::HttpsIp { .. } => f.write_str("HTTPS public IP"),
            Probe::Stun { .. } => f.write_str("UDP/STUN egress"),
            Probe::Nat { .. } => f.write_str("Physical-interface NAT"),
            Probe::Trace { host, .. } => write!(f, "Trace {host}"),
        }
    }
}

pub const MAX_COUNT: u32 = 50;
/// Stdin payload for `--nettool-stream`; the manager fills `config`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    #[serde(default)]
    pub config: String,
    pub egress: Egress,
    pub probe: Probe,
    pub count: u32,
    pub interval_ms: u64,
    pub timeout_ms: u64,
}

/// Separate JSON envelope for the versioned diagnostic entry point. The legacy
/// Request and its bincode layout remain unchanged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetailedRequest {
    #[serde(flatten)]
    pub request: Request,
    #[serde(default)]
    pub pinned: Option<PinnedEgress>,
    #[serde(default)]
    pub resolver: Option<String>,
    #[serde(default)]
    pub family: Option<AddressFamily>,
    #[serde(default)]
    pub path_context: Option<crate::network_path::PathContext>,
    /// Filled only by the target Agent after OS sampling, never by a frontend.
    #[serde(default)]
    pub interface_resolvers: Vec<crate::network_state::Resolver>,
}
impl From<Request> for DetailedRequest {
    fn from(request: Request) -> Self {
        Self {
            request,
            pinned: None,
            resolver: None,
            family: None,
            path_context: None,
            interface_resolvers: vec![],
        }
    }
}
impl std::ops::Deref for DetailedRequest {
    type Target = Request;
    fn deref(&self) -> &Request {
        &self.request
    }
}
impl std::ops::DerefMut for DetailedRequest {
    fn deref_mut(&mut self) -> &mut Request {
        &mut self.request
    }
}

/// A single immutable diagnostic path. Node credentials are private request
/// data; reports may retain the chain and binding but must never retain node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PinnedEgress {
    pub version: u16,
    pub requested_tag: String,
    pub leaf_tag: String,
    pub effective_interface: Option<String>,
    pub interface_source: Option<String>,
    pub selection_chain: Vec<SelectionHop>,
    pub node: Option<crate::config::protocol::OutboundConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionHop {
    pub group: String,
    pub mode: String,
    pub selected: String,
    /// Runtime selection, or membership inherited from a named outer selection.
    pub source: String,
    pub interface: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Attempt {
    pub seq: u32,
    pub rtt_ms: Option<f64>,
    /// Peer address, DNS answers or other success details.
    pub detail: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub sent: u32,
    pub received: u32,
    pub min_ms: Option<f64>,
    pub avg_ms: Option<f64>,
    pub max_ms: Option<f64>,
}
impl Summary {
    pub fn from_attempts<'a>(attempts: impl IntoIterator<Item = &'a Attempt>) -> Self {
        let mut summary = Summary::default();
        let mut total = 0.0;
        for attempt in attempts {
            summary.sent += 1;
            if let (Some(ms), None) = (attempt.rtt_ms, &attempt.error) {
                summary.received += 1;
                total += ms;
                summary.min_ms = Some(summary.min_ms.map_or(ms, |v| v.min(ms)));
                summary.max_ms = Some(summary.max_ms.map_or(ms, |v| v.max(ms)));
            }
        }
        if summary.received > 0 {
            summary.avg_ms = Some(total / summary.received as f64);
        }
        summary
    }
    pub fn loss_percent(&self) -> u32 {
        (u64::from(self.received) * 100)
            .checked_div(u64::from(self.sent))
            .map_or(0, |received| 100_u64.saturating_sub(received) as u32)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Started {
        egress: String,
        probe: String,
        /// How the egress was realised, e.g. the bound source address.
        note: String,
        running_config: bool,
    },
    Attempt(Attempt),
    Summary(Summary),
    Done,
    /// Emitted only by the versioned detailed diagnostic entry point.
    Dns {
        seq: u32,
        detail: Box<DnsDetail>,
    },
    /// Added only by the manager's versioned job runner, never the legacy stream.
    Context(Box<DiagnosticContext>),
    /// Versioned diagnostics only. A separate record for each resolver boundary.
    Resolution {
        seq: u32,
        purpose: String,
        host: String,
        method: String,
        resolver: Option<String>,
        addresses: Vec<String>,
        interface: Option<String>,
    },
    Tls {
        seq: u32,
        detail: Box<crate::network_tls::TlsDetail>,
    },
    Http {
        seq: u32,
        detail: Box<crate::network_tls::HttpDetail>,
    },
    PublicIp {
        seq: u32,
        address: String,
        source: String,
        service: String,
    },
    Nat(crate::network_nat::Event),
    Stun(crate::network_nat::StunResult),
    Trace {
        seq: u32,
        sample: crate::network_path::TraceSample,
    },
    TraceSummary(crate::network_path::TraceSummary),
    Echo {
        seq: u32,
        observation: crate::network_path::EchoObservation,
    },
    Tcp {
        seq: u32,
        result: ConnectionResult,
    },
    /// A completed diagnostic connection. Proxy entry and business target are
    /// independent; an unobservable remote DNS address stays None.
    Connection {
        seq: u32,
        purpose: String,
        host: String,
        address: Option<String>,
        interface: Option<String>,
        address_source: String,
        family: Option<AddressFamily>,
    },
    /// Metadata supplied by the Agent, never accepted from a probe child.
    Project {
        project: crate::network_analysis::ProjectContext,
        detail: Box<Event>,
    },
    ProjectStatus {
        project_id: String,
        path_id: String,
        round: u8,
        state: String,
        message: String,
    },
    Policy(crate::network_analysis::PathPolicy),
    Analysis(Box<crate::network_analysis::AnalysisReport>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectionResult {
    pub host: String,
    pub port: u16,
    pub family: Option<AddressFamily>,
    pub outcome: ConnectionOutcome,
    /// Includes resolution and proxy negotiation, never a target network RTT.
    pub connect_ms: Option<f64>,
    pub message: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionOutcome {
    Connected,
    Refused,
    TimedOut,
    Unreachable,
    Failed,
}

/// Snapshot of the execution context, not a claim that a packet was sent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiagnosticContext {
    pub sampled_at_ms: String,
    pub running_core: Option<crate::control::Identity>,
    pub config_digest: Option<String>,
    /// `matched_running`, `saved_not_applied`, or an explanation of unavailable evidence.
    pub config_observation: String,
    pub actual_egress: Egress,
    /// `requested` or `pinned_request`; connection evidence follows separately.
    pub egress_source: String,
    /// True when the manager supplied a verified, immutable configured path.
    /// The subprocess still never initializes a complete profile.
    pub config_used: bool,
    pub selection_chain: Vec<SelectionHop>,
    pub effective_interface: Option<String>,
    pub interface_source: Option<String>,
    pub probe_binary: ProbeBinaryIdentity,
    /// Older compatible streams contain attempts and summary, without DNS detail.
    pub legacy_details: bool,
}

/// Executable identity sampled from its own `--program-info` subprocess.
/// The platform's ProgramInfo collection API depends on this crate, so the
/// report retains its identity fields without importing platform operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeBinaryIdentity {
    pub name: String,
    pub version: String,
    pub compiled_at: String,
    pub target: String,
    pub profile: String,
    pub rustc: String,
    pub git_revision: Option<String>,
    /// PID of the short-lived `--program-info` query, not the later probe.
    /// The diagnostic subprocess PID is not included in this metadata snapshot.
    pub capability_process_id: u32,
    pub executable: Option<String>,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DnsDetail {
    Response {
        transport: String,
        server: String,
        outcome: String,
        response_received: bool,
        parsed: bool,
        rcode: String,
        flags: DnsFlags,
        edns: Option<DnsEdns>,
        questions: Vec<DnsQuestion>,
        answer_count: u32,
        authority_count: u32,
        additional_count: u32,
        cname_chain: Vec<String>,
        truncated: bool,
        /// AD is a server assertion, never proof of local DNSSEC validation.
        dnssec_locally_validated: bool,
    },
    Records {
        section: String,
        records: Vec<DnsRecord>,
    },
    Retry {
        from: String,
        to: String,
        reason: String,
    },
    Failure {
        outcome: String,
        response_received: bool,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DnsFlags {
    pub authoritative: bool,
    pub truncated: bool,
    pub recursion_desired: bool,
    pub recursion_available: bool,
    pub authenticated_data: bool,
    pub checking_disabled: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DnsEdns {
    pub version: u8,
    pub udp_payload: u16,
    pub dnssec_ok: bool,
    pub options: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DnsQuestion {
    pub name: String,
    pub record_type: String,
    pub class: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DnsRecord {
    pub name: String,
    pub record_type: String,
    pub class: String,
    pub ttl: u32,
    pub text: String,
    pub text_truncated: bool,
}

#[cfg(test)]
#[path = "test/nettool_test.rs"]
mod tests;
