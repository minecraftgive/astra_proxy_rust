use serde::{Deserialize, Serialize};
use std::{net::SocketAddr, path::PathBuf};
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct User {
    pub name: String,
    pub uuid: Option<uuid::Uuid>,
    pub password: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    Socks5,
    Http,
    Shadowsocks,
    Vmess,
    Vless,
    Trojan,
    Hysteria2,
    Tuic,
}
impl Protocol {
    pub fn quic(self) -> bool {
        matches!(self, Self::Hysteria2 | Self::Tuic)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tls {
    pub certificate: PathBuf,
    pub private_key: PathBuf,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reality {
    pub private_key: String,
    pub server_names: Vec<String>,
    pub short_ids: Vec<String>,
    pub target: String,
    #[serde(default = "reality_window")]
    pub max_time_diff_secs: u64,
}
fn reality_window() -> u64 {
    60
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Decryption {
    pub private_keys: Vec<String>,
    #[serde(default = "native")]
    pub mode: String,
    #[serde(default)]
    pub ticket_lifetime_secs: u32,
}
fn native() -> String {
    "native".into()
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    #[default]
    Tcp,
    Ws,
    Grpc,
    HttpUpgrade,
    Xhttp,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Listener {
    pub tag: String,
    pub protocol: Protocol,
    pub listen: SocketAddr,
    #[serde(default)]
    pub users: Vec<User>,
    pub password: Option<String>,
    pub cipher: Option<String>,
    pub tls: Option<Tls>,
    pub reality: Option<Reality>,
    pub flow: Option<String>,
    pub decryption: Option<Decryption>,
    #[serde(default)]
    pub multiplex: bool,
    pub obfs_password: Option<String>,
    #[serde(default)]
    pub transport: Transport,
    pub path: Option<String>,
    pub service_name: Option<String>,
    #[serde(default)]
    pub allow_plaintext: bool,
    #[serde(default = "yes")]
    pub udp: bool,
}
fn yes() -> bool {
    true
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub auto_start: bool,
    pub max_connections: usize,
    pub max_udp_flows: usize,
    pub handshake_timeout_secs: u64,
    pub connect_timeout_secs: u64,
    pub idle_timeout_secs: u64,
    pub shutdown_timeout_secs: u64,
    pub allow_private_targets: bool,
    pub deny_cidrs: Vec<ipnet::IpNet>,
    pub listeners: Vec<Listener>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            auto_start: false,
            max_connections: 1024,
            max_udp_flows: 512,
            handshake_timeout_secs: 10,
            connect_timeout_secs: 8,
            idle_timeout_secs: 120,
            shutdown_timeout_secs: 10,
            allow_private_targets: false,
            deny_cidrs: vec![],
            listeners: vec![],
        }
    }
}

impl Config {
    /// Field, authentication and cross-field checks before resource preparation.
    pub fn validate_structure(&self) -> anyhow::Result<()> {
        use anyhow::{ensure, Context};
        use std::collections::HashSet;
        ensure!(
            (1..=65536).contains(&self.max_connections)
                && (1..=65536).contains(&self.max_udp_flows),
            "invalid session limits"
        );
        ensure!(
            self.handshake_timeout_secs > 0
                && self.connect_timeout_secs > 0
                && self.idle_timeout_secs > 0
                && self.shutdown_timeout_secs <= 300,
            "invalid timeouts"
        );
        ensure!(
            !self.listeners.is_empty() && self.listeners.len() <= 64,
            "configure 1..64 server listeners"
        );
        let mut tags = HashSet::new();
        let mut addresses = HashSet::new();
        for listener in &self.listeners {
            ensure!(
                !listener.tag.is_empty() && tags.insert(listener.tag.clone()),
                "duplicate/empty listener tag"
            );
            ensure!(
                listener.listen.port() != 0
                    && addresses.insert((listener.protocol.quic(), listener.listen)),
                "duplicate listener address or zero port"
            );
            ensure!(
                listener.users.len() <= 256,
                "too many users on {}",
                listener.tag
            );
            if listener.protocol == Protocol::Shadowsocks {
                listener
                    .cipher
                    .as_deref()
                    .filter(|v| !v.is_empty())
                    .context("Shadowsocks requires cipher")?;
                listener
                    .password
                    .as_deref()
                    .filter(|v| !v.is_empty())
                    .context("Shadowsocks password is required")?;
            } else {
                ensure!(
                    !listener.users.is_empty(),
                    "{} requires authenticated users",
                    listener.tag
                );
                let mut names = HashSet::new();
                let mut ids = HashSet::new();
                let mut passwords = HashSet::new();
                for user in &listener.users {
                    ensure!(
                        !user.name.is_empty() && names.insert(user.name.clone()),
                        "duplicate/empty user name"
                    );
                    if matches!(
                        listener.protocol,
                        Protocol::Vless | Protocol::Vmess | Protocol::Tuic
                    ) {
                        ensure!(
                            user.uuid.is_some_and(|v| !v.is_nil() && ids.insert(v)),
                            "missing/duplicate/nil UUID"
                        );
                    }
                    if !matches!(listener.protocol, Protocol::Vless | Protocol::Vmess) {
                        let password = user
                            .password
                            .as_ref()
                            .filter(|v| !v.is_empty() && v.len() <= 255)
                            .context("user password must have 1..255 bytes")?;
                        ensure!(
                            passwords.insert(password.clone()),
                            "user passwords must be distinct"
                        );
                    }
                }
            }
            ensure!(
                !listener.protocol.quic() || listener.tls.is_some(),
                "QUIC listeners require TLS certificate and key"
            );
            ensure!(
                listener.reality.is_none()
                    || (listener.tls.is_none() && listener.protocol == Protocol::Vless),
                "REALITY requires VLESS and replaces TLS configuration"
            );
            if let Some(flow) = &listener.flow {
                ensure!(
                    flow == "xtls-rprx-vision"
                        && listener.protocol == Protocol::Vless
                        && (listener.tls.is_some() || listener.reality.is_some()),
                    "Vision requires VLESS over TLS/REALITY"
                );
            }
            if listener.decryption.is_some() {
                ensure!(
                    listener.protocol == Protocol::Vless,
                    "VLESS Encryption requires VLESS"
                );
            }
            ensure!(
                !listener.multiplex
                    || matches!(
                        listener.protocol,
                        Protocol::Vless
                            | Protocol::Vmess
                            | Protocol::Trojan
                            | Protocol::Shadowsocks
                    ),
                "multiplex is unavailable on this protocol"
            );
            ensure!(
                listener.obfs_password.is_none() || listener.protocol == Protocol::Hysteria2,
                "Salamander requires Hysteria2"
            );
            if let Some(password) = &listener.obfs_password {
                ensure!(
                    !password.is_empty() && password.len() <= 255,
                    "invalid obfuscation password"
                );
            }
            ensure!(
                listener.protocol != Protocol::Trojan || listener.tls.is_some(),
                "Trojan requires TLS"
            );
            ensure!(
                listener.tls.is_some()
                    || listener.reality.is_some()
                    || listener.protocol == Protocol::Shadowsocks
                    || listener.allow_plaintext
                    || (listener.listen.ip().is_loopback()
                        && matches!(listener.protocol, Protocol::Socks5 | Protocol::Http)),
                "non-TLS listener requires explicit allow_plaintext=true"
            );
            ensure!(
                !listener.protocol.quic() || listener.transport == Transport::Tcp,
                "QUIC protocols do not accept TCP transports"
            );
            ensure!(
                listener.protocol != Protocol::Shadowsocks || listener.transport == Transport::Tcp,
                "Shadowsocks uses native TCP/UDP transport"
            );
        }
        Ok(())
    }
}
