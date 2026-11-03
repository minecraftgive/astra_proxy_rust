use serde::{Deserialize, Serialize};
use std::collections::HashMap;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    Tuic,
    Vless,
    Vmess,
    Shadowsocks,
    Hysteria2,
    Trojan,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    pub protocol: Protocol,
    pub server: String,
    pub port: u16,
    pub uuid: Option<String>,
    pub password: Option<String>,
    pub cipher: Option<String>,
    pub encryption: Option<String>,
    pub flow: Option<String>,
    pub tls: Option<TlsOptions>,
    #[serde(default)]
    pub transport: TransportOptions,
    pub mux: Option<String>,
    pub obfs_password: Option<String>,
    pub pin_sha256: Option<String>,
    pub tuic_udp_mode: Option<String>,
    pub tuic_idle_timeout_secs: Option<u64>,
    pub congestion_control: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsOptions {
    pub server_name: String,
    pub ca_file: Option<String>,
    pub fingerprint: Option<String>,
    pub cert_pin_sha256: Option<String>,
    pub reality_public_key: Option<String>,
    pub reality_short_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TransportOptions {
    pub kind: TransportKind,
    pub path: Option<String>,
    pub host: Option<String>,
    pub service_name: Option<String>,
    pub mode: Option<String>,
    pub headers: HashMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TransportKind {
    #[default]
    Tcp,
    Ws,
    Grpc,
    HttpUpgrade,
    Xhttp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OutboundConfig {
    Native {
        tag: String,
        config: Box<NodeConfig>,
    },
    Socks5 {
        tag: String,
        server: String,
        #[serde(default)]
        username: Option<String>,
        #[serde(default)]
        password: Option<String>,
    },
    Http {
        tag: String,
        server: String,
        #[serde(default)]
        username: Option<String>,
        #[serde(default)]
        password: Option<String>,
    },
    TrojanTls {
        tag: String,
        server: String,
        sni: String,
        password: String,
    },
    VlessTls {
        tag: String,
        server: String,
        sni: String,
        uuid: String,
    },
}
impl OutboundConfig {
    pub fn tag(&self) -> &str {
        match self {
            Self::Native { tag, .. }
            | Self::Http { tag, .. }
            | Self::Socks5 { tag, .. }
            | Self::TrojanTls { tag, .. }
            | Self::VlessTls { tag, .. } => tag,
        }
    }
}
