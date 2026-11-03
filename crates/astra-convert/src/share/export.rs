//! Export Astra outbound nodes to common share URIs.
use anyhow::{bail, ensure, Result};
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine,
};
use std::collections::BTreeSet;
use toml::{Table, Value};
use url::Url;

fn string<'a>(table: &'a Table, key: &str) -> Result<Option<&'a str>> {
    match table.get(key) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        _ => bail!("{key} must be a string"),
    }
}
fn required<'a>(table: &'a Table, key: &str) -> Result<&'a str> {
    let value = string(table, key)?.filter(|value| !value.is_empty());
    value.ok_or_else(|| anyhow::anyhow!("{key} is required"))
}
fn uuid<'a>(table: &'a Table, key: &str) -> Result<&'a str> {
    let value = required(table, key)?;
    let parsed = uuid::Uuid::parse_str(value).map_err(|_| anyhow::anyhow!("invalid {key}"))?;
    ensure!(!parsed.is_nil(), "{key} must not be nil");
    Ok(value)
}
fn sha256_hex(value: &str, name: &str) -> Result<()> {
    ensure!(
        value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "{name} must be 64 hexadecimal characters"
    );
    Ok(())
}
fn integer(table: &Table, key: &str) -> Result<Option<i64>> {
    match table.get(key) {
        None => Ok(None),
        Some(Value::Integer(value)) => Ok(Some(*value)),
        _ => bail!("{key} must be an integer"),
    }
}
fn table<'a>(table: &'a Table, key: &str) -> Result<Option<&'a Table>> {
    match table.get(key) {
        None => Ok(None),
        Some(Value::Table(value)) => Ok(Some(value)),
        _ => bail!("{key} must be a table"),
    }
}
fn only(table: &Table, allowed: &[&str]) -> Result<()> {
    let allowed: BTreeSet<_> = allowed.iter().copied().collect();
    ensure!(
        table.keys().all(|key| allowed.contains(key.as_str())),
        "unsupported outbound option"
    );
    Ok(())
}
fn endpoint(scheme: &str, server: &str, port: i64) -> Result<Url> {
    ensure!(
        !server.is_empty() && (1..=65535).contains(&port),
        "invalid server endpoint"
    );
    let host = server.trim_matches(['[', ']']);
    ensure!(!host.is_empty(), "invalid server endpoint");
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    Url::parse(&format!("{scheme}://{host}:{port}"))
        .map_err(|_| anyhow::anyhow!("invalid server endpoint"))
}
fn validate_tag(tag: &str) -> Result<()> {
    ensure!(!tag.is_empty(), "tag is required");
    ensure!(tag.len() <= 64, "tag exceeds 64 bytes");
    ensure!(
        tag.chars().all(|character| !character.is_control()),
        "tag contains control characters"
    );
    Ok(())
}
fn finish(mut url: Url, tag: &str) -> Result<String> {
    validate_tag(tag)?;
    url.set_fragment(Some(tag));
    Ok(url.into())
}
fn credential(url: &mut Url, username: &str, password: Option<&str>) -> Result<()> {
    ensure!(!username.is_empty(), "credential is required");
    url.set_username(username)
        .map_err(|_| anyhow::anyhow!("invalid credential"))?;
    if let Some(password) = password {
        ensure!(!password.is_empty(), "credential is required");
        url.set_password(Some(password))
            .map_err(|_| anyhow::anyhow!("invalid credential"))?;
    }
    Ok(())
}
fn query(url: &mut Url, key: &str, value: &str) {
    url.query_pairs_mut().append_pair(key, value);
}

