use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
};

pub use astra_protocols::accept::Protocol;
pub use astra_protocols::accept::User;
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
pub struct Prepared {
    pub config: Config,
    pub tls: Vec<Option<Arc<tokio_rustls::rustls::ServerConfig>>>,
}
pub fn load(path: &Path) -> Result<Prepared> {
    ensure!(
        std::fs::metadata(path)?.len() <= 1024 * 1024,
        "server configuration exceeds 1 MiB"
    );
    let text = std::fs::read_to_string(path)?;
    ensure!(
        text.len() <= 1024 * 1024,
        "server configuration exceeds 1 MiB"
    );
    let mut config: Config = toml::from_str(&text)?;
    ensure!(
        (1..=65536).contains(&config.max_connections)
            && (1..=65536).contains(&config.max_udp_flows),
        "invalid session limits"
    );
    ensure!(
        config.handshake_timeout_secs > 0
            && config.connect_timeout_secs > 0
            && config.idle_timeout_secs > 0
            && config.shutdown_timeout_secs <= 300,
        "invalid timeouts"
    );
    ensure!(
        !config.listeners.is_empty() && config.listeners.len() <= 64,
        "configure 1..64 server listeners"
    );
    let mut tags = HashSet::new();
    let mut addresses = HashSet::new();
    let mut tls = vec![];
    for listener in &mut config.listeners {
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
            let cipher: shadowsocks::crypto::CipherKind = listener
                .cipher
                .as_deref()
                .context("Shadowsocks requires cipher")?
                .parse()
                .map_err(|_| anyhow::anyhow!("unknown Shadowsocks cipher"))?;
            ensure!(
                cipher.is_aead() || cipher.is_aead_2022(),
                "only authenticated Shadowsocks AEAD/2022 ciphers are accepted"
            );
            let password = listener
                .password
                .as_deref()
                .filter(|v| !v.is_empty())
                .context("Shadowsocks password is required")?;
            shadowsocks::config::ServerConfig::new(listener.listen, password, cipher)
                .map_err(|e| anyhow::anyhow!("Shadowsocks key: {e}"))?;
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
        if let Some(reality) = &listener.reality {
            crate::security::RealityState::validate(reality)?;
        }
        if let Some(flow) = &listener.flow {
            ensure!(
                flow == "xtls-rprx-vision"
                    && listener.protocol == Protocol::Vless
                    && (listener.tls.is_some() || listener.reality.is_some()),
                "Vision requires VLESS over TLS/REALITY"
            );
        }
        if let Some(encryption) = &listener.decryption {
            ensure!(
                listener.protocol == Protocol::Vless,
                "VLESS Encryption requires VLESS"
            );
            crate::encryption::prepare(encryption)?;
        }
        ensure!(
            !listener.multiplex
                || matches!(
                    listener.protocol,
                    Protocol::Vless | Protocol::Vmess | Protocol::Trojan | Protocol::Shadowsocks
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
        let compiled = if let Some(options) = &mut listener.tls {
            let base = path.parent().context("config has no directory")?;
            if options.certificate.is_relative() {
                options.certificate = base.join(&options.certificate);
            }
            if options.private_key.is_relative() {
                options.private_key = base.join(&options.private_key);
            }
            let certs = rustls_pemfile::certs(&mut std::io::BufReader::new(std::fs::File::open(
                &options.certificate,
            )?))
            .collect::<std::io::Result<Vec<_>>>()?;
            let key = rustls_pemfile::private_key(&mut std::io::BufReader::new(
                std::fs::File::open(&options.private_key)?,
            ))?
            .context("no private key in PEM")?;
            let mut tls = tokio_rustls::rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(certs, key)?;
            tls.alpn_protocols = if listener.protocol.quic() {
                vec![b"h3".to_vec()]
            } else if matches!(listener.transport, Transport::Grpc | Transport::Xhttp) {
                vec![b"h2".to_vec()]
            } else {
                vec![b"http/1.1".to_vec()]
            };
            Some(Arc::new(tls))
        } else {
            None
        };
        tls.push(compiled);
    }
    Ok(Prepared { config, tls })
}

#[cfg(test)]
mod tests {
    use super::*;
    const VALID:&str="[[listeners]]\ntag='socks'\nprotocol='socks5'\nlisten='127.0.0.1:1080'\n[[listeners.users]]\nname='user'\npassword='secret'\n";
    fn check(text: &str) -> Result<Prepared> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("server.toml");
        std::fs::write(&path, text)?;
        load(&path)
    }
    #[test]
    fn rejects_unauthenticated_ambiguous_and_public_plaintext_configs() {
        assert!(check(VALID).is_ok());
        assert!(check(&VALID.replace("127.0.0.1", "0.0.0.0")).is_err());
        assert!(check(&format!(
            "allow_private_targets=true\n{}",
            VALID.replace("listen=", "typo=")
        ))
        .is_err());
        assert!(check(VALID.split("[[listeners.users]]").next().unwrap()).is_err());
        assert!(check(&format!("{VALID}\n{VALID}")).is_err());
        assert!(check(&VALID.replace("password='secret'", "password=''")).is_err());
    }
}
