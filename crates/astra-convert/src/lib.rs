//! Offline mihomo YAML -> Astra TOML conversion, independent of the proxy core.
use anyhow::{bail, ensure, Context, Result};
use serde::Serialize;
use serde_yaml::Value as Y;
use std::collections::{BTreeMap, HashSet};
use toml::{Table, Value as T};

#[derive(Default)]
pub struct Options {
    pub selections: BTreeMap<String, String>,
    pub rule_socket: Option<String>,
}

pub struct Converted {
    pub proxy: String,
    pub rules: String,
    pub warnings: Vec<String>,
    pub nodes: usize,
    pub rule_count: usize,
}

#[derive(Serialize)]
struct Rule {
    kind: String,
    value: String,
    action: String,
}
#[derive(Serialize)]
struct Rules {
    socket: String,
    default_action: String,
    rules: Vec<Rule>,
}

fn get<'a>(v: &'a Y, key: &str) -> &'a Y {
    v.get(key).unwrap_or(&Y::Null)
}
fn scalar(v: &Y) -> Option<String> {
    match v {
        Y::String(s) => Some(s.clone()),
        Y::Number(n) => Some(n.to_string()),
        _ => None,
    }
}
fn s(v: &Y, key: &str) -> Option<String> {
    scalar(get(v, key))
}
fn required(v: &Y, key: &str) -> Result<String> {
    s(v, key)
        .filter(|s| !s.is_empty())
        .with_context(|| format!("缺少 {key}"))
}
fn seq<'a>(v: &'a Y, key: &str) -> &'a [Y] {
    get(v, key).as_sequence().map(Vec::as_slice).unwrap_or(&[])
}
fn yes(v: &Y, key: &str) -> bool {
    get(v, key).as_bool() == Some(true)
}
fn put(t: &mut Table, key: &str, value: impl Into<String>) {
    t.insert(key.into(), T::String(value.into()));
}
fn copy(t: &mut Table, v: &Y, from: &str, to: &str) {
    if let Some(value) = s(v, from) {
        put(t, to, value);
    }
}
fn port(v: &Y, key: &str) -> Result<Option<u16>> {
    let Some(n) = s(v, key) else {
        return Ok(None);
    };
    let n: u16 = n
        .parse()
        .with_context(|| format!("{key} 必须为 0..65535"))?;
    Ok((n != 0).then_some(n))
}
fn address(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn node(v: &Y, warnings: &mut Vec<String>) -> Result<Table> {
    let tag = required(v, "name")?;
    // Rule Core's action frame permits at most 64 bytes of tag data.
    ensure!(tag.len() <= 64, "节点名称超过 64 字节");
    let kind = required(v, "type")?;
    let server = required(v, "server")?;
    let port = port(v, "port")?.context("缺少非零 port")?;
    let mut out = Table::new();
    put(&mut out, "tag", &tag);
    if matches!(kind.as_str(), "socks5" | "http") {
        ensure!(!yes(v, "tls"), "上游 SOCKS/HTTP TLS 不支持");
        put(&mut out, "kind", kind);
        put(&mut out, "server", address(&server, port));
        let user = s(v, "username");
        let pass = s(v, "password");
        ensure!(
            user.is_some() == pass.is_some(),
            "username/password 必须同时存在"
        );
        if let (Some(user), Some(pass)) = (user, pass) {
            ensure!(
                !user.is_empty() && user.len() <= 255 && !pass.is_empty() && pass.len() <= 255,
                "无效认证长度"
            );
            put(&mut out, "username", user);
            put(&mut out, "password", pass);
        }
        return Ok(out);
    }
    let protocol = match kind.as_str() {
        "ss" => "shadowsocks",
        "vless" | "vmess" | "trojan" | "tuic" | "hysteria2" => &kind,
        _ => bail!("不支持的协议 {kind}"),
    };
    let mut cfg = Table::new();
    put(&mut out, "kind", "native");
    put(&mut cfg, "protocol", protocol);
    put(&mut cfg, "server", &server);
    cfg.insert("port".into(), T::Integer(port.into()));
    if matches!(protocol, "vless" | "vmess" | "tuic") {
        let uuid = required(v, "uuid")?;
        uuid::Uuid::parse_str(&uuid).context("无效 uuid")?;
        put(&mut cfg, "uuid", uuid);
    }
    if matches!(protocol, "trojan" | "shadowsocks" | "tuic" | "hysteria2") {
        put(&mut cfg, "password", required(v, "password")?);
    }
    if protocol == "shadowsocks" {
        ensure!(
            get(v, "plugin").is_null() || s(v, "plugin").as_deref() == Some(""),
            "SS plugin 不支持"
        );
        put(&mut cfg, "cipher", required(v, "cipher")?);
    }
    if protocol == "vmess" {
        ensure!(
            s(v, "alterId").is_none_or(|s| s == "0"),
            "旧版 VMess alterId 不支持"
        );
        let cipher = s(v, "cipher").unwrap_or_else(|| "auto".into());
        ensure!(
            matches!(
                cipher.as_str(),
                "auto" | "aes-128-gcm" | "chacha20-poly1305"
            ),
            "不支持的 VMess cipher"
        );
        put(&mut cfg, "cipher", cipher);
    }
    if protocol == "vless" {
        copy(&mut cfg, v, "flow", "flow");
        copy(&mut cfg, v, "encryption", "encryption");
    }
    if protocol == "tuic" {
        if let Some(mode) = s(v, "udp-relay-mode") {
            ensure!(
                matches!(mode.as_str(), "native" | "quic"),
                "无效 TUIC UDP 模式"
            );
            put(&mut cfg, "tuic_udp_mode", mode);
        }
        if let Some(cc) = s(v, "congestion-controller") {
            ensure!(
                matches!(cc.as_str(), "cubic" | "bbr" | "new_reno"),
                "无效拥塞控制算法"
            );
            put(&mut cfg, "congestion_control", cc);
        }
    }
    if protocol == "hysteria2" {
        if let Some(obfs) = s(v, "obfs").filter(|x| !x.is_empty()) {
            ensure!(obfs == "salamander", "不支持的 Hysteria2 obfs");
            put(&mut cfg, "obfs_password", required(v, "obfs-password")?);
        }
        copy(&mut cfg, v, "fingerprint", "pin_sha256");
    }
    let reality = get(v, "reality-opts");
    let tls =
        yes(v, "tls") || !reality.is_null() || matches!(protocol, "trojan" | "tuic" | "hysteria2");
    if tls {
        let mut t = Table::new();
        put(
            &mut t,
            "server_name",
            s(v, "servername")
                .or_else(|| s(v, "sni"))
                .filter(|x| !x.is_empty())
                .unwrap_or(server),
        );
        if !matches!(protocol, "tuic" | "hysteria2") {
            copy(&mut t, v, "client-fingerprint", "fingerprint");
            copy(&mut t, v, "fingerprint", "cert_pin_sha256");
            if !reality.is_null() {
                put(
                    &mut t,
                    "reality_public_key",
                    required(reality, "public-key")?,
                );
                copy(&mut t, reality, "short-id", "reality_short_id");
            }
        }
        if yes(v, "skip-cert-verify") {
            warnings.push(format!(
                "节点 {tag}: 忽略 skip-cert-verify，Astra 仍验证证书"
            ));
        }
        cfg.insert("tls".into(), T::Table(t));
    }
    let network = s(v, "network").unwrap_or_else(|| "tcp".into());
    ensure!(
        !matches!(protocol, "shadowsocks" | "tuic" | "hysteria2") || network == "tcp",
        "该协议不支持额外传输"
    );
    let (transport, opts) = match network.as_str() {
        "tcp" => ("tcp", ""),
        "ws" if yes(get(v, "ws-opts"), "v2ray-http-upgrade") => ("http_upgrade", "ws-opts"),
        "ws" => ("ws", "ws-opts"),
        "grpc" => ("grpc", "grpc-opts"),
        "http-upgrade" | "httpupgrade" => ("http_upgrade", "http-upgrade-opts"),
        "xhttp" => ("xhttp", "xhttp-opts"),
        _ => bail!("不支持的传输 {network}"),
    };
    if transport != "tcp" {
        let opt = get(v, opts);
        let mut t = Table::new();
        put(&mut t, "kind", transport);
        copy(&mut t, opt, "path", "path");
        copy(&mut t, opt, "host", "host");
        copy(&mut t, opt, "grpc-service-name", "service_name");
        if transport == "xhttp" {
            put(
                &mut t,
                "mode",
                s(opt, "mode").unwrap_or_else(|| "auto".into()),
            );
        }
        if let Some(headers) = get(opt, "headers").as_mapping() {
            let mut h = Table::new();
            for (key, value) in headers {
                if let (Some(key), Some(value)) = (key.as_str(), scalar(value)) {
                    if key.eq_ignore_ascii_case("host") {
                        put(&mut t, "host", &value);
                    } else {
                        put(&mut h, key, value);
                    }
                }
            }
            if transport != "grpc" {
                t.insert("headers".into(), T::Table(h));
            }
        }
        cfg.insert("transport".into(), T::Table(t));
    }
    if let Some(smux) = get(v, "smux").as_mapping() {
        let smux = Y::Mapping(smux.clone());
        if yes(&smux, "enabled") && matches!(protocol, "vless" | "vmess") {
            put(
                &mut cfg,
                "mux",
                s(&smux, "protocol").unwrap_or_else(|| "smux".into()),
            );
        }
    }
    out.insert("config".into(), T::Table(cfg));
    Ok(out)
}

struct Resolver<'a> {
    nodes: &'a HashSet<String>,
    aliases: BTreeMap<String, String>,
    groups: BTreeMap<String, &'a Y>,
    options: &'a Options,
}
impl Resolver<'_> {
    fn resolve(&self, name: &str, seen: &mut HashSet<String>) -> Option<String> {
        match name {
            "DIRECT" => return Some("direct".into()),
            "REJECT" | "REJECT-DROP" => return Some("block".into()),
            _ => {}
        }
        if self.nodes.contains(name) {
            return Some(format!("proxy:{name}"));
        }
        if let Some(action) = self.aliases.get(name) {
            return Some(action.clone());
        }
        if seen.len() >= 64 || !seen.insert(name.into()) {
            return None;
        }
        let group = self.groups.get(name)?;
        if !matches!(
            s(group, "type").as_deref(),
            Some("select" | "url-test" | "fallback" | "load-balance")
        ) {
            return None;
        }
        if let Some(selected) = self.options.selections.get(name) {
            return self.resolve(selected, seen);
        }
        for member in seq(group, "proxies").iter().filter_map(Y::as_str) {
            if let Some(action) = self.resolve(member, &mut seen.clone()) {
                return Some(action);
            }
        }
        None
    }
    fn action(&self, name: &str) -> Option<String> {
        self.resolve(name, &mut HashSet::new())
    }
}