fn tls<'a>(cfg: &'a Table, protocol: &str) -> Result<Option<&'a Table>> {
    let Some(tls) = table(cfg, "tls")? else {
        return Ok(None);
    };
    only(
        tls,
        &[
            "server_name",
            "ca_file",
            "fingerprint",
            "cert_pin_sha256",
            "reality_public_key",
            "reality_short_id",
        ],
    )?;
    for key in [
        "server_name",
        "ca_file",
        "fingerprint",
        "cert_pin_sha256",
        "reality_public_key",
        "reality_short_id",
    ] {
        let _ = string(tls, key)?;
    }
    ensure!(
        string(tls, "ca_file")?.is_none(),
        "custom CA is not exportable"
    );
    ensure!(
        string(tls, "server_name")?.is_some_and(|value| !value.is_empty()),
        "TLS server name is required"
    );
    let reality_public_key = string(tls, "reality_public_key")?;
    let reality_short_id = string(tls, "reality_short_id")?;
    let cert_pin = string(tls, "cert_pin_sha256")?;
    if let Some(value) = reality_public_key {
        ensure!(!value.is_empty(), "REALITY public key is required");
    }
    if let Some(value) = reality_short_id {
        ensure!(!value.is_empty(), "REALITY short ID is required");
        ensure!(
            reality_public_key.is_some(),
            "REALITY short ID requires a public key"
        );
    }
    if let Some(value) = cert_pin {
        ensure!(!value.is_empty(), "certificate pin is required");
        ensure!(
            reality_public_key.is_none(),
            "certificate pin cannot be combined with REALITY"
        );
        sha256_hex(value, "certificate pin")?;
    }
    if matches!(protocol, "tuic" | "hysteria2") {
        ensure!(
            string(tls, "fingerprint")?.is_none(),
            "TLS fingerprint is not exportable"
        );
        ensure!(
            string(tls, "cert_pin_sha256")?.is_none(),
            "certificate pin is not exportable"
        );
        ensure!(
            string(tls, "reality_public_key")?.is_none(),
            "REALITY is not exportable"
        );
        ensure!(
            string(tls, "reality_short_id")?.is_none(),
            "REALITY is not exportable"
        );
    }
    Ok(Some(tls))
}
fn transport<'a>(cfg: &'a Table, protocol: &str) -> Result<Option<&'a Table>> {
    let Some(t) = table(cfg, "transport")? else {
        return Ok(None);
    };
    only(
        t,
        &["kind", "path", "host", "service_name", "mode", "headers"],
    )?;
    ensure!(
        table(t, "headers")?.is_none(),
        "custom transport headers are not exportable"
    );
    let kind = string(t, "kind")?.unwrap_or("tcp");
    ensure!(
        matches!(kind, "tcp" | "ws" | "grpc" | "http_upgrade" | "xhttp"),
        "unsupported transport"
    );
    ensure!(
        matches!(protocol, "vless" | "vmess" | "trojan") || kind == "tcp",
        "transport is not supported by this protocol"
    );
    if kind != "xhttp" {
        ensure!(
            string(t, "mode")?.is_none(),
            "transport mode is not exportable"
        );
    }
    if !matches!(kind, "ws" | "grpc" | "http_upgrade" | "xhttp") {
        for key in ["path", "host", "service_name"] {
            ensure!(
                string(t, key)?.is_none(),
                "transport option requires a transport"
            );
        }
    }
    if kind != "grpc" {
        ensure!(
            string(t, "service_name")?.is_none(),
            "service name requires gRPC"
        );
    }
    if protocol == "vmess" {
        ensure!(
            !(kind == "grpc" && string(t, "service_name")?.is_some()),
            "VMess gRPC service name is not exportable"
        );
        ensure!(
            !(kind == "xhttp" && string(t, "mode")?.is_some()),
            "VMess XHTTP mode is not exportable"
        );
    }
    Ok(Some(t))
}
fn add_transport(url: &mut Url, t: Option<&Table>) -> Result<()> {
    let Some(t) = t else { return Ok(()) };
    let kind = string(t, "kind")?.unwrap_or("tcp");
    let uri_kind = if kind == "http_upgrade" {
        "httpupgrade"
    } else {
        kind
    };
    if kind != "tcp" {
        query(url, "type", uri_kind);
    }
    for (from, to) in [
        ("path", "path"),
        ("host", "host"),
        ("mode", "mode"),
        ("service_name", "serviceName"),
    ] {
        if let Some(value) = string(t, from)? {
            query(url, to, value);
        }
    }
    Ok(())
}
fn add_tls(url: &mut Url, tls: Option<&Table>, protocol: &str) -> Result<()> {
    let Some(tls) = tls else { return Ok(()) };
    let reality = string(tls, "reality_public_key")?;
    if let Some(reality) = reality {
        ensure!(protocol == "vless", "REALITY is not exportable");
        query(url, "security", "reality");
        query(url, "pbk", reality);
        if let Some(value) = string(tls, "reality_short_id")? {
            query(url, "sid", value);
        }
    } else if protocol == "vless" {
        query(url, "security", "tls");
    }
    if let Some(value) = string(tls, "server_name")? {
        query(url, "sni", value);
    }
    if let Some(value) = string(tls, "fingerprint")? {
        query(url, "fp", value);
    }
    if let Some(value) = string(tls, "cert_pin_sha256")? {
        query(url, "pcs", value);
    }
    Ok(())
}

