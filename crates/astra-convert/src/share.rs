//! Node share links are credentials: never include the original URI in errors.
use crate::{convert, Converted, Options};
mod other;
use anyhow::{ensure, Context, Result};
use serde_yaml::{Mapping, Value as Y};
use std::collections::BTreeMap;

fn direct_share(input: &str) -> bool {
    let input = input.trim();
    [
        "vless://",
        "vmess://",
        "trojan://",
        "ss://",
        "tuic://",
        "hysteria2://",
        "hy2://",
        "socks://",
        "socks5://",
    ]
    .iter()
    .any(|scheme| input.starts_with(scheme))
}
pub fn is_share(input: &str) -> bool {
    direct_share(input)
        || (input.len() <= 8 * 1024 * 1024
            && !input.contains("://")
            && other::unbase(input).is_ok_and(|v| direct_share(&v)))
}
fn decode(value: &str) -> Result<String> {
    Ok(percent_encoding::percent_decode_str(value)
        .decode_utf8()
        .context("share link contains invalid UTF-8")?
        .into_owned())
}
fn put(map: &mut Mapping, key: &str, value: impl Into<String>) {
    map.insert(Y::String(key.into()), Y::String(value.into()));
}
pub fn convert_links(input: &str, options: &Options) -> Result<Converted> {
    ensure!(input.len() <= 8 * 1024 * 1024, "share input exceeds 8 MiB");
    let decoded;
    let input = if !direct_share(input) && !input.contains("://") {
        decoded = other::unbase(input)?;
        decoded.as_str()
    } else {
        input
    };
    let mut nodes = vec![];
    for (line, value) in input.lines().enumerate() {
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        let parsed = (|| -> Result<Y> {
            if !value.starts_with("vless://") {
                return other::parse(value);
            }
            let url = url::Url::parse(value).map_err(|_| anyhow::anyhow!("invalid share link"))?;
            ensure!(url.scheme() == "vless", "only vless:// links are supported");
            ensure!(
                url.password().is_none() && matches!(url.path(), "" | "/"),
                "invalid VLESS authority or path"
            );
            let uuid = decode(url.username())?;
            uuid::Uuid::parse_str(&uuid).context("invalid VLESS UUID")?;
            let host = url
                .host_str()
                .context("missing VLESS server")?
                .trim_matches(['[', ']']);
            let port = url
                .port()
                .filter(|p| *p != 0)
                .context("missing nonzero VLESS port")?;
            let mut query = BTreeMap::new();
            for (key, value) in url.query_pairs() {
                ensure!(
                    query.insert(key.into_owned(), value.into_owned()).is_none(),
                    "duplicate share parameter"
                );
            }
            let allowed = [
                "encryption",
                "flow",
                "security",
                "sni",
                "serverName",
                "pcs",
                "fp",
                "pbk",
                "sid",
                "alpn",
                "type",
                "path",
                "host",
                "mode",
                "serviceName",
                "allowInsecure",
            ];
            ensure!(
                query.keys().all(|k| allowed.contains(&k.as_str())),
                "unsupported share parameter"
            );
            let param = |key: &str, default: &str| {
                query.get(key).cloned().unwrap_or_else(|| default.into())
            };
            ensure!(
                matches!(param("allowInsecure", "0").as_str(), "0" | "false"),
                "insecure TLS share links are not supported"
            );
            let security = param("security", "none");
            ensure!(
                matches!(security.as_str(), "none" | "tls" | "reality"),
                "unsupported VLESS security"
            );
            ensure!(
                security != "none"
                    || !query.keys().any(|k| matches!(
                        k.as_str(),
                        "sni" | "serverName" | "pcs" | "fp" | "pbk" | "sid" | "alpn"
                    )),
                "TLS parameters require TLS or REALITY"
            );
            let mut node = Mapping::new();
            put(
                &mut node,
                "name",
                match url.fragment() {
                    Some(name) if !name.is_empty() => decode(name)?,
                    _ => format!("{host}:{port}"),
                },
            );
            put(&mut node, "type", "vless");
            put(&mut node, "server", host);
            node.insert(Y::String("port".into()), Y::Number(port.into()));
            put(&mut node, "uuid", uuid);
            put(&mut node, "encryption", param("encryption", "none"));
            if let Some(flow) = query.get("flow") {
                put(&mut node, "flow", flow);
            }
            if security != "none" {
                node.insert(Y::String("tls".into()), Y::Bool(true));
                put(
                    &mut node,
                    "servername",
                    param("sni", &param("serverName", host)),
                );
                if let Some(pin) = query.get("pcs") {
                    ensure!(
                        pin.len() == 64 && pin.bytes().all(|b| b.is_ascii_hexdigit()),
                        "invalid SHA-256 certificate pin"
                    );
                    put(&mut node, "fingerprint", pin);
                }
                if let Some(fp) = query.get("fp") {
                    put(&mut node, "client-fingerprint", fp);
                }
                ensure!(
                    !query.contains_key("alpn"),
                    "custom ALPN is not supported by the converter"
                );
                if security == "reality" {
                    let mut reality = Mapping::new();
                    put(
                        &mut reality,
                        "public-key",
                        query.get("pbk").context("REALITY requires pbk")?,
                    );
                    if let Some(sid) = query.get("sid") {
                        put(&mut reality, "short-id", sid);
                    }
                    node.insert(Y::String("reality-opts".into()), Y::Mapping(reality));
                } else {
                    ensure!(
                        !query.contains_key("pbk") && !query.contains_key("sid"),
                        "REALITY parameters require security=reality"
                    );
                }
            }
            let transport = param("type", "tcp");
            let opts = match transport.as_str() {
                "tcp" => None,
                "xhttp" => Some("xhttp-opts"),
                "ws" => Some("ws-opts"),
                "grpc" => Some("grpc-opts"),
                "httpupgrade" | "http-upgrade" => Some("http-upgrade-opts"),
                _ => anyhow::bail!("unsupported VLESS transport"),
            };
            put(&mut node, "network", &transport);
            if let Some(opts) = opts {
                let mut values = Mapping::new();
                for (key, target) in [
                    ("path", "path"),
                    ("host", "host"),
                    ("mode", "mode"),
                    ("serviceName", "grpc-service-name"),
                ] {
                    if let Some(value) = query.get(key) {
                        put(&mut values, target, value);
                    }
                }
                node.insert(Y::String(opts.into()), Y::Mapping(values));
            }
            Ok(Y::Mapping(node))
        })()
        .with_context(|| format!("share link line {}", line + 1))?;
        nodes.push(parsed);
        ensure!(nodes.len() <= 4096, "too many share links");
    }
    ensure!(!nodes.is_empty(), "no share links");
    let mut root = Mapping::new();
    root.insert(Y::String("proxies".into()), Y::Sequence(nodes));
    let mut converted = convert(&serde_yaml::to_string(&root)?, options)?;
    ensure!(
        converted.nodes == input.lines().filter(|v| !v.trim().is_empty()).count(),
        "share link contains an unsupported or invalid node"
    );
    converted.warnings.clear();
    Ok(converted)
}
