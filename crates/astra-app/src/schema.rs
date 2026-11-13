use crate::edit::*;
use astra_model::document::{PathPart as Part, Platform};
use toml_edit::{DocumentMut, Item};
#[derive(Clone)]
pub enum Type {
    Text,
    Number(i64, i64),
    Choice(&'static [&'static str]),
    Boolean,
    StringList,
}
#[derive(Clone)]
pub struct Spec {
    pub key: String,
    pub label: String,
    pub kind: Type,
    pub default: String,
    pub required: bool,
    pub help: String,
}
impl Spec {
    pub fn text(key: &str, label: &str, default: &str, required: bool) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
            kind: Type::Text,
            default: default.into(),
            required,
            help: String::new(),
        }
    }
    pub fn number(key: &str, label: &str, default: i64, min: i64, max: i64) -> Self {
        Self {
            kind: Type::Number(min, max),
            ..Self::text(key, label, &default.to_string(), false)
        }
    }
    pub fn choice(
        key: &str,
        label: &str,
        default: &str,
        choices: &'static [&'static str],
        required: bool,
    ) -> Self {
        Self {
            kind: Type::Choice(choices),
            ..Self::text(key, label, default, required)
        }
    }
    pub fn secret(&self) -> bool {
        let key = self.key.to_ascii_lowercase();
        (key.contains("password") || key == "authentication")
            || key == "uuid"
            || key.contains("authorization")
            || key.contains("token")
            || key.contains("cookie")
            || key.contains("private_key")
    }
}
#[derive(Clone)]
pub enum Row {
    Field(Spec),
    Group { key: String, label: String },
    Entry { index: usize },
}
pub const OUTBOUND_KINDS: &[&str] = &[
    "SOCKS5",
    "HTTP CONNECT",
    "VLESS",
    "VMess",
    "Shadowsocks",
    "Trojan",
    "Hysteria2",
    "TUIC",
    "Trojan TLS (legacy)",
    "VLESS TLS (legacy)",
];
pub const RULE_KINDS: &[&str] = &[
    "domain_exact",
    "domain_suffix",
    "domain_subdomain",
    "domain_pattern",
    "ip_cidr",
    "port",
    "network",
    "process_name",
    "process_path",
    "domain_keyword",
    "geoip",
    "geosite",
    "domain_regex",
    "rule_set",
];
pub const PROTOCOLS: &[&str] = &[
    "vless",
    "vmess",
    "shadowsocks",
    "trojan",
    "hysteria2",
    "tuic",
];

fn auto_start(t: fn(&str) -> &str) -> Spec {
    Spec {
        kind: Type::Boolean,
        help: t("Start this instance when TUI or silent startup runs.").into(),
        ..Spec::text("auto_start", t("Auto-start this instance"), "false", false)
    }
}

pub fn rows_for(
    doc: &DocumentMut,
    path: &[Part],
    kind: astra_model::CoreKind,
    platform: Platform,
    t: fn(&str) -> &str,
) -> Vec<Row> {
    if kind == astra_model::CoreKind::Server {
        server_rows(doc, path, t)
    } else {
        rows(doc, path, kind == astra_model::CoreKind::Proxy, platform, t)
    }
}