fn native(node: &Table, tag: &str) -> Result<String> {
    only(node, &["tag", "kind", "config"])?;
    let cfg = table(node, "config")?.ok_or_else(|| anyhow::anyhow!("native config is required"))?;
    only(
        cfg,
        &[
            "protocol",
            "server",
            "port",
            "uuid",
            "password",
            "cipher",
            "encryption",
            "flow",
            "tls",
            "transport",
            "mux",
            "obfs_password",
            "pin_sha256",
            "tuic_udp_mode",
            "tuic_idle_timeout_secs",
            "congestion_control",
        ],
    )?;
    let protocol = required(cfg, "protocol")?;
    ensure!(
        matches!(
            protocol,
            "vless" | "vmess" | "shadowsocks" | "trojan" | "hysteria2" | "tuic"
        ),
        "unsupported native protocol"
    );
    ensure!(string(cfg, "mux")?.is_none(), "mux is not exportable");
    let server = required(cfg, "server")?;
    let port = integer(cfg, "port")?.ok_or_else(|| anyhow::anyhow!("port is required"))?;
    let tls = tls(cfg, protocol)?;
    let transport = transport(cfg, protocol)?;
    let mut url = endpoint(
        match protocol {
            "shadowsocks" => "ss",
            "hysteria2" => "hysteria2",
            other => other,
        },
        server,
        port,
    )?;
    match protocol {
        "vless" => {
            credential(&mut url, uuid(cfg, "uuid")?, None)?;
            if let Some(value) = string(cfg, "encryption")? {
                query(&mut url, "encryption", value);
            }
            if let Some(value) = string(cfg, "flow")? {
                query(&mut url, "flow", value);
            }
            add_tls(&mut url, tls, protocol)?;
            add_transport(&mut url, transport)?;
        }
        "vmess" => {
            ensure!(
                tls.and_then(|t| string(t, "reality_public_key").ok().flatten())
                    .is_none(),
                "REALITY is not exportable"
            );
            let mut doc = serde_json::Map::new();
            doc.insert("v".into(), serde_json::Value::String("2".into()));
            doc.insert("ps".into(), serde_json::Value::String(tag.into()));
            doc.insert("add".into(), serde_json::Value::String(server.into()));
            doc.insert("port".into(), serde_json::Value::String(port.to_string()));
            doc.insert(
                "id".into(),
                serde_json::Value::String(uuid(cfg, "uuid")?.into()),
            );
            doc.insert("aid".into(), serde_json::Value::String("0".into()));
            let cipher = string(cfg, "cipher")?.unwrap_or("auto");
            ensure!(
                matches!(cipher, "auto" | "aes-128-gcm" | "chacha20-poly1305"),
                "unsupported VMess cipher"
            );
            doc.insert("scy".into(), serde_json::Value::String(cipher.into()));
            if let Some(t) = transport {
                ensure!(
                    string(t, "mode")?.is_none(),
                    "VMess transport mode is not exportable"
                );
                ensure!(
                    string(t, "service_name")?.is_none(),
                    "VMess transport service name is not exportable"
                );
            }
            let t = transport
                .map(|t| string(t, "kind"))
                .transpose()?
                .flatten()
                .unwrap_or("tcp");
            doc.insert(
                "net".into(),
                serde_json::Value::String(
                    if t == "http_upgrade" {
                        "httpupgrade"
                    } else {
                        t
                    }
                    .into(),
                ),
            );
            if let Some(t) = transport {
                for (from, to) in [("path", "path"), ("host", "host")] {
                    if let Some(v) = string(t, from)? {
                        doc.insert(to.into(), serde_json::Value::String(v.into()));
                    }
                }
            }
            if tls.is_some() {
                doc.insert("tls".into(), serde_json::Value::String("tls".into()));
                if let Some(t) = tls {
                    for (from, to) in [
                        ("server_name", "sni"),
                        ("fingerprint", "fp"),
                        ("cert_pin_sha256", "pcs"),
                    ] {
                        if let Some(v) = string(t, from)? {
                            doc.insert(to.into(), serde_json::Value::String(v.into()));
                        }
                    }
                }
            }
            return Ok(format!(
                "vmess://{}",
                STANDARD.encode(serde_json::Value::Object(doc).to_string())
            ));
        }
        "shadowsocks" => {
            ensure!(
                tls.is_none() && transport.is_none(),
                "TLS or transport is not supported"
            );
            credential(
                &mut url,
                &URL_SAFE_NO_PAD.encode(format!(
                    "{}:{}",
                    required(cfg, "cipher")?,
                    required(cfg, "password")?
                )),
                None,
            )?;
        }
        "trojan" => {
            ensure!(tls.is_some(), "Trojan requires TLS");
            ensure!(
                tls.and_then(|t| string(t, "reality_public_key").ok().flatten())
                    .is_none(),
                "REALITY is not exportable"
            );
            credential(&mut url, required(cfg, "password")?, None)?;
            query(&mut url, "security", "tls");
            add_tls(&mut url, tls, protocol)?;
            add_transport(&mut url, transport)?;
        }
        "hysteria2" => {
            ensure!(transport.is_none(), "transport is not supported");
            credential(&mut url, required(cfg, "password")?, None)?;
            if let Some(tls) = tls {
                ensure!(
                    string(tls, "server_name")?.is_some(),
                    "TLS server name is required"
                );
                query(&mut url, "sni", string(tls, "server_name")?.unwrap());
            }
            if let Some(value) = string(cfg, "obfs_password")? {
                ensure!(!value.is_empty(), "obfs password is required");
                query(&mut url, "obfs", "salamander");
                query(&mut url, "obfs-password", value);
            }
            if let Some(value) = string(cfg, "pin_sha256")? {
                ensure!(!value.is_empty(), "Hysteria2 certificate pin is required");
                ensure!(
                    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
                    "Hysteria2 certificate pin must be 64 hexadecimal characters"
                );
                query(&mut url, "pinSHA256", value);
            }
        }
        "tuic" => {
            ensure!(transport.is_none(), "transport is not supported");
            ensure!(tls.is_some(), "TUIC requires TLS");
            ensure!(string(cfg, "cipher")?.is_none(), "cipher is not supported");
            ensure!(
                integer(cfg, "tuic_idle_timeout_secs")?.is_none(),
                "TUIC idle timeout is not exportable"
            );
            let user = format!("{}:{}", uuid(cfg, "uuid")?, required(cfg, "password")?);
            credential(&mut url, &user, None)?;
            if let Some(tls) = tls {
                ensure!(
                    string(tls, "server_name")?.is_some(),
                    "TLS server name is required"
                );
                query(&mut url, "sni", string(tls, "server_name")?.unwrap());
            }
            if let Some(value) = string(cfg, "congestion_control")? {
                ensure!(
                    matches!(value, "cubic" | "bbr" | "new_reno"),
                    "unsupported TUIC congestion control"
                );
                query(&mut url, "congestion_control", value);
            }
            if let Some(value) = string(cfg, "tuic_udp_mode")? {
                ensure!(
                    matches!(value, "native" | "quic"),
                    "unsupported TUIC UDP mode"
                );
                query(&mut url, "udp_relay_mode", value);
            }
        }
        _ => unreachable!(),
    }
    finish(url, tag)
}

