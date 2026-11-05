//! Network tool probes (DNS / TCP / ICMP) pinned to one egress. Shared by the core and manager.
use serde::{Deserialize, Serialize};

/// Where probe traffic leaves the machine.
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
}

#[cfg(test)]
#[path = "test/nettool_test.rs"]
mod tests;
