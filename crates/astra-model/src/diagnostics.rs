//! Result data shared by the core and manager; no credentials or configuration text.
use serde::{Deserialize, Serialize};
/// Stdin payload keeps large node selections within OS command-line limits.
#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub config: String,
    pub targets: Option<Vec<String>>,
    pub list_targets: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Probe {
    pub outbound: String,
    pub direct: bool,
    pub connect_ms: Option<u64>,
    pub elapsed_ms: u64,
    pub ip: Option<std::net::IpAddr>,
    pub error: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub url: String,
    pub running_config: bool,
    pub probes: Vec<Probe>,
}

/// Incremental results: a slow node must not hide completed peers or TCP latency.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Started {
        targets: Vec<String>,
        url: String,
        running_config: bool,
    },
    Running {
        outbound: String,
    },
    Connected {
        outbound: String,
        connect_ms: u64,
    },
    Finished {
        probe: Probe,
    },
    Done,
}

/// First stdin line of a `--probe-serve` core: the configuration it keeps loaded.
#[derive(Debug, Serialize, Deserialize)]
pub struct ProbeServe {
    pub config: String,
}
/// One request line to a `--probe-serve` core; the reply line is a [`Probe`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeRequest {
    pub outbound: String,
    /// Also look up the exit IP; otherwise only the TCP handshake is timed.
    pub ip: bool,
}