/// Server schema uses the same field contract as client and rule documents.
pub fn server_rows(doc: &DocumentMut, path: &[Part], t: fn(&str) -> &str) -> Vec<Row> {
    if matches!(path.last(),Some(Part::Key(k)) if k=="listeners" || k=="users") {
        return collection(doc, path)
            .map(|a| {
                a.iter()
                    .enumerate()
                    .map(|(index, _)| Row::Entry { index })
                    .collect()
            })
            .unwrap_or_default();
    }
    let text = Spec::text;
    let number = Spec::number;
    let choice = Spec::choice;
    let boolean = |key: &str, default: bool| Spec {
        kind: Type::Boolean,
        ..text(key, t(key), if default { "true" } else { "false" }, false)
    };
    let list = |key: &str, required: bool| Spec {
        kind: Type::StringList,
        ..text(key, t(key), "[]", required)
    };
    let (fields, groups): (Vec<Spec>, Vec<&str>) = if path.is_empty() {
        (
            vec![
                number("max_connections", t("Maximum connections"), 1024, 1, 65536),
                number("max_udp_flows", t("Maximum UDP flows"), 512, 1, 65536),
                number(
                    "handshake_timeout_secs",
                    t("Handshake timeout (seconds)"),
                    10,
                    1,
                    i64::MAX,
                ),
                number(
                    "connect_timeout_secs",
                    t("Connect timeout (seconds)"),
                    8,
                    1,
                    i64::MAX,
                ),
                number(
                    "idle_timeout_secs",
                    t("Idle timeout (seconds)"),
                    120,
                    1,
                    i64::MAX,
                ),
                number(
                    "shutdown_timeout_secs",
                    t("Shutdown timeout (seconds)"),
                    10,
                    0,
                    300,
                ),
                boolean("allow_private_targets", false),
                list("deny_cidrs", false),
                auto_start(t),
            ],
            vec!["listeners"],
        )
    } else if matches!(path,[Part::Key(k),Part::Index(_)] if k=="listeners") {
        (
            vec![
                text("tag", t("Listener tag"), "listener", true),
                choice(
                    "protocol",
                    t("Protocol"),
                    "socks5",
                    &[
                        "socks5",
                        "http",
                        "shadowsocks",
                        "vmess",
                        "vless",
                        "trojan",
                        "hysteria2",
                        "tuic",
                    ],
                    true,
                ),
                text("listen", t("Listen address"), "127.0.0.1:1080", true),
                text("password", t("Password"), "", false),
                text("cipher", t("Cipher"), "", false),
                text("flow", t("Flow"), "", false),
                boolean("multiplex", false),
                text("obfs_password", t("Obfuscation password"), "", false),
                choice(
                    "transport",
                    t("Transport"),
                    "tcp",
                    &["tcp", "ws", "grpc", "http_upgrade", "xhttp"],
                    false,
                ),
                text("path", t("Path"), "", false),
                text("service_name", t("Service name"), "", false),
                boolean("allow_plaintext", false),
                boolean("udp", true),
            ],
            vec!["users", "tls", "reality", "decryption"],
        )
    } else if matches!(path.last(), Some(Part::Index(_)))
        && path.iter().any(|p| matches!(p,Part::Key(k) if k=="users"))
    {
        (
            vec![
                text("name", t("Name"), "", true),
                text("uuid", "UUID", "", false),
                text("password", t("Password"), "", false),
            ],
            vec![],
        )
    } else {
        match last_key(path) {
            Some("tls") => (
                vec![
                    text("certificate", t("Certificate path"), "", true),
                    text("private_key", t("Private key path"), "", true),
                ],
                vec![],
            ),
            Some("reality") => (
                vec![
                    text("private_key", t("Private key"), "", true),
                    list("server_names", true),
                    list("short_ids", true),
                    text("target", t("Target address"), "", true),
                    number(
                        "max_time_diff_secs",
                        t("Maximum clock difference (seconds)"),
                        60,
                        0,
                        i64::MAX,
                    ),
                ],
                vec![],
            ),
            Some("decryption") => (
                vec![
                    list("private_keys", true),
                    choice(
                        "mode",
                        t("Mode"),
                        "native",
                        &["native", "xorpub", "random"],
                        false,
                    ),
                    number(
                        "ticket_lifetime_secs",
                        t("Ticket lifetime (seconds)"),
                        0,
                        0,
                        u32::MAX as i64,
                    ),
                ],
                vec![],
            ),
            _ => (vec![], vec![]),
        }
    };
    let mut output: Vec<Row> = fields.into_iter().map(Row::Field).collect();
    output.extend(groups.into_iter().map(|key| Row::Group {
        key: key.into(),
        label: t(key).into(),
    }));
    // Future fields remain discoverable, and edits never discard them.
    if let Some(table) = table_at(doc.as_table(), path) {
        for (key, item) in table.iter() {
            if output.iter().any(|r| match r {
                Row::Field(s) => s.key == key,
                Row::Group { key: k, .. } => k == key,
                _ => false,
            }) {
                continue;
            }
            if item.is_table() || item.is_inline_table() || item.is_array_of_tables() {
                output.push(Row::Group {
                    key: key.into(),
                    label: key.into(),
                });
            } else {
                let mut field = text(key, key, "", false);
                field.kind = if item.is_integer() {
                    Type::Number(i64::MIN, i64::MAX)
                } else if item.is_bool() {
                    Type::Boolean
                } else if item.is_array() {
                    Type::StringList
                } else {
                    Type::Text
                };
                output.push(Row::Field(field));
            }
        }
    }
    output
}