pub fn convert(input: &str, options: &Options) -> Result<Converted> {
    let mut root: Y = serde_yaml::from_str(input).context("无法解析 mihomo YAML")?;
    root.apply_merge().context("无效 YAML 合并键")?;
    ensure!(root.is_mapping(), "配置根节点必须是映射");
    for key in ["proxies", "proxy-groups", "rules"] {
        ensure!(
            get(&root, key).is_null() || get(&root, key).is_sequence(),
            "{key} 必须是数组"
        );
    }
    let socket = options
        .rule_socket
        .clone()
        .unwrap_or_else(|| "/tmp/astra-rule.sock".into());
    ensure!(!socket.is_empty(), "rule socket 不可为空");
    let mut warnings = vec!["仅转换有对应能力的字段；其他字段忽略，未执行节点连通性检查".into()];
    let mut proxy = Table::new();
    put(&mut proxy, "rule_socket", &socket);
    // Authentication and LAN ACLs cannot be represented by Astra's listeners.
    let host = if !seq(&root, "authentication").is_empty() || !yes(&root, "allow-lan") {
        "127.0.0.1".to_owned()
    } else {
        match s(&root, "bind-address").as_deref() {
            None | Some("*") => "0.0.0.0".into(),
            Some(host) => {
                host.parse::<std::net::IpAddr>()
                    .context("bind-address 必须是 IP 或 *")?;
                host.into()
            }
        }
    };
    if !seq(&root, "authentication").is_empty() {
        warnings.push("入站认证不支持，监听地址限制为 127.0.0.1".into());
    }
    let mixed = port(&root, "mixed-port")?;
    let socks = port(&root, "socks-port")?.or(mixed);
    let http = port(&root, "port")?;
    if let Some(port) = socks {
        put(&mut proxy, "socks5_listen", address(&host, port));
    }
    if let Some(port) = http {
        ensure!(Some(port) != socks, "SOCKS5 和 HTTP 不能共用端口");
        put(&mut proxy, "http_listen", address(&host, port));
    }
    if mixed.is_some() {
        warnings.push("mixed-port 仅映射为 SOCKS5（优先使用 socks-port）；HTTP 需独立 port".into());
    }
    if socks.is_none() && http.is_none() {
        put(&mut proxy, "socks5_listen", "127.0.0.1:1080");
        warnings.push("无可转换入站，使用 SOCKS5 127.0.0.1:1080".into());
    }
    for key in [
        "dns",
        "tun",
        "sniffer",
        "proxy-providers",
        "rule-providers",
        "listeners",
        "redir-port",
        "tproxy-port",
        "hosts",
        "external-controller",
        "lan-allowed-ips",
        "lan-disallowed-ips",
    ] {
        if !get(&root, key).is_null() {
            warnings.push(format!("忽略顶层配置 {key}"));
        }
    }
    let mut nodes = HashSet::new();
    let mut names = HashSet::new();
    let mut outbounds = Vec::new();
    let mut aliases = BTreeMap::new();
    for (i, v) in seq(&root, "proxies").iter().enumerate() {
        let name = s(v, "name").unwrap_or_else(|| format!("proxies[{i}]"));
        ensure!(names.insert(name.clone()), "重复名称 {name}");
        ensure!(
            !matches!(name.as_str(), "DIRECT" | "REJECT" | "REJECT-DROP"),
            "名称与内置策略冲突 {name}"
        );
        match s(v, "type").as_deref() {
            Some("direct") => {
                aliases.insert(name, "direct".into());
                continue;
            }
            Some("reject") => {
                aliases.insert(name, "block".into());
                continue;
            }
            _ => {}
        }
        match node(v, &mut warnings) {
            Ok(outbound) => {
                nodes.insert(name);
                outbounds.push(T::Table(outbound));
            }
            Err(err) => warnings.push(format!("跳过节点 {name}: {err}")),
        }
    }
    let mut groups = BTreeMap::new();
    for group in seq(&root, "proxy-groups") {
        if let Some(name) = s(group, "name") {
            ensure!(names.insert(name.clone()), "重复名称 {name}");
            ensure!(
                !matches!(name.as_str(), "DIRECT" | "REJECT" | "REJECT-DROP"),
                "代理组名称与内置策略冲突"
            );
            groups.insert(name, group);
        } else {
            warnings.push("跳过无名称代理组".into());
        }
    }
    let resolver = Resolver {
        nodes: &nodes,
        aliases,
        groups,
        options,
    };
    for (group, selected) in &options.selections {
        let spec = resolver
            .groups
            .get(group)
            .with_context(|| format!("--select 代理组不存在: {group}"))?;
        ensure!(
            seq(spec, "proxies")
                .iter()
                .any(|x| x.as_str() == Some(selected)),
            "--select 目标不是代理组成员: {group}"
        );
        ensure!(
            resolver.action(group).is_some(),
            "--select 目标不可转换或存在循环: {group}"
        );
    }
    for name in resolver.groups.keys() {
        warnings.push(match resolver.action(name) {
            Some(action) => format!("代理组 {name} 固定为 {action}，不保留测速/故障转移/负载均衡"),
            None => format!("跳过代理组 {name}: 无可用成员、循环或类型不支持"),
        });
    }
    let mut rules = Rules {
        socket,
        default_action: "block".into(),
        rules: Vec::new(),
    };
    let mode = s(&root, "mode")
        .unwrap_or_else(|| "rule".into())
        .to_ascii_lowercase();
    match mode.as_str() {
        "direct" => rules.default_action = "direct".into(),
        "global" => {
            rules.default_action = resolver.action("GLOBAL").unwrap_or_else(|| "block".into());
            warnings.push(format!(
                "global 模式固定为 {}；可用 --select GLOBAL=成员 指定",
                rules.default_action
            ));
        }
        "rule" => {
            let mut matched = false;
            for (i, spec) in seq(&root, "rules").iter().enumerate() {
                let Some(spec) = spec.as_str() else {
                    warnings.push(format!("跳过规则 {}: 必须为字符串", i + 1));
                    continue;
                };
                let parts: Vec<_> = spec.split(',').map(str::trim).collect();
                let final_rule = parts.first() == Some(&"MATCH");
                let target = if final_rule {
                    parts.get(1)
                } else {
                    parts.get(2)
                };
                let action = target.and_then(|name| resolver.action(name));
                let Some(action) = action else {
                    warnings.push(format!("跳过规则 {}: 策略不存在或不可转换", i + 1));
                    continue;
                };
                if final_rule && parts.len() == 2 {
                    rules.default_action = action;
                    matched = true;
                    break;
                }
                let result = parse_rule(&parts).map(|(kind, value)| Rule {
                    kind,
                    value,
                    action,
                });
                match result {
                    Ok(rule) => rules.rules.push(rule),
                    Err(err) => warnings.push(format!("跳过规则 {}: {err}", i + 1)),
                }
            }
            if !matched {
                warnings.push("没有可转换的 MATCH，未匹配流量默认 block".into());
            }
        }
        _ => bail!("不支持的 mode: {mode}"),
    }
    let node_count = outbounds.len();
    proxy.insert("outbounds".into(), T::Array(outbounds));
    Ok(Converted {
        proxy: toml::to_string_pretty(&proxy)?,
        rules: toml::to_string_pretty(&rules)?,
        nodes: node_count,
        rule_count: rules.rules.len(),
        warnings,
    })
}