fn legacy(node: &Table, tag: &str, kind: &str) -> Result<String> {
    let allowed = if kind == "socks5" {
        &["tag", "kind", "server", "username", "password"][..]
    } else {
        &["tag", "kind", "server", "sni", "uuid", "password"][..]
    };
    only(node, allowed)?;
    let server = required(node, "server")?;
    let parsed = Url::parse(&format!("socks5://{server}"))
        .map_err(|_| anyhow::anyhow!("invalid server endpoint"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("invalid server endpoint"))?;
    let port = parsed
        .port()
        .ok_or_else(|| anyhow::anyhow!("invalid server endpoint"))?;
    let scheme = if kind == "socks5" {
        "socks5"
    } else {
        kind.strip_suffix("_tls").unwrap()
    };
    let mut url = endpoint(scheme, host, port.into())?;
    match kind {
        "socks5" => {
            let user = string(node, "username")?;
            let pass = string(node, "password")?;
            ensure!(
                user.is_none() == pass.is_none(),
                "username and password must be paired"
            );
            if let (Some(u), Some(p)) = (user, pass) {
                credential(&mut url, u, Some(p))?;
            }
        }
        "vless_tls" => {
            credential(&mut url, uuid(node, "uuid")?, None)?;
            query(&mut url, "security", "tls");
            query(&mut url, "sni", required(node, "sni")?);
        }
        "trojan_tls" => {
            credential(&mut url, required(node, "password")?, None)?;
            query(&mut url, "security", "tls");
            query(&mut url, "sni", required(node, "sni")?);
        }
        _ => unreachable!(),
    }
    finish(url, tag)
}

/// Export one TOML `outbounds` node as a common share URI.
pub fn export_node(node: &Value) -> Result<String> {
    let node = node
        .as_table()
        .ok_or_else(|| anyhow::anyhow!("outbound node must be a table"))?;
    let tag = required(node, "tag")?;
    validate_tag(tag)?;
    match required(node, "kind")? {
        "native" => native(node, tag),
        "socks5" => legacy(node, tag, "socks5"),
        "vless_tls" => legacy(node, tag, "vless_tls"),
        "trojan_tls" => legacy(node, tag, "trojan_tls"),
        "http" => bail!("HTTP outbounds cannot be exported"),
        _ => bail!("unsupported outbound kind"),
    }
}