pub fn rows(
    doc: &DocumentMut,
    path: &[Part],
    is_proxy: bool,
    platform: Platform,
    t: fn(&str) -> &str,
) -> Vec<Row> {
    if matches!(path.last(), Some(Part::Key(k)) if k == "rules" || k == "outbounds") {
        return collection(doc, path)
            .map(|a| {
                a.iter()
                    .enumerate()
                    .map(|(index, _)| Row::Entry { index })
                    .collect()
            })
            .unwrap_or_default();
    }
    let table = table_at(doc.as_table(), path);
    let get = |key: &str| {
        table
            .and_then(|t| t.get(key))
            .and_then(Item::as_str)
            .unwrap_or("")
    };
    let text = Spec::text;
    let number = Spec::number;
    let choice = Spec::choice;
    let max = u32::MAX as i64;
    let mut fields = if path.is_empty() {
        if is_proxy {
            vec![
                Spec {
                    kind: Type::Boolean,
                    ..text(
                        "standalone",
                        t("独立代理模式（无需 Rule Core）"),
                        "false",
                        false,
                    )
                },
                text("standalone_outbound", t("独立模式出站标签"), "", false),
                text(
                    "rule_socket",
                    t("规则 Core Socket"),
                    "/tmp/astra-rule.sock",
                    !doc.get("standalone")
                        .and_then(Item::as_bool)
                        .unwrap_or(false),
                ),
                text("socks5_listen", t("SOCKS5 监听地址（留空禁用）"), "", false),
                text("http_listen", t("HTTP 监听地址（留空禁用）"), "", false),
                text("mixed_listen", t("Mixed listener"), "", false),
                Spec {
                    kind: Type::StringList,
                    ..text("authentication", t("Inbound authentication"), "[]", false)
                },
                number("ipc_connections", t("IPC 连接数"), 8, 1, 128),
                number(
                    "decision_timeout_ms",
                    t("规则查询超时（毫秒）"),
                    500,
                    1,
                    i64::MAX,
                ),
                number("connect_timeout_secs", t("连接超时（秒）"), 8, 1, i64::MAX),
                number(
                    "diagnostic_connect_timeout_secs",
                    t("诊断 TCP/代理连接超时（秒）"),
                    3,
                    1,
                    astra_model::diagnostics::MAX_CONNECT_TIMEOUT_SECS as i64,
                ),
                number(
                    "shutdown_timeout_secs",
                    t("停止等待时间（秒）"),
                    2,
                    0,
                    i64::MAX,
                ),
                number("max_connections", t("最大连接数"), 4096, 1, max),
                number(
                    "decision_cache_ttl_secs",
                    t("决策缓存时间（秒，0 禁用）"),
                    5,
                    0,
                    i64::MAX,
                ),
                number("max_udp_flows", t("最大 UDP 流数"), 512, 1, 65536),
                number(
                    "udp_targets_per_association",
                    t("每个 UDP 关联的目标数"),
                    32,
                    1,
                    65536,
                ),
                Spec {
                    kind: Type::Boolean,
                    ..text(
                        "watch_config",
                        t("自动监测配置文件并热重载"),
                        "false",
                        false,
                    )
                },
                number(
                    "udp_idle_timeout_secs",
                    t("UDP 空闲超时（秒）"),
                    120,
                    1,
                    i64::MAX,
                ),
                Spec {
                    kind: Type::StringList,
                    ..text("lan_allowed_ips", t("Allowed source CIDRs"), "[]", false)
                },
                Spec {
                    kind: Type::StringList,
                    ..text("lan_disallowed_ips", t("Blocked source CIDRs"), "[]", false)
                },
                Spec {
                    kind: Type::StringList,
                    ..text(
                        "skip_auth_prefixes",
                        t("Skip authentication for CIDRs"),
                        "[]",
                        false,
                    )
                },
                auto_start(t),
            ]
        } else {
            vec![
                text("socket", t("规则监听 Socket"), "/tmp/astra-rule.sock", true),
                text("default_action", t("默认路由动作"), "direct", true),
                text("geosite_database", t("GeoSite 数据库"), "", false),
                text("geosite_url", t("GeoSite 下载地址"), "", false),
                number(
                    "geosite_interval",
                    t("GeoSite 更新间隔（秒）"),
                    86400,
                    1,
                    31536000,
                ),
                text("geoip_database", t("GeoIP 数据库"), "", false),
                text("geoip_url", t("GeoIP 下载地址"), "", false),
                number(
                    "geoip_interval",
                    t("GeoIP 更新间隔（秒）"),
                    86400,
                    1,
                    31536000,
                ),
                Spec {
                    kind: Type::Boolean,
                    ..text("direct_local", t("本地 IP 直连"), "true", false)
                },
                Spec {
                    kind: Type::Boolean,
                    ..text(
                        "watch_config",
                        t("自动监测配置文件并热重载"),
                        "false",
                        false,
                    )
                },
                auto_start(t),
            ]
        }
    } else if last_key(path) == Some("subscription")
        || (path.len() == 2 && path.first() == Some(&Part::Key("subscriptions".into())))
    {
        vec![
            text("url", t("Subscription URL"), "", true),
            number("interval", t("Update interval (seconds)"), 3600, 60, 604800),
            text("user_agent", "User-Agent", "mihomo/1.19.0", false),
            text(
                "proxy",
                t("Update proxy (group / node / DIRECT)"),
                "PROXY",
                false,
            ),
            Spec {
                kind: Type::Boolean,
                ..text(
                    "import_groups",
                    t("Import subscription groups"),
                    "false",
                    false,
                )
            },
        ]
    } else if path.len() == 2 && proxy_group_name(path).is_some() {
        let mut fields = vec![
            text(
                "interface",
                t("Egress adapter (physical or virtual)"),
                "",
                false,
            ),
            choice(
                "mode",
                t("Group mode"),
                "select",
                &["select", "auto", "fallback", "load_balance", "relay"],
                true,
            ),
            Spec {
                kind: Type::StringList,
                ..text("members", t("Group members"), "[]", false)
            },
            Spec {
                kind: Type::Boolean,
                ..text("include_all", t("Include all nodes"), "false", false)
            },
            Spec {
                kind: Type::Boolean,
                ..text(
                    "include_all_groups",
                    t("Include other groups"),
                    "false",
                    false,
                )
            },
            text("filter", t("Dynamic member filter"), "", false),
            text("exclude_filter", t("Dynamic member exclusion"), "", false),
        ];
        if !get("subscription").is_empty() {
            fields.insert(
                0,
                Spec {
                    kind: Type::Boolean,
                    ..text(
                        "include_in_proxy",
                        t("Include in Proxy group"),
                        "true",
                        false,
                    )
                },
            );
        }
        if matches!(get("mode"), "auto" | "fallback" | "load_balance") {
            fields.extend([
                text(
                    "url",
                    t("Test URL"),
                    "https://www.gstatic.com/generate_204",
                    true,
                ),
                number("interval", t("Test interval (seconds)"), 300, 1, 86400),
                number("timeout_ms", t("Test timeout (ms)"), 5000, 100, 30000),
                number("tolerance_ms", t("Switch tolerance (ms)"), 50, 0, 10000),
            ]);
        } else if get("mode") != "relay" {
            fields.push(text("selected", t("Selected member"), "", false));
        }
        fields.extend(
            [
                ("disconnect_on_manual", "Disconnect on manual switch"),
                ("disconnect_on_auto", "Disconnect on automatic improvement"),
                ("disconnect_on_failure", "Disconnect on member failure"),
                (
                    "disconnect_on_recovery",
                    "Disconnect on availability recovery",
                ),
                (
                    "disconnect_on_config_change",
                    "Disconnect on members or mode change",
                ),
            ]
            .into_iter()
            .map(|(key, label)| Spec {
                kind: Type::Boolean,
                ..text(key, t(label), "false", false)
            }),
        );
        if get("mode") == "relay" {
            fields.retain(|field| {
                !matches!(
                    field.key.as_str(),
                    "include_all" | "include_all_groups" | "filter" | "exclude_filter"
                )
            });
        }
        fields
    } else if path.len() == 2 && matches!(&path[0],Part::Key(key) if key=="rule_sets") {
        vec![
            Spec {
                kind: Type::Boolean,
                ..text("enabled", t("Rule set enabled"), "true", false)
            },
            choice(
                "behavior",
                t("规则集类型"),
                "classical",
                &["classical", "domain", "ipcidr"],
                true,
            ),
            choice("format", t("文件格式"), "yaml", &["yaml", "text"], true),
            text("path", t("规则集文件或缓存路径"), "", false),
            text("url", t("规则集下载地址"), "", false),
            number("interval", t("更新间隔（秒）"), 86400, 0, 31536000),
            Spec {
                kind: Type::StringList,
                ..text("payload", t("内联规则列表"), "[]", false)
            },
        ]
    } else if last_key(path) == Some("system_proxy") {
        vec![Spec {
            kind: Type::StringList,
            ..text(
                "services",
                t("macOS 网络服务名称列表（空列表自动发现）"),
                "[]",
                false,
            )
        }]
    } else if last_key(path) == Some("dns") {
        vec![
            Spec {
                kind: Type::Boolean,
                ..text("enable", "启用 DNS 服务", "true", false)
            },
            Spec {
                kind: Type::StringList,
                ..text(
                    "listen",
                    "DNS 监听地址（UDP/TCP）",
                    "[\"127.0.0.1:1053\"]",
                    false,
                )
            },
            Spec {
                kind: Type::StringList,
                ..text(
                    "nameserver",
                    "DNS 上游（默认仅 DoH/DoT）",
                    "[\"https://1.1.1.1/dns-query\",\"tls://1.0.0.1:853\"]",
                    false,
                )
            },
            Spec {
                kind: Type::StringList,
                ..text("fallback", "DNS 失败备用上游", "[]", false)
            },
            Spec {
                kind: Type::StringList,
                ..text(
                    "proxy_server_nameserver",
                    "代理服务器解析 DNS（始终直连，留空沿用原策略）",
                    "[]",
                    false,
                )
            },
            Spec {
                kind: Type::Boolean,
                ..text(
                    "plaintext_bypass",
                    "允许明文 DNS（跳过 DoH/DoT 强制检查，不安全）",
                    "false",
                    false,
                )
            },
            Spec {
                kind: Type::StringList,
                ..text(
                    "plaintext_bypass_domains",
                    "允许明文策略上游的域名（如 +.lan）",
                    "[]",
                    false,
                )
            },
            Spec {
                kind: Type::StringList,
                ..text(
                    "bootstrap",
                    "明文引导解析 IP（仅 plaintext_bypass 时使用）",
                    "[\"1.1.1.1\",\"8.8.8.8\"]",
                    false,
                )
            },
            Spec {
                kind: Type::Boolean,
                ..text("respect_rules", "DNS 上游连接遵循路由规则", "false", false)
            },
            Spec {
                kind: Type::Boolean,
                ..text("ipv6", "解析 IPv6 地址", "true", false)
            },
            Spec {
                kind: Type::Boolean,
                ..text("hijack", "接管 TUN 的 53 端口 DNS", "true", false)
            },
            Spec {
                kind: Type::Boolean,
                ..text(
                    "system_takeover",
                    "TUN 期间将系统 DNS 指向 TUN（退出时恢复）",
                    "true",
                    false,
                )
            },
            Spec {
                kind: Type::Boolean,
                ..text(
                    "route_system_dns",
                    "TUN 期间把原系统 DNS 服务器路由进 TUN",
                    "true",
                    false,
                )
            },
            Spec {
                kind: Type::Boolean,
                ..text(
                    "system_takeover_without_tun",
                    "未开 TUN 时也接管系统 DNS（macOS，监听 53 端口，退出时恢复）",
                    "false",
                    false,
                )
            },
            Spec {
                kind: Type::Boolean,
                ..text("fake_ip", "启用 Fake IP", "false", false)
            },
            text("fake_ip_range", "Fake IPv4 地址池", "198.18.0.0/16", false),
            text("fake_ip_range6", "Fake IPv6 地址池（可选 ULA）", "", false),
            choice(
                "fake_ip_filter_mode",
                "Fake IP 过滤方式",
                "blacklist",
                &["blacklist", "whitelist"],
                false,
            ),
            Spec {
                kind: Type::StringList,
                ..text(
                    "fake_ip_filter",
                    "Fake IP 域名过滤列表",
                    "[\"+.lan\",\"+.local\",\"localhost\"]",
                    false,
                )
            },
            number("cache_capacity", "DNS 缓存条目上限", 4096, 0, 65536),
            number(
                "cache_max_ttl_secs",
                "DNS 缓存最大 TTL（秒）",
                3600,
                1,
                86400,
            ),
            number("negative_ttl_secs", "DNS 负缓存最大 TTL（秒）", 60, 0, 3600),
            number(
                "query_timeout_ms",
                "DNS 单个上游超时（毫秒）",
                3000,
                100,
                30000,
            ),
            number("max_concurrent", "DNS 并发查询上限", 128, 1, 1024),
            number("mapping_capacity", "DNS 域名映射条目上限", 8192, 1, 65536),
            number(
                "mapping_ttl_secs",
                "Fake IP 映射保留时间（秒）",
                86400,
                1,
                604800,
            ),
            number("fake_ip_ttl", "Fake IP 响应 TTL（秒）", 60, 1, 3600),
        ]
    } else if last_key(path) == Some("tun") {
        vec![
            Spec {
                kind: Type::Boolean,
                ..text(
                    "auto_interface",
                    "自动跟随出口网卡（运行时检测）",
                    if get("interface").is_empty() {
                        "true"
                    } else {
                        "false"
                    },
                    false,
                )
            },
            Spec {
                kind: Type::Boolean,
                ..text("auto_gateway", "自动跟随所选网卡的网关", "true", false)
            },
            Spec {
                kind: Type::Boolean,
                ..text(
                    "sniff",
                    "从 TLS/HTTP 识别域名以匹配规则（保留原始目标 IP）",
                    "true",
                    false,
                )
            },
            text("name", t("TUN 接口名（留空自动分配）"), "", false),
            text("address", t("TUN IPv4 地址"), "198.18.0.1", false),
            number("prefix", t("TUN 地址前缀"), 30, 0, 32),
            text("ipv6_address", t("TUN IPv6 地址（可选）"), "", false),
            number("ipv6_prefix", t("TUN IPv6 地址前缀"), 126, 0, 128),
            number("mtu", "TUN MTU", 1500, 1280, 9000),
            Spec {
                kind: Type::StringList,
                ..text("routes", t("接管的 CIDR 列表"), "[]", false)
            },
            Spec {
                kind: Type::StringList,
                ..text(
                    "bypass",
                    t("经原网关绕过的 CIDR 列表（含节点 IP 和 DNS）"),
                    "[]",
                    false,
                )
            },
            text("gateway", t("原出口网关"), "", false),
            text("gateway6", t("原出口 IPv6 网关（可选）"), "", false),
            text(
                "interface",
                t("Egress adapter (physical or virtual)"),
                "",
                false,
            ),
        ]
    } else if matches!(last_key(path), Some("tproxy" | "redirect")) {
        vec![text(
            "listen",
            t("Linux 透明代理监听地址"),
            "0.0.0.0:7893",
            true,
        )]
    } else if matches!(path.last(), Some(Part::Index(_))) && last_key(path) == Some("rules") {
        vec![
            choice("kind", t("匹配类型"), "domain_suffix", RULE_KINDS, true),
            text("value", t("匹配内容"), "example.com", true),
            text("action", t("路由动作"), "direct", true),
        ]
    } else if matches!(path.last(), Some(Part::Index(_))) && last_key(path) == Some("outbounds") {
        let mut f = vec![
            choice(
                "kind",
                t("节点类型"),
                "socks5",
                &["socks5", "http", "native", "trojan_tls", "vless_tls"],
                true,
            ),
            text("tag", t("节点标签"), "edge", true),
            text(
                if table.is_some_and(|table| table.contains_key("dialer-proxy")) {
                    "dialer-proxy"
                } else {
                    "dialer_proxy"
                },
                t("前置代理（节点或代理组）"),
                "",
                false,
            ),
        ];
        match get("kind") {
            "native" => {}
            "trojan_tls" | "vless_tls" => {
                f.extend([
                    text(
                        "server",
                        t("服务器（主机:端口）"),
                        "node.example.com:443",
                        true,
                    ),
                    text("sni", t("TLS 服务器名"), "node.example.com", true),
                ]);
                f.push(if get("kind") == "trojan_tls" {
                    text("password", t("密码"), "", true)
                } else {
                    text("uuid", "UUID", "", true)
                });
            }
            _ => f.extend([
                text("server", t("服务器（主机:端口）"), "127.0.0.1:7890", true),
                text("username", t("用户名"), "", false),
                text("password", t("密码"), "", false),
            ]),
        }
        f
    } else if last_key(path) == Some("config") {
        let protocol = get("protocol");
        let mut f = vec![
            choice("protocol", t("代理协议"), "vless", PROTOCOLS, true),
            text("server", t("服务器主机"), "node.example.com", true),
            number("port", t("服务器端口"), 443, 1, 65535),
        ];
        if matches!(protocol, "vless" | "vmess" | "tuic") {
            f.push(text(
                "uuid",
                "UUID",
                "00000000-0000-0000-0000-000000000001",
                true,
            ));
        }
        if matches!(protocol, "trojan" | "shadowsocks" | "hysteria2" | "tuic") {
            f.push(text("password", t("密码"), "", true));
        }
        match protocol {
            "vless" => f.extend([
                choice(
                    "flow",
                    t("Vision 流控"),
                    "",
                    &["", "xtls-rprx-vision"],
                    false,
                ),
                text(
                    "encryption",
                    t("VLESS Encryption（完整字符串）"),
                    "none",
                    false,
                ),
            ]),
            "vmess" => f.push(choice(
                "cipher",
                t("加密方式"),
                "auto",
                &["auto", "aes-128-gcm", "chacha20-poly1305"],
                false,
            )),
            "shadowsocks" => f.push(text("cipher", t("加密方式"), "aes-128-gcm", true)),
            "hysteria2" => f.extend([
                text("obfs_password", t("Salamander 混淆密码"), "", false),
                text("pin_sha256", t("证书 SHA-256 固定值"), "", false),
            ]),
            "tuic" => f.extend([
                choice(
                    "tuic_udp_mode",
                    t("UDP 模式"),
                    "native",
                    &["native", "quic"],
                    false,
                ),
                number("tuic_idle_timeout_secs", t("空闲超时（秒）"), 15, 3, 3600),
                choice(
                    "congestion_control",
                    t("拥塞控制"),
                    "cubic",
                    &["cubic", "bbr", "new_reno"],
                    false,
                ),
            ]),
            _ => {}
        }
        if matches!(protocol, "vless" | "vmess" | "shadowsocks") {
            f.push(text("mux", t("多路复用协议（可选）"), "", false));
        }
        f
    } else if last_key(path) == Some("tls") {
        vec![
            text("server_name", t("TLS 服务器名"), "node.example.com", true),
            text("ca_file", t("自定义 CA 文件"), "", false),
            choice(
                "fingerprint",
                t("客户端 TLS 指纹"),
                "",
                &[
                    "",
                    "chrome",
                    "chrome120",
                    "firefox",
                    "firefox120",
                    "safari",
                    "safari16",
                    "ios",
                    "android",
                    "edge",
                    "random",
                ],
                false,
            ),
            text("cert_pin_sha256", t("服务端证书 SHA-256 指纹"), "", false),
            text("reality_public_key", t("REALITY 公钥"), "", false),
            text("reality_short_id", "REALITY Short ID", "", false),
        ]
    } else if last_key(path) == Some("transport") {
        let mut f = vec![
            choice(
                "kind",
                t("传输方式"),
                "tcp",
                &["tcp", "ws", "grpc", "http_upgrade", "xhttp"],
                false,
            ),
            text("path", t("请求路径"), "/", false),
            text("host", "Host / Authority", "", false),
            text("service_name", t("gRPC 服务名"), "GunService", false),
        ];
        if get("kind") == "xhttp" {
            f.push(choice(
                "mode",
                t("XHTTP 模式"),
                "stream-one",
                &["stream-one", "stream-up", "packet-up", "auto"],
                false,
            ));
        }
        f
    } else {
        vec![]
    };
    let chain_preview = if is_proxy {
        chain_preview(doc, path)
    } else {
        None
    };
    for field in &mut fields {
        if field.key == "port" && last_key(path) == Some("config") {
            field.required = true;
        }
        field.help = match field.key.as_str() {
            "auto_start" if path.is_empty() => {
                t("Start this instance when TUI or silent startup runs.").into()
            }
            "action" | "default_action" => {
                t("Select direct / block / a proxy node; manual proxy:tag is supported").into()
            }
            "dialer_proxy" | "dialer-proxy" => {
                "前置节点或代理组；relay 中仅首跳应用自身前置配置".into()
            }
            "members" if get("mode") == "relay" => {
                "按 entry → relay → … → exit 顺序填写；中继可引用代理组".into()
            }
            "kind" if last_key(path) == Some("rules") => {
                t("↑↓ 选择匹配类型；更改类型后请检查匹配内容；规则按列表顺序生效").into()
            }
            "kind" if last_key(path) == Some("transport") => {
                t("↑↓ 选择传输方式；切换方式会移除不适用的模式和请求头").into()
            }
            "kind" => t("↑↓ 选择；切换节点类型会重置专用字段并保留标签，保存前检查凭据").into(),
            "protocol" => t("↑↓ 选择；保留可复用参数，重置不适用的协议字段，保存前检查凭据").into(),
            "standalone" => {
                t("true：全部 TCP/UDP 入站使用固定出站，不连接 Rule Core；false：使用规则分流")
                    .into()
            }
            "standalone_outbound" => {
                t("Select an existing node; leave empty to use the only node").into()
            }
            "port" => t("输入 1–65535 的整数").into(),
            "encryption" => {
                t("填写完整 mlkem768x25519plus… 字符串；留空或 none 不启用；不是 cipher").into()
            }
            "fingerprint" => t("客户端 ClientHello 样式，例如 chrome；不是服务端证书哈希").into(),
            "cert_pin_sha256" => {
                t("服务端证书 SHA-256：64 位十六进制；替代 CA 验证，不与自定义 CA / REALITY 合用")
                    .into()
            }
            "mode" if last_key(path) == Some("transport") => {
                t("auto：普通 TLS 选择 packet-up，REALITY 选择 stream-one；省略默认 stream-one")
                    .into()
            }
            _ if matches!(field.kind, Type::Boolean) => {
                t("Enter / Space toggle · d reset to default").into()
            }
            _ if field.required => t("Enter edit · required").into(),
            _ => t("Enter edit · d reset to default").into(),
        };
        if matches!(
            field.key.as_str(),
            "dialer_proxy" | "dialer-proxy" | "members"
        ) {
            if let Some(preview) = &chain_preview {
                field.help.push_str(&format!(" · 路径预览: {preview}"));
            }
        }
    }
    let mut rows: Vec<Row> = fields
        .into_iter()
        .filter(|field| platform.option(last_key(path), &field.key))
        .map(Row::Field)
        .collect();
    let groups: Vec<(&str, &str)> = if path.is_empty() {
        if is_proxy {
            vec![
                ("system_proxy", t("系统代理（SOCKS）")),
                ("tun", t("TUN 接管")),
                ("dns", "DNS / Fake IP"),
                ("tproxy", "TPROXY (TCP/UDP)"),
                ("redirect", "REDIRECT (TCP)"),
                ("outbounds", t("出站节点")),
                ("proxy_groups", t("Proxy groups")),
                ("subscriptions", t("Subscriptions")),
            ]
        } else {
            vec![
                ("rules", t("路由规则（按顺序匹配）")),
                ("rule_sets", t("规则集")),
            ]
        }
    } else if get("kind") == "native" && matches!(path.last(), Some(Part::Index(_))) {
        vec![("config", t("协议参数"))]
    } else if last_key(path) == Some("config") {
        vec![("tls", "TLS / REALITY"), ("transport", t("传输参数"))]
    } else if last_key(path) == Some("transport") {
        vec![("headers", t("自定义请求头"))]
    } else {
        vec![]
    };
    rows.extend(
        groups
            .into_iter()
            .filter(|(key, _)| platform.option(last_key(path), key))
            .map(|(key, label)| Row::Group {
                key: key.into(),
                label: label.into(),
            }),
    );
    if let Some(table) = table {
        for (key, item) in table.iter() {
            if path.is_empty() && is_proxy && key == "allow_lan" {
                continue;
            }
            if (last_key(path) == Some("subscription")
                || (path.len() == 2 && path.first() == Some(&Part::Key("subscriptions".into()))))
                && matches!(key, "managed_nodes" | "managed_groups")
            {
                continue;
            }
            if proxy_group_name(path).is_some()
                && ((matches!(get("mode"), "auto" | "fallback" | "load_balance")
                    && key == "selected")
                    || (!matches!(get("mode"), "auto" | "fallback" | "load_balance")
                        && matches!(key, "url" | "interval" | "timeout_ms" | "tolerance_ms")))
            {
                continue;
            }
            if !platform.option(last_key(path), key) {
                continue;
            }
            if rows.iter().any(|r| match r {
                Row::Field(s) => s.key == key,
                Row::Group { key: k, .. } => k == key,
                _ => false,
            }) {
                continue;
            }
            if item.as_table_like().is_some() || item.as_array_of_tables().is_some() {
                rows.push(Row::Group {
                    key: key.into(),
                    label: key.into(),
                });
            } else {
                let mut spec = text(key, key, "", false);
                if item.as_integer().is_some() {
                    spec.kind = Type::Number(i64::MIN, i64::MAX);
                } else if item.as_array().is_some() {
                    spec.kind = Type::StringList;
                } else if item.as_bool().is_some() {
                    spec.kind = Type::Boolean;
                }
                rows.push(Row::Field(spec));
            }
        }
    }
    rows
}

