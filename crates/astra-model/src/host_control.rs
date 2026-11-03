//! Resident host management data contract.
use crate::config::dns::Config;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    net::IpAddr,
    path::{Path, PathBuf},
};
#[derive(Clone, Serialize, Deserialize)]
pub enum MappingOperation {
    Configure {
        config: Config,
        reserved: HashSet<IpAddr>,
    },
    Allocate {
        config: Config,
        domain: String,
        ipv6: bool,
        reserved: HashSet<IpAddr>,
    },
    Restore {
        config: Config,
        ip: IpAddr,
    },
    Observe {
        config: Config,
        domain: String,
        ip: IpAddr,
        ttl: u32,
    },
}
#[derive(Clone, Serialize, Deserialize)]
pub enum Command {
    Status,
    Telemetry,
    Prepare {
        directory: PathBuf,
        rule_socket: Option<String>,
    },
    Activate {
        generation: u64,
    },
    Abort {
        generation: u64,
    },
    Pause,
    Resume,
    SelectExisting {
        generation: u64,
    },
    Reverse {
        ip: std::net::IpAddr,
        token: String,
    },
    Lookup {
        host: String,
        port: u16,
        token: String,
    },
    UdpPermit {
        token: String,
    },
    Mapping {
        token: String,
        operation: Box<MappingOperation>,
    },
    /// Read-only internal diagnostics for the resident host and live workers.
    Monitor,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct GenerationStatus {
    pub generation: u64,
    pub directory: PathBuf,
    pub pid: Option<u32>,
    pub active: bool,
    pub prepared: bool,
    pub flows: usize,
    pub rule_socket: String,
    pub failed: bool,
    pub log: PathBuf,
}
#[derive(Serialize, Deserialize)]
pub struct Status {
    pub active: u64,
    pub generations: Vec<GenerationStatus>,
    #[serde(default)]
    pub boot_id: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub binary: Option<PathBuf>,
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub busy: bool,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

pub fn endpoint(control: &Path) -> PathBuf {
    control.with_extension("host.sock")
}
