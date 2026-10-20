use super::{decode, put};
use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose as b64, Engine};
use serde_yaml::{Mapping, Value as Y};
use std::collections::BTreeMap;

pub(super) fn unbase(value: &str) -> Result<String> {
    let value: String = value.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = [
        b64::STANDARD,
        b64::STANDARD_NO_PAD,
        b64::URL_SAFE,
        b64::URL_SAFE_NO_PAD,
    ]
    .iter()
    .find_map(|engine| engine.decode(&value).ok())
    .context("invalid base64 share data")?;
    String::from_utf8(bytes).context("share data must be UTF-8")
}
fn endpoint(url: &url::Url, default: Option<u16>) -> Result<Mapping> {
    ensure!(matches!(url.path(), "" | "/"), "unsupported share path");
    let host = url
        .host_str()
        .context("missing server")?
        .trim_matches(['[', ']']);
    let port = url
        .port()
        .or(default)
        .filter(|p| *p != 0)
        .context("missing nonzero port")?;
    let mut node = Mapping::new();
    put(
        &mut node,
        "name",
        match url.fragment() {
            Some(name) if !name.is_empty() => decode(name)?,
            _ => format!("{host}:{port}"),
        },
    );
    put(&mut node, "server", host);
    node.insert("port".into(), Y::Number(port.into()));
    Ok(node)
}
fn query(url: &url::Url, allowed: &[&str]) -> Result<BTreeMap<String, String>> {
    let mut result = BTreeMap::new();
    for (key, value) in url.query_pairs() {
        ensure!(
            allowed.contains(&key.as_ref()),
            "unsupported share parameter"
        );
        ensure!(
            result
                .insert(key.into_owned(), value.into_owned())
                .is_none(),
            "duplicate share parameter"
        );
    }
    for key in ["insecure", "allowInsecure", "allow_insecure"] {
        ensure!(
            result
                .get(key)
                .is_none_or(|v| matches!(v.as_str(), "" | "0" | "false")),
            "insecure TLS is not supported"
        );
    }
    ensure!(
        result.get("alpn").is_none_or(|v| v.is_empty()),
        "custom ALPN is not supported by this core"
    );
    Ok(result)
}
fn copy(node: &mut Mapping, q: &BTreeMap<String, String>, source: &str, target: &str) {
    if let Some(value) = q.get(source).filter(|v| !v.is_empty()) {
        put(node, target, value);
    }
}
fn auth(url: &url::Url) -> Result<String> {
    let mut value = decode(url.username())?;
    if let Some(password) = url.password() {
        value.push(':');
        value.push_str(&decode(password)?);
    }
    ensure!(!value.is_empty(), "missing authentication");
    Ok(value)
}
fn transport(node: &mut Mapping, q: &BTreeMap<String, String>) -> Result<()> {
    let kind = q.get("type").map(String::as_str).unwrap_or("tcp");
    let key = match kind {
        "tcp" => None,
        "ws" => Some("ws-opts"),
        "grpc" => Some("grpc-opts"),
        "xhttp" => Some("xhttp-opts"),
        "httpupgrade" | "http-upgrade" => Some("http-upgrade-opts"),
        _ => anyhow::bail!("unsupported share transport"),
    };
    put(node, "network", kind);
    if let Some(key) = key {
        let mut opts = Mapping::new();
        for (from, to) in [
            ("path", "path"),
            ("host", "host"),
            ("mode", "mode"),
            ("serviceName", "grpc-service-name"),
        ] {
            copy(&mut opts, q, from, to);
        }
        node.insert(key.into(), Y::Mapping(opts));
    } else {
        ensure!(
            !q.keys()
                .any(|k| matches!(k.as_str(), "path" | "host" | "mode" | "serviceName")),
            "transport parameters require a matching transport"
        );
    }
    Ok(())
}
pub(super) fn parse(value: &str) -> Result<Y> {
    if let Some(encoded) = value.strip_prefix("vmess://") {
        return vmess(encoded);
    }
    if let Some(encoded) = value.strip_prefix("ss://") {
        if !encoded.split('#').next().unwrap_or("").contains('@') {
            let (data, name) = encoded.split_once('#').unwrap_or((encoded, ""));
            let decoded = unbase(data)?;
            ensure!(
                decoded.contains('@') && !decoded.contains(['?', '#', '\r', '\n']),
                "invalid legacy Shadowsocks share"
            );
            return parse(&format!("ss://{decoded}#{name}"));
        }
    }
    let url = url::Url::parse(value).map_err(|_| anyhow::anyhow!("invalid share link"))?;
    let kind = match url.scheme() {
        "trojan" => "trojan",
        "ss" => "ss",
        "tuic" => "tuic",
        "hy2" | "hysteria2" => "hysteria2",
        "socks" | "socks5" => "socks5",
        _ => anyhow::bail!("unsupported share protocol"),
    };
    let mut node = endpoint(&url, (kind == "hysteria2").then_some(443))?;
    put(&mut node, "type", kind);
    match kind {
        "ss" => {
            let q = query(&url, &["plugin"])?;
            ensure!(
                q.get("plugin").is_none_or(|v| v.is_empty()),
                "Shadowsocks plugins are not supported"
            );
            let raw = if url.password().is_some() {
                auth(&url)?
            } else {
                unbase(&decode(url.username())?)?
            };
            let (cipher, password) = raw
                .split_once(':')
                .context("Shadowsocks requires method:password")?;
            ensure!(
                !cipher.is_empty() && !password.is_empty(),
                "missing Shadowsocks credentials"
            );
            put(&mut node, "cipher", cipher);
            put(&mut node, "password", password);
        }
        "socks5" => {
            query(&url, &[])?;
            if !url.username().is_empty() {
                let raw = if url.password().is_some() {
                    auth(&url)?
                } else {
                    unbase(&decode(url.username())?)?
                };
                let (username, password) = raw
                    .split_once(':')
                    .context("SOCKS requires username:password")?;
                put(&mut node, "username", username);
                put(&mut node, "password", password);
            }
        }
        "trojan" => {
            let q = query(
                &url,
                &[
                    "security",
                    "sni",
                    "peer",
                    "fp",
                    "pcs",
                    "type",
                    "path",
                    "host",
                    "mode",
                    "serviceName",
                    "alpn",
                    "insecure",
                    "allowInsecure",
                ],
            )?;
            ensure!(
                q.get("security")
                    .is_none_or(|v| matches!(v.as_str(), "tls" | "")),
                "Trojan requires TLS"
            );
            put(&mut node, "password", auth(&url)?);
            copy(&mut node, &q, "peer", "sni");
            copy(&mut node, &q, "sni", "sni");
            copy(&mut node, &q, "fp", "client-fingerprint");
            copy(&mut node, &q, "pcs", "fingerprint");
            transport(&mut node, &q)?;
        }
        "hysteria2" => {
            let q = query(
                &url,
                &[
                    "sni",
                    "insecure",
                    "obfs",
                    "obfs-password",
                    "pinSHA256",
                    "alpn",
                ],
            )?;
            put(&mut node, "password", auth(&url)?);
            for (from, to) in [
                ("sni", "sni"),
                ("obfs", "obfs"),
                ("obfs-password", "obfs-password"),
                ("pinSHA256", "fingerprint"),
            ] {
                copy(&mut node, &q, from, to);
            }
            ensure!(
                !q.contains_key("obfs-password")
                    || q.get("obfs").is_some_and(|v| v == "salamander"),
                "obfs-password requires salamander"
            );
        }
        "tuic" => {
            let q = query(
                &url,
                &[
                    "sni",
                    "congestion_control",
                    "udp_relay_mode",
                    "alpn",
                    "allow_insecure",
                    "insecure",
                    "allowInsecure",
                ],
            )?;
            let raw = auth(&url)?;
            let (uuid, password) = raw.split_once(':').context("TUIC requires UUID:password")?;
            uuid::Uuid::parse_str(uuid).context("invalid TUIC UUID")?;
            ensure!(!password.is_empty(), "TUIC requires a password");
            put(&mut node, "uuid", uuid);
            put(&mut node, "password", password);
            for (from, to) in [
                ("sni", "sni"),
                ("congestion_control", "congestion-controller"),
                ("udp_relay_mode", "udp-relay-mode"),
            ] {
                copy(&mut node, &q, from, to);
            }
        }
        _ => unreachable!(),
    }
    Ok(Y::Mapping(node))
}
fn vmess(value: &str) -> Result<Y> {
    let text = unbase(value)?;
    let doc: serde_json::Value =
        serde_json::from_str(&text).map_err(|_| anyhow::anyhow!("invalid VMess JSON"))?;
    ensure!(doc.is_object(), "VMess JSON must be an object");
    let get = |key: &str| -> String {
        match &doc[key] {
            serde_json::Value::String(v) => v.clone(),
            serde_json::Value::Number(v) => v.to_string(),
            serde_json::Value::Bool(v) => v.to_string(),
            _ => String::new(),
        }
    };
    ensure!(
        matches!(get("aid").as_str(), "" | "0"),
        "legacy VMess alterId is not supported"
    );
    ensure!(
        matches!(get("type").as_str(), "" | "none"),
        "VMess header disguise is not supported"
    );
    ensure!(
        get("alpn").is_empty() && get("vcn").is_empty(),
        "custom VMess TLS ALPN or verification name is not supported"
    );
    ensure!(
        matches!(get("insecure").as_str(), "" | "0" | "false"),
        "insecure TLS is not supported"
    );
    let port: u16 = get("port").parse().context("invalid VMess port")?;
    ensure!(port > 0 && !get("add").is_empty(), "invalid VMess endpoint");
    uuid::Uuid::parse_str(&get("id")).context("invalid VMess UUID")?;
    let mut node = Mapping::new();
    put(
        &mut node,
        "name",
        if get("ps").is_empty() {
            format!("{}:{port}", get("add"))
        } else {
            get("ps")
        },
    );
    put(&mut node, "type", "vmess");
    put(&mut node, "server", get("add"));
    node.insert("port".into(), Y::Number(port.into()));
    put(&mut node, "uuid", get("id"));
    put(
        &mut node,
        "cipher",
        if get("scy").is_empty() {
            "auto".into()
        } else {
            get("scy")
        },
    );
    ensure!(
        matches!(get("tls").as_str(), "" | "none" | "tls"),
        "unsupported VMess security"
    );
    if get("tls") == "tls" {
        node.insert("tls".into(), Y::Bool(true));
        for (from, to) in [
            ("sni", "sni"),
            ("fp", "client-fingerprint"),
            ("pcs", "fingerprint"),
        ] {
            if !get(from).is_empty() {
                put(&mut node, to, get(from));
            }
        }
    }
    let mut q = BTreeMap::new();
    let net = if get("net").is_empty() {
        "tcp".into()
    } else {
        get("net")
    };
    q.insert("type".into(), net.clone());
    if net != "tcp" {
        for key in ["host", "path"] {
            if !get(key).is_empty() {
                q.insert(
                    if net == "grpc" && key == "path" {
                        "serviceName".into()
                    } else {
                        key.into()
                    },
                    get(key),
                );
            }
        }
    }
    transport(&mut node, &q)?;
    Ok(Y::Mapping(node))
}