fn chain_preview(doc: &DocumentMut, path: &[Part]) -> Option<String> {
    let table = table_at(doc.as_table(), path)?;
    let root = proxy_group_name(path).or_else(|| table.get("tag").and_then(Item::as_str))?;
    let mut config: astra_model::config::proxy::ProxyConfig =
        toml::from_str(&doc.to_string()).ok()?;
    let nodes: std::collections::HashMap<_, _> = config
        .outbounds
        .into_iter()
        .map(|node| (node.tag().to_owned(), node))
        .collect();
    let tags = nodes.keys().cloned().collect();
    astra_model::groups::ensure_default(&mut config.proxy_groups, &tags).ok()?;
    let groups = astra_model::groups::expand(&config.proxy_groups, &tags).ok()?;
    let route = astra_model::chain::resolve(root, &groups, &nodes, |name, group| {
        anyhow::ensure!(
            group.mode == astra_model::groups::Mode::Select,
            "{name}: 等待运行时选择"
        );
        group
            .selected
            .as_ref()
            .or_else(|| group.members.first())
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("{name}: 尚无成员"))
    });
    Some(match route {
        Ok(route) if route.blocked => "REJECT".into(),
        Ok(route) if route.nodes.is_empty() => "DIRECT".into(),
        Ok(route) => route
            .nodes
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let role = if route.nodes.len() == 1 {
                    "entry/exit"
                } else if i == 0 {
                    "entry"
                } else if i + 1 == route.nodes.len() {
                    "exit"
                } else {
                    "relay"
                };
                format!("{role} {name}")
            })
            .collect::<Vec<_>>()
            .join(" → "),
        Err(error) => error.to_string(),
    })
}
