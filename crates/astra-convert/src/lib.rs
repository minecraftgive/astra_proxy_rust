//! Offline mihomo YAML -> Astra TOML conversion, independent of the proxy core.
mod dns;
pub mod share;
mod source;
pub mod subscription;
use anyhow::{bail, ensure, Context, Result};
use serde::Serialize;
use serde_yaml::Value as Y;
use std::{
    collections::{BTreeMap, HashSet},
    path::PathBuf,
};
use toml::{Table, Value as T};

#[derive(Default)]
pub struct Options {
    pub selections: BTreeMap<String, String>,
    pub rule_socket: Option<String>,
    pub base_dir: PathBuf,
    pub geoip_database: Option<String>,
    pub geosite_database: Option<String>,
    pub strict_rules: bool,
    /// Input came from a remote subscription: local `file` providers are
    /// refused so a subscription cannot read files from this machine.
    pub remote: bool,
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
    direct_local: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    geoip_database: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    geoip_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    geosite_database: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    geosite_url: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    rule_sets: BTreeMap<String, astra_rule_data::RuleSet>,
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
    preserved: HashSet<String>,
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
        if self.preserved.contains(name) {
            return Some(format!("proxy:{name}"));
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

fn native_groups(
    resolver: &Resolver<'_>,
) -> (
    BTreeMap<String, astra_ipc::groups::Group>,
    Vec<(String, String)>,
) {
    use astra_ipc::groups::{Group, Mode};
    let names: HashSet<_> = resolver
        .groups
        .iter()
        .filter(|(_, g)| {
            matches!(
                s(g, "type").as_deref(),
                Some("select" | "url-test" | "fallback" | "load-balance")
            )
        })
        .map(|(name, _)| name.clone())
        .collect();
    let mut candidates = BTreeMap::new();
    let mut errors = Vec::new();
    for name in &names {
        let source = resolver.groups[name];
        let parsed = (|| -> Result<Group> {
            let member = |name: &str| -> Result<String> {
                if names.contains(name) || resolver.nodes.contains(name) {
                    return Ok(name.into());
                }
                let action = resolver
                    .action(name)
                    .with_context(|| format!("不可用的代理组成员 {name}"))?;
                Ok(match action.as_str() {
                    "direct" => "DIRECT".into(),
                    "block" => "REJECT".into(),
                    _ => action
                        .strip_prefix("proxy:")
                        .context("无效代理组成员")?
                        .into(),
                })
            };
            let members = seq(source, "proxies")
                .iter()
                .map(|m| member(m.as_str().context("代理组成员必须是名称")?))
                .collect::<Result<Vec<_>>>()?;
            let selected = resolver
                .options
                .selections
                .get(name)
                .cloned()
                .or_else(|| s(source, "default-selected"))
                .as_ref()
                .map(|v| member(v))
                .transpose()?;
            let integer = |key: &str, default| -> Result<u64> {
                if get(source, key).is_null() {
                    Ok(default)
                } else {
                    get(source, key)
                        .as_u64()
                        .with_context(|| format!("{key} 必须是非负整数"))
                }
            };
            Ok(Group {
                mode: if selected.is_some() || s(source, "type").as_deref() == Some("select") {
                    Mode::Select
                } else {
                    match s(source, "type").as_deref() {
                        Some("fallback") => Mode::Fallback,
                        Some("load-balance") => Mode::LoadBalance,
                        _ => Mode::Auto,
                    }
                },
                members,
                include_all: yes(source, "include-all-proxies") || yes(source, "include-all"),
                filter: s(source, "filter"),
                exclude_filter: s(source, "exclude-filter"),
                selected,
                url: s(source, "url").unwrap_or_else(|| Group::default().url),
                interval: integer("interval", 300)?,
                timeout_ms: integer("timeout", 5000)?,
                tolerance_ms: integer("tolerance", 50)?,
                ..Default::default()
            })
        })();
        match parsed {
            Ok(group) => {
                candidates.insert(name.clone(), group);
            }
            Err(e) => errors.push((name.clone(), format!("{e:#}"))),
        }
    }
    let mut valid = BTreeMap::new();
    for name in candidates.keys() {
        let mut subset = BTreeMap::new();
        let mut pending = vec![name.clone()];
        while let Some(name) = pending.pop() {
            if subset.contains_key(&name) {
                continue;
            }
            if let Some(group) = candidates.get(&name) {
                pending.extend(
                    group
                        .members
                        .iter()
                        .filter(|m| candidates.contains_key(*m))
                        .cloned(),
                );
                subset.insert(name, group.clone());
            }
        }
        match astra_ipc::groups::validate(&subset, resolver.nodes) {
            Ok(_) => {
                valid.insert(name.clone(), candidates[name].clone());
            }
            Err(e) => errors.push((name.clone(), e)),
        }
    }
    (valid, errors)
}

pub fn convert(input: &str, options: &Options) -> Result<Converted> {
    let mut root: Y = serde_yaml::from_str(input).context("无法解析 mihomo YAML")?;
    let locations = source::Source::parse(input);
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
    // Preserve explicit LAN access and inbound authentication.
    let host = if !yes(&root, "allow-lan") {
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
    for (source, target) in [
        ("lan-allowed-ips", "lan_allowed_ips"),
        ("lan-disallowed-ips", "lan_disallowed_ips"),
        ("skip-auth-prefixes", "skip_auth_prefixes"),
    ] {
        if !get(&root, source).is_null() {
            let prefixes = seq(&root, source)
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(|v| toml::Value::String(v.into()))
                        .context("inbound prefixes must be strings")
                })
                .collect::<Result<Vec<_>>>()?;
            proxy.insert(target.into(), toml::Value::Array(prefixes));
        }
    }
    for key in [
        "lan_allowed_ips",
        "lan_disallowed_ips",
        "skip_auth_prefixes",
    ] {
        if let Some(prefixes) = proxy.get(key).and_then(toml::Value::as_array) {
            ensure!(prefixes.len() <= 256, "too many inbound ACL prefixes");
            for prefix in prefixes {
                prefix
                    .as_str()
                    .unwrap()
                    .parse::<ipnet::IpNet>()
                    .context("invalid inbound CIDR prefix")?;
            }
        }
    }
    let mixed = port(&root, "mixed-port")?;
    let socks = port(&root, "socks-port")?;
    let http = port(&root, "port")?;
    if let Some(port) = socks {
        put(&mut proxy, "socks5_listen", address(&host, port));
    }
    if let Some(port) = http {
        ensure!(Some(port) != socks, "SOCKS5 和 HTTP 不能共用端口");
        put(&mut proxy, "http_listen", address(&host, port));
    }
    if let Some(port) = mixed {
        ensure!(
            Some(port) != socks && Some(port) != http,
            "mixed-port 必须与 socks-port / port 不同"
        );
        put(&mut proxy, "mixed_listen", address(&host, port));
    }
    let authentication = seq(&root, "authentication")
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .context("authentication entries must be strings")
        })
        .collect::<Result<Vec<_>>>()?;
    if !authentication.is_empty() {
        proxy.insert(
            "authentication".into(),
            toml::Value::try_from(authentication)?,
        );
    }
    if socks.is_none() && http.is_none() && mixed.is_none() {
        put(&mut proxy, "socks5_listen", "127.0.0.1:1080");
        warnings.push("无可转换入站，使用 SOCKS5 127.0.0.1:1080".into());
    }
    if !get(&root, "dns").is_null() {
        proxy.insert(
            "dns".into(),
            dns::convert(get(&root, "dns"), &mut warnings)?,
        );
    }
    for key in [
        "tun",
        "sniffer",
        "proxy-providers",
        "listeners",
        "redir-port",
        "tproxy-port",
        "hosts",
        "external-controller",
    ] {
        if !get(&root, key).is_null() {
            warnings.push(source::at(
                locations.top(key),
                format!("忽略顶层配置 {key}"),
            ));
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
        let mut node_warnings = Vec::new();
        let parsed = node(v, &mut node_warnings);
        warnings.extend(
            node_warnings
                .into_iter()
                .map(|warning| source::at(locations.item("proxies", i), warning)),
        );
        match parsed {
            Ok(outbound) => {
                nodes.insert(name);
                outbounds.push(T::Table(outbound));
            }
            Err(err) => warnings.push(source::at(
                locations.item("proxies", i),
                format!("跳过节点 {name}: {err}"),
            )),
        }
    }
    let mut groups = BTreeMap::new();
    let mut group_lines = BTreeMap::new();
    for (index, group) in seq(&root, "proxy-groups").iter().enumerate() {
        if let Some(name) = s(group, "name") {
            ensure!(names.insert(name.clone()), "重复名称 {name}");
            ensure!(
                !matches!(name.as_str(), "DIRECT" | "REJECT" | "REJECT-DROP"),
                "代理组名称与内置策略冲突"
            );
            group_lines.insert(name.clone(), locations.item("proxy-groups", index));
            groups.insert(name, group);
        } else {
            warnings.push(source::at(
                locations.item("proxy-groups", index),
                "跳过无名称代理组",
            ));
        }
    }
    let mut resolver = Resolver {
        nodes: &nodes,
        aliases,
        groups,
        options,
        preserved: HashSet::new(),
    };
    let (mut proxy_groups, group_errors) = native_groups(&resolver);
    for (name, source) in &resolver.groups {
        if s(source, "type").as_deref() == Some("load-balance")
            && s(source, "strategy").as_deref() != Some("round-robin")
        {
            warnings.push(source::at(group_lines.get(name).copied().flatten(), format!("代理组 {name}: load-balance 使用健康节点轮询，未保留 consistent-hashing / sticky-sessions 策略")));
        }
    }
    astra_ipc::groups::validate(&proxy_groups, &nodes)
        .map_err(anyhow::Error::msg)
        .context("代理组配置超过原生限制或包含无效引用")?;
    resolver.preserved.extend(proxy_groups.keys().cloned());
    for (name, error) in group_errors {
        warnings.push(source::at(
            group_lines.get(&name).copied().flatten(),
            format!("代理组 {name} 无法保留: {error}"),
        ));
    }
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
        if resolver.preserved.contains(name) {
            continue;
        }
        warnings.push(source::at(
            group_lines.get(name).copied().flatten(),
            match resolver.action(name) {
                Some(action) => {
                    format!("代理组 {name} 固定为 {action}，不保留测速/故障转移/负载均衡")
                }
                None => format!("跳过代理组 {name}: 无可用成员、循环或类型不支持"),
            },
        ));
    }
    let mut providers = BTreeMap::new();
    if let Some(mapping) = get(&root, "rule-providers").as_mapping() {
        for (index, (name, provider)) in mapping.iter().enumerate() {
            let name = name
                .as_str()
                .context("rule-provider name must be a string")?
                .to_owned();
            let parsed = (|| -> Result<astra_rule_data::RuleSet> {
                let behavior = required(provider, "behavior")?;
                let format = s(provider, "format").unwrap_or_else(|| "yaml".into());
                let mut source = astra_rule_data::RuleSet {
                    behavior,
                    format,
                    ..Default::default()
                };
                source.interval = if get(provider, "interval").is_null() {
                    86400
                } else {
                    get(provider, "interval")
                        .as_u64()
                        .context("interval must be a nonnegative integer")?
                };
                match required(provider, "type")?.as_str() {
                    "inline" => {
                        source.payload = seq(provider, "payload")
                            .iter()
                            .map(|item| {
                                item.as_str()
                                    .map(str::to_owned)
                                    .context("provider payload must contain strings")
                            })
                            .collect::<Result<_>>()?;
                        source.validate()?;
                        source.parse_entries(&source.payload)?;
                    }
                    "file" => {
                        source.path = Some(
                            astra_rule_data::resolve(
                                &options.base_dir,
                                &required(provider, "path")?,
                            )
                            .to_string_lossy()
                            .into_owned(),
                        );
                    }
                    "http" => {
                        source.url = Some(required(provider, "url")?);
                        source.path =
                            Some(format!("rule-cache/provider-{index}.{}", source.format));
                        for key in ["proxy", "header", "path-in-bundle", "size-limit"] {
                            if !get(provider, key).is_null() {
                                warnings.push(source::at(
                                    locations.named("rule-providers", &name),
                                    format!("规则集 {name}: 未映射 {key}，资源由规则核心直接下载"),
                                ));
                            }
                        }
                    }
                    other => bail!("unsupported provider type: {other}"),
                }
                source.validate()?;
                Ok(source)
            })();
            match parsed {
                Ok(source) => {
                    providers.insert(name, source);
                }
                Err(error) => {
                    if options.strict_rules {
                        return Err(error).context(source::at(
                            locations.named("rule-providers", &name),
                            format!("rule-provider {name}"),
                        ));
                    }
                    warnings.push(source::at(
                        locations.named("rule-providers", &name),
                        format!("规则集 {name} 不可转换: {error}"),
                    ));
                }
            }
        }
    }
    let mut rules = Rules {
        socket,
        default_action: "block".into(),
        direct_local: false,
        geoip_database: options.geoip_database.clone(),
        geoip_url: None,
        geosite_database: options.geosite_database.clone(),
        geosite_url: if options.geosite_database.is_some() {
            None
        } else {
            s(get(&root, "geox-url"), "geosite")
        },
        rule_sets: BTreeMap::new(),
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
                let line = locations.item("rules", i);
                let Some(spec) = spec.as_str() else {
                    ensure!(
                        !options.strict_rules,
                        "{}",
                        source::at(line, format!("规则 {} 必须为字符串", i + 1))
                    );
                    warnings.push(source::at(
                        line,
                        format!("跳过规则 {}: 必须为字符串", i + 1),
                    ));
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
                    ensure!(
                        !options.strict_rules,
                        "{}",
                        source::at(line, format!("规则 {} 的策略不存在或不可转换", i + 1))
                    );
                    warnings.push(source::at(
                        line,
                        format!("跳过规则 {}: 策略不存在或不可转换", i + 1),
                    ));
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
                    Ok(rule) => {
                        if rule.kind == "rule_set" {
                            let source = providers.get(&rule.value).with_context(|| {
                                source::at(
                                    line,
                                    format!(
                                        "RULE-SET references missing or unsupported provider: {}",
                                        rule.value
                                    ),
                                )
                            })?;
                            rules.rule_sets.insert(rule.value.clone(), source.clone());
                        }
                        rules.rules.push(rule);
                    }
                    Err(err) => {
                        if options.strict_rules {
                            return Err(err).context(source::at(line, format!("rule {}", i + 1)));
                        }
                        warnings.push(source::at(line, format!("跳过规则 {}: {err}", i + 1)));
                    }
                }
            }
            if !matched {
                warnings.push("没有可转换的 MATCH，未匹配流量默认 block".into());
            }
        }
        _ => bail!("不支持的 mode: {mode}"),
    }
    let has_geoip = rules
        .rules
        .iter()
        .any(|rule| rule.kind == "geoip" && rule.value != "LAN");
    let custom_geoip = rules
        .rules
        .iter()
        .any(|rule| rule.kind == "geoip" && rule.value != "LAN" && rule.value.len() != 2)
        || rules
            .rule_sets
            .values()
            .filter(|set| set.behavior == "classical")
            .any(|set| {
                set.payload.iter().any(|item| {
                    astra_rule_data::parse_classical(item).is_ok_and(|rule| {
                        rule.kind == "geoip" && rule.value != "LAN" && rule.value.len() != 2
                    })
                })
            });
    let geox = get(&root, "geox-url");
    let use_dat = yes(&root, "geodata-mode")
        || (s(geox, "mmdb").is_none() && (custom_geoip || s(geox, "geoip").is_some()));
    // An explicit local database must not be replaced by a remote source.
    if options.geoip_database.is_none() {
        rules.geoip_url = s(geox, if use_dat { "geoip" } else { "mmdb" });
    }
    if rules.geoip_database.is_none()
        && (has_geoip
            || rules.geoip_url.is_some()
            || rules
                .rule_sets
                .values()
                .any(|set| set.behavior == "classical"))
    {
        let filename = if use_dat { "geoip.dat" } else { "Country.mmdb" };
        let local = astra_rule_data::resolve(&options.base_dir, filename);
        rules.geoip_database = Some(if rules.geoip_url.is_some() {
            format!("geoip/{filename}")
        } else if use_dat && !local.is_file() {
            rules.geoip_url = Some(
                "https://github.com/MetaCubeX/meta-rules-dat/releases/download/latest/geoip.dat"
                    .into(),
            );
            "geoip/geoip.dat".into()
        } else {
            local.to_string_lossy().into_owned()
        });
        if has_geoip && rules.geoip_url.is_none() {
            warnings.push(format!(
                "GEOIP 默认读取源配置旁的 {filename}，可用 --geoip-db 指定 MMDB 或 geoip.dat"
            ));
        }
    }
    let needs_geosite = rules.rules.iter().any(|r| r.kind == "geosite")
        || rules
            .rule_sets
            .values()
            .any(|source| source.behavior == "classical");
    if rules.geosite_database.is_none() && (needs_geosite || rules.geosite_url.is_some()) {
        let local = astra_rule_data::resolve(&options.base_dir, "geosite.dat");
        if rules.geosite_url.is_none() && local.is_file() {
            rules.geosite_database = Some(local.to_string_lossy().into_owned());
        } else {
            rules.geosite_database = Some("geosite/geosite.dat".into());
            if rules.geosite_url.is_none() {
                rules.geosite_url=Some("https://github.com/MetaCubeX/meta-rules-dat/releases/download/latest/geosite.dat".into());
            }
        }
    }
    let node_count = outbounds.len();
    astra_ipc::groups::ensure_default(&mut proxy_groups, &nodes).map_err(anyhow::Error::msg)?;
    astra_ipc::groups::validate(&proxy_groups, &nodes).map_err(anyhow::Error::msg)?;
    for rule in &mut rules.rules {
        if rule.action.starts_with("proxy:") {
            rule.action = "proxy:PROXY".into();
        }
    }
    if rules.default_action.starts_with("proxy:") {
        rules.default_action = "proxy:PROXY".into();
    }
    proxy.insert("outbounds".into(), T::Array(outbounds));
    if !proxy_groups.is_empty() {
        proxy.insert("proxy_groups".into(), T::try_from(&proxy_groups)?);
    }
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
        parts.len() == 3 || (parts.len() == 4 && parts[3] == "no-resolve"),
        "不支持的规则参数"
    );
    let classical = if parts.len() == 4 {
        format!("{},{},{}", parts[0], parts[1], parts[3])
    } else {
        format!("{},{}", parts[0], parts[1])
    };
    let matcher = astra_rule_data::parse_classical(&classical)?;
    Ok((matcher.kind, matcher.value))
}