fn parse_rule(parts: &[&str]) -> Result<(String, String)> {
    ensure!(
        parts.len() == 3
            || (parts.len() == 4
                && parts[3] == "no-resolve"
                && matches!(parts[0], "IP-CIDR" | "IP-CIDR6")),
        "不支持的规则参数"
    );
    let value = parts[1];
    let kind = match parts[0] {
        "DOMAIN" | "DOMAIN-SUFFIX" => {
            ensure!(
                !value.trim_end_matches('.').is_empty()
                    && value.len() <= 253
                    && !value.contains(' ')
                    && !value.starts_with('.')
                    && !value.trim_end_matches('.').ends_with('.'),
                "无效域名规则"
            );
            if parts[0] == "DOMAIN" {
                "domain_exact"
            } else {
                "domain_suffix"
            }
        }
        "IP-CIDR" | "IP-CIDR6" => {
            value.parse::<ipnet::IpNet>().context("无效 CIDR")?;
            "ip_cidr"
        }
        "DST-PORT" => {
            ensure!(
                value.parse::<u16>().is_ok_and(|p| p > 0),
                "仅支持单个非零目标端口"
            );
            "port"
        }
        "NETWORK" => {
            ensure!(
                matches!(value.to_ascii_lowercase().as_str(), "tcp" | "udp"),
                "无效 NETWORK"
            );
            "network"
        }
        _ => bail!("不支持的规则类型 {}", parts[0]),
    };
    Ok((
        kind.into(),
        if kind == "network" {
            value.to_ascii_lowercase()
        } else {
            value.into()
        },
    ))
}
