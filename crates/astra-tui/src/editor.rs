//! Entry-oriented configuration forms. TOML stays an implementation detail of saving.
use crate::protocol::Kind as CoreKind;
use anyhow::{bail, Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};
use serde::{Deserialize, Serialize};
use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table, TableLike};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
enum Part {
    Key(String),
    Index(usize),
}
type Path = Vec<Part>;
#[derive(Default, Clone, Serialize, Deserialize)]
struct Form {
    path: Path,
    selected: usize,
    stack: Vec<(Path, usize)>,
    mode: Mode,
    #[serde(default)]
    expanded_nodes: Vec<usize>,
    #[serde(default)]
    flat: bool,
}
#[derive(Default, Clone, Serialize, Deserialize)]
enum Mode {
    #[default]
    Browse,
    Field {
        key: String,
        input: TextInput,
        reveal: bool,
    },
    Header(TextInput),
    NewOutbound(usize),
    Delete {
        key: Option<String>,
        index: Option<usize>,
    },
}
#[derive(Default, Clone, Serialize, Deserialize)]
struct TextInput {
    text: String,
    cursor: usize,
    replace: bool,
}
impl TextInput {
    fn new(text: String) -> Self {
        let cursor = text.chars().count();
        Self {
            text,
            cursor,
            replace: true,
        }
    }
    fn byte(&self) -> usize {
        self.text
            .char_indices()
            .nth(self.cursor)
            .map(|(i, _)| i)
            .unwrap_or(self.text.len())
    }
    fn insert(&mut self, text: &str) {
        if self.replace {
            self.text.clear();
            self.cursor = 0;
            self.replace = false;
        }
        for c in text.chars().filter(|c| !c.is_control()) {
            let byte = self.byte();
            self.text.insert(byte, c);
            self.cursor += 1;
        }
    }
    fn key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            if key.code == KeyCode::Char('u') {
                self.text.clear();
                self.cursor = 0;
                self.replace = false;
            }
            return;
        }
        match key.code {
            KeyCode::Char(c) => self.insert(&c.to_string()),
            KeyCode::Left => {
                self.replace = false;
                self.cursor = self.cursor.saturating_sub(1);
            }
            KeyCode::Right => {
                self.replace = false;
                self.cursor = (self.cursor + 1).min(self.text.chars().count());
            }
            KeyCode::Home => {
                self.replace = false;
                self.cursor = 0;
            }
            KeyCode::End => {
                self.replace = false;
                self.cursor = self.text.chars().count();
            }
            KeyCode::Backspace | KeyCode::Delete if self.replace => {
                self.text.clear();
                self.cursor = 0;
                self.replace = false;
            }
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                let byte = self.byte();
                self.text.remove(byte);
            }
            KeyCode::Delete if self.cursor < self.text.chars().count() => {
                let byte = self.byte();
                self.text.remove(byte);
            }
            _ => {}
        }
    }
}

// Keep the old draft representation so existing ui-session.json files migrate without loss.
#[derive(Default, Clone, Serialize, Deserialize)]
pub(crate) struct Editor {
    lines: Vec<String>,
    #[serde(default)]
    row: usize,
    #[serde(default)]
    col: usize,
    pub original: String,
    pub name: String,
    #[serde(default)]
    kind: Option<CoreKind>,
    #[serde(default)]
    form: Form,
}

#[derive(Clone)]
enum Type {
    Text,
    Number(i64, i64),
    Choice(&'static [&'static str]),
    Boolean,
    StringList,
}
#[derive(Clone)]
struct Spec {
    key: String,
    label: String,
    kind: Type,
    default: String,
    required: bool,
    help: String,
}
impl Spec {
    fn text(key: &str, label: &str, default: &str, required: bool) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
            kind: Type::Text,
            default: default.into(),
            required,
            help: String::new(),
        }
    }
    fn number(key: &str, label: &str, default: i64, min: i64, max: i64) -> Self {
        Self {
            kind: Type::Number(min, max),
            ..Self::text(key, label, &default.to_string(), false)
        }
    }
    fn choice(
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
    fn secret(&self) -> bool {
        let key = self.key.to_ascii_lowercase();
        key.contains("password")
            || key == "uuid"
            || key.contains("authorization")
            || key.contains("token")
            || key.contains("cookie")
    }
}
#[derive(Clone)]
enum Row {
    Field(Spec),
    Group { key: String, label: String },
    Entry { index: usize },
}
#[derive(Clone)]
enum FlatRow {
    Field {
        path: Path,
        spec: Spec,
        prefix: String,
    },
    Collection {
        path: Path,
        label: String,
    },
    Node {
        index: usize,
        label: String,
    },
    Rule {
        index: usize,
        label: String,
    },
    Section {
        path: Path,
        label: String,
    },
    AddHeader {
        path: Path,
    },
}
impl FlatRow {
    fn path(&self) -> Path {
        match self {
            Self::Field { path, .. }
            | Self::Collection { path, .. }
            | Self::Section { path, .. }
            | Self::AddHeader { path } => path.clone(),
            Self::Node { index, .. } => vec![Part::Key("outbounds".into()), Part::Index(*index)],
            Self::Rule { index, .. } => vec![Part::Key("rules".into()), Part::Index(*index)],
        }
    }
    fn node(&self) -> Option<usize> {
        node_index(&self.path())
    }
}
fn node_index(path: &[Part]) -> Option<usize> {
    match path {
        [Part::Key(key), Part::Index(index), ..] if key == "outbounds" => Some(*index),
        _ => None,
    }
}
fn item_index(path: &[Part]) -> Option<(&str, usize)> {
    match path {
        [Part::Key(key), Part::Index(index), ..] if key == "outbounds" || key == "rules" => {
            Some((key, *index))
        }
        _ => None,
    }
}
const OUTBOUND_KINDS: &[&str] = &[
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
const RULE_KINDS: &[&str] = &[
    "domain_exact",
    "domain_suffix",
    "ip_cidr",
    "port",
    "network",
];
const PROTOCOLS: &[&str] = &[
    "vless",
    "vmess",
    "shadowsocks",
    "trojan",
    "hysteria2",
    "tuic",
];

fn table_at<'a>(table: &'a dyn TableLike, path: &[Part]) -> Option<&'a dyn TableLike> {
    if path.is_empty() {
        return Some(table);
    }
    let Part::Key(key) = &path[0] else {
        return None;
    };
    let item = table.get(key)?;
    if let Some(Part::Index(index)) = path.get(1) {
        table_at(item.as_array_of_tables()?.get(*index)?, &path[2..])
    } else {
        table_at(item.as_table_like()?, &path[1..])
    }
}
fn table_at_mut<'a>(table: &'a mut dyn TableLike, path: &[Part]) -> Result<&'a mut dyn TableLike> {
    if path.is_empty() {
        return Ok(table);
    }
    let Part::Key(key) = &path[0] else {
        bail!("无效的条目路径");
    };
    if let Some(Part::Index(index)) = path.get(1) {
        let child = table
            .get_mut(key)
            .and_then(Item::as_array_of_tables_mut)
            .and_then(|a| a.get_mut(*index))
            .context("条目已不存在，请返回上一级")?;
        table_at_mut(child, &path[2..])
    } else {
        if table.get(key).is_none() {
            table.insert(key, Item::Table(Table::new()));
        }
        let child = table
            .get_mut(key)
            .and_then(Item::as_table_like_mut)
            .context("该条目不是参数分组")?;
        table_at_mut(child, &path[1..])
    }
}
fn scalar(item: &Item) -> String {
    if let Some(s) = item.as_str() {
        s.to_owned()
    } else {
        item.to_string().trim().to_owned()
    }
}
fn replace_field(table: &mut dyn TableLike, key: &str, mut item: Item) {
    if let Some(existing) = table.get_mut(key) {
        if let (Some(old), Some(new)) = (existing.as_value(), item.as_value_mut()) {
            *new.decor_mut() = old.decor().clone();
        }
        *existing = item;
    } else {
        table.insert(key, item);
    }
}
// Encoding respects parsed source positions, which must follow the new logical order
// after moving an array item (including all of that item's nested tables).
fn position_tables(table: &mut Table, position: &mut isize) {
    table.set_position(*position);
    *position += 1;
    for (_, item) in table.iter_mut() {
        if let Some(child) = item.as_table_mut() {
            position_tables(child, position);
        } else if let Some(array) = item.as_array_of_tables_mut() {
            for child in array.iter_mut() {
                position_tables(child, position);
            }
        }
    }
}
fn last_key(path: &[Part]) -> Option<&str> {
    path.iter().rev().find_map(|p| {
        if let Part::Key(k) = p {
            Some(k.as_str())
        } else {
            None
        }
    })
}
fn collection<'a>(doc: &'a DocumentMut, path: &[Part]) -> Option<&'a ArrayOfTables> {
    let (Part::Key(key), parent) = path.split_last()? else {
        return None;
    };
    table_at(doc.as_table(), parent)?
        .get(key)?
        .as_array_of_tables()
}
fn collection_mut<'a>(doc: &'a mut DocumentMut, path: &[Part]) -> Result<&'a mut ArrayOfTables> {
    let (Part::Key(key), parent) = path.split_last().context("没有选择条目列表")? else {
        bail!("没有选择条目列表");
    };
    let table = table_at_mut(doc.as_table_mut(), parent)?;
    if table.get(key).is_none() {
        table.insert(key, Item::ArrayOfTables(ArrayOfTables::new()));
    }
    table
        .get_mut(key)
        .and_then(Item::as_array_of_tables_mut)
        .context("该分组不是条目列表")
}

impl Editor {
    pub fn new(name: String, text: String) -> Self {
        Self {
            lines: text.split('\n').map(str::to_owned).collect(),
            original: text,
            name,
            form: Form {
                flat: true,
                ..Default::default()
            },
            ..Default::default()
        }
    }
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }
    pub fn with_kind(mut self, kind: CoreKind) -> Self {
        self.kind = Some(kind);
        self
    }
    fn is_proxy(&self, doc: &DocumentMut) -> bool {
        self.kind.map(|k| k == CoreKind::Proxy).unwrap_or_else(|| {
            doc.get("rule_socket").is_some()
                || doc.get("standalone").is_some()
                || doc.get("outbounds").is_some()
        })
    }
    fn document(&self) -> Result<DocumentMut> {
        let mut doc: DocumentMut = self
            .text()
            .parse()
            .context("配置格式无法解析。原草稿已保留，请修复配置后重新打开")?;
        // Both [[rules]] and rules = [{...}] are valid core configurations.
        for key in ["rules", "outbounds"] {
            if let Some(array) = doc.get(key).and_then(Item::as_array) {
                let mut tables = ArrayOfTables::new();
                for item in array.iter() {
                    tables.push(
                        item.as_inline_table()
                            .context("列表中需要参数条目")?
                            .clone()
                            .into_table(),
                    );
                }
                doc[key] = Item::ArrayOfTables(tables);
            }
        }
        Ok(doc)
    }
    fn update(&mut self, mut doc: DocumentMut) {
        position_tables(doc.as_table_mut(), &mut 0);
        self.lines = doc.to_string().split('\n').map(str::to_owned).collect();
    }
    pub fn normalize(&mut self) {
        self.row = 0;
        self.col = 0;
        if !self.form.flat {
            let mut target = self.form.path.clone();
            let key = match &self.form.mode {
                Mode::Field { key, .. } => Some(key.clone()),
                _ => match self
                    .document()
                    .ok()
                    .and_then(|doc| self.rows(&doc).get(self.form.selected).cloned())
                {
                    Some(Row::Field(spec)) => Some(spec.key),
                    Some(Row::Group { key, .. }) => {
                        target.push(Part::Key(key));
                        None
                    }
                    Some(Row::Entry { index }) => {
                        target.push(Part::Index(index));
                        None
                    }
                    None => None,
                },
            };
            if target.len() > 2 || key.is_some() {
                if let Some(index) = node_index(&target) {
                    self.expand_node(index);
                }
            }
            self.form.flat = true;
            self.form.stack.clear();
            if let Ok(doc) = self.document() {
                if let Some(key) = key {
                    self.focus_field(&doc, &target, &key);
                } else {
                    self.form.selected = self
                        .visible_rows(&doc)
                        .iter()
                        .position(|r| r.path() == target)
                        .unwrap_or(0);
                }
            }
        }
        if let Ok(doc) = self.document() {
            let count = doc
                .get("outbounds")
                .and_then(Item::as_array_of_tables)
                .map(|a| a.len())
                .unwrap_or(0);
            self.form.expanded_nodes.retain(|i| *i < count);
            self.form.selected = self
                .form
                .selected
                .min(self.visible_rows(&doc).len().saturating_sub(1));
        }
    }
    pub fn paste(&mut self, text: &str) {
        match &mut self.form.mode {
            Mode::Field { input, .. } | Mode::Header(input) => input.insert(text),
            _ => {}
        }
    }
    pub fn cancel_input(&mut self) {
        self.form.mode = Mode::Browse;
    }
    pub fn can_close(&self) -> bool {
        if !matches!(self.form.mode, Mode::Browse) {
            return false;
        }
        !self
            .document()
            .ok()
            .and_then(|doc| {
                self.visible_rows(&doc)
                    .get(self.form.selected)
                    .and_then(FlatRow::node)
            })
            .is_some_and(|index| self.form.expanded_nodes.contains(&index))
    }
    pub fn prepare_save(&mut self) -> Result<String> {
        match self.form.mode {
            Mode::Field { .. } => self.commit_field()?,
            Mode::Browse => {}
            _ => bail!("请先确认或取消当前条目窗口，再保存配置"),
        }
        Ok(self.text())
    }
    fn schema_rows(&self, doc: &DocumentMut, path: &[Part]) -> Vec<Row> {
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
            if self.is_proxy(doc) {
                vec![
                    Spec {
                        kind: Type::Boolean,
                        ..text(
                            "standalone",
                            "独立代理模式（无需 Rule Core）",
                            "false",
                            false,
                        )
                    },
                    text("standalone_outbound", "独立模式出站标签", "", false),
                    text(
                        "rule_socket",
                        "规则 Core Socket",
                        "/tmp/astra-rule.sock",
                        !doc.get("standalone")
                            .and_then(Item::as_bool)
                            .unwrap_or(false),
                    ),
                    text("socks5_listen", "SOCKS5 监听地址（留空禁用）", "", false),
                    text("http_listen", "HTTP 监听地址（留空禁用）", "", false),
                    number("ipc_connections", "IPC 连接数", 8, 1, 128),
                    number(
                        "decision_timeout_ms",
                        "规则查询超时（毫秒）",
                        500,
                        1,
                        i64::MAX,
                    ),
                    number("connect_timeout_secs", "连接超时（秒）", 8, 1, i64::MAX),
                    number(
                        "shutdown_timeout_secs",
                        "停止等待时间（秒）",
                        10,
                        0,
                        i64::MAX,
                    ),
                    number("max_connections", "最大连接数", 4096, 1, max),
                    number(
                        "decision_cache_ttl_secs",
                        "决策缓存时间（秒，0 禁用）",
                        5,
                        0,
                        i64::MAX,
                    ),
                    number("max_udp_flows", "最大 UDP 流数", 512, 1, 65536),
                    number(
                        "udp_targets_per_association",
                        "每个 UDP 关联的目标数",
                        32,
                        1,
                        65536,
                    ),
                    Spec {
                        kind: Type::Boolean,
                        ..text("watch_config", "自动监测配置文件并热重载", "false", false)
                    },
                    number(
                        "udp_idle_timeout_secs",
                        "UDP 空闲超时（秒）",
                        120,
                        1,
                        i64::MAX,
                    ),
                ]
            } else {
                vec![
                    text("socket", "规则监听 Socket", "/tmp/astra-rule.sock", true),
                    text("default_action", "默认路由动作", "direct", true),
                    Spec {
                        kind: Type::Boolean,
                        ..text("watch_config", "自动监测配置文件并热重载", "false", false)
                    },
                ]
            }
        } else if last_key(path) == Some("system_proxy") {
            vec![Spec {
                kind: Type::StringList,
                ..text(
                    "services",
                    "macOS 网络服务名称列表（空列表自动发现）",
                    "[]",
                    false,
                )
            }]
        } else if last_key(path) == Some("tun") {
            vec![
                text("name", "TUN 接口名（留空自动分配）", "", false),
                text("address", "TUN IPv4 地址", "198.18.0.1", false),
                number("prefix", "TUN 地址前缀", 30, 0, 32),
                text("ipv6_address", "TUN IPv6 地址（可选）", "", false),
                number("ipv6_prefix", "TUN IPv6 地址前缀", 126, 0, 128),
                number("mtu", "TUN MTU", 1500, 1280, 9000),
                Spec {
                    kind: Type::StringList,
                    ..text("routes", "接管的 CIDR 列表", "[]", false)
                },
                Spec {
                    kind: Type::StringList,
                    ..text(
                        "bypass",
                        "经原网关绕过的 CIDR 列表（含节点 IP 和 DNS）",
                        "[]",
                        false,
                    )
                },
                text("gateway", "原出口网关", "", false),
                text("gateway6", "原出口 IPv6 网关（可选）", "", false),
                text(
                    "interface",
                    "原出口接口（如 en0 / eth0 / Ethernet 或 Windows 索引）",
                    "",
                    false,
                ),
            ]
        } else if matches!(last_key(path), Some("tproxy" | "redirect")) {
            vec![text(
                "listen",
                "Linux 透明代理监听地址",
                "0.0.0.0:7893",
                true,
            )]
        } else if matches!(path.last(), Some(Part::Index(_))) && last_key(path) == Some("rules") {
            vec![
                choice("kind", "匹配类型", "domain_suffix", RULE_KINDS, true),
                text("value", "匹配内容", "example.com", true),
                text("action", "路由动作", "direct", true),
            ]
        } else if matches!(path.last(), Some(Part::Index(_))) && last_key(path) == Some("outbounds")
        {
            let mut f = vec![
                choice(
                    "kind",
                    "节点类型",
                    "socks5",
                    &["socks5", "http", "native", "trojan_tls", "vless_tls"],
                    true,
                ),
                text("tag", "节点标签", "edge", true),
            ];
            match get("kind") {
                "native" => {}
                "trojan_tls" | "vless_tls" => {
                    f.extend([
                        text(
                            "server",
                            "服务器（主机:端口）",
                            "node.example.com:443",
                            true,
                        ),
                        text("sni", "TLS 服务器名", "node.example.com", true),
                    ]);
                    f.push(if get("kind") == "trojan_tls" {
                        text("password", "密码", "", true)
                    } else {
                        text("uuid", "UUID", "", true)
                    });
                }
                _ => f.extend([
                    text("server", "服务器（主机:端口）", "127.0.0.1:7890", true),
                    text("username", "用户名", "", false),
                    text("password", "密码", "", false),
                ]),
            }
            f
        } else if last_key(path) == Some("config") {
            let protocol = get("protocol");
            let mut f = vec![
                choice("protocol", "代理协议", "vless", PROTOCOLS, true),
                text("server", "服务器主机", "node.example.com", true),
                number("port", "服务器端口", 443, 1, 65535),
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
                f.push(text("password", "密码", "", true));
            }
            match protocol {
                "vless" => f.extend([
                    choice("flow", "Vision 流控", "", &["", "xtls-rprx-vision"], false),
                    text(
                        "encryption",
                        "VLESS Encryption（完整字符串）",
                        "none",
                        false,
                    ),
                ]),
                "vmess" => f.push(choice(
                    "cipher",
                    "加密方式",
                    "auto",
                    &["auto", "aes-128-gcm", "chacha20-poly1305"],
                    false,
                )),
                "shadowsocks" => f.push(text("cipher", "加密方式", "aes-128-gcm", true)),
                "hysteria2" => f.extend([
                    text("obfs_password", "Salamander 混淆密码", "", false),
                    text("pin_sha256", "证书 SHA-256 固定值", "", false),
                ]),
                "tuic" => f.extend([
                    choice(
                        "tuic_udp_mode",
                        "UDP 模式",
                        "native",
                        &["native", "quic"],
                        false,
                    ),
                    number("tuic_idle_timeout_secs", "空闲超时（秒）", 15, 3, 3600),
                    choice(
                        "congestion_control",
                        "拥塞控制",
                        "cubic",
                        &["cubic", "bbr", "new_reno"],
                        false,
                    ),
                ]),
                _ => {}
            }
            if matches!(protocol, "vless" | "vmess" | "shadowsocks") {
                f.push(text("mux", "多路复用协议（可选）", "", false));
            }
            f
        } else if last_key(path) == Some("tls") {
            vec![
                text("server_name", "TLS 服务器名", "node.example.com", true),
                text("ca_file", "自定义 CA 文件", "", false),
                choice(
                    "fingerprint",
                    "客户端 TLS 指纹",
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
                text("cert_pin_sha256", "服务端证书 SHA-256 指纹", "", false),
                text("reality_public_key", "REALITY 公钥", "", false),
                text("reality_short_id", "REALITY Short ID", "", false),
            ]
        } else if last_key(path) == Some("transport") {
            let mut f = vec![
                choice(
                    "kind",
                    "传输方式",
                    "tcp",
                    &["tcp", "ws", "grpc", "http_upgrade", "xhttp"],
                    false,
                ),
                text("path", "请求路径", "/", false),
                text("host", "Host / Authority", "", false),
                text("service_name", "gRPC 服务名", "GunService", false),
            ];
            if get("kind") == "xhttp" {
                f.push(choice(
                    "mode",
                    "XHTTP 模式",
                    "stream-one",
                    &["stream-one", "stream-up", "packet-up", "auto"],
                    false,
                ));
            }
            f
        } else {
            vec![]
        };
        for field in &mut fields {
            if field.key == "port" && last_key(path) == Some("config") {
                field.required = true;
            }
            field.help = match field.key.as_str() {
                "action" | "default_action" => {
                    "direct 直连；block 拦截；proxy:节点标签 使用指定出站".into()
                }
                "kind" if last_key(path) == Some("rules") => {
                    "↑↓ 选择匹配类型；更改类型后请检查匹配内容；规则按列表顺序生效".into()
                }
                "kind" if last_key(path) == Some("transport") => {
                    "↑↓ 选择传输方式；切换方式会移除不适用的模式和请求头".into()
                }
                "kind" => "↑↓ 选择；切换节点类型会重置专用字段并保留标签，保存前检查凭据".into(),
                "protocol" => {
                    "↑↓ 选择；保留可复用参数，重置不适用的协议字段，保存前检查凭据".into()
                }
                "standalone" => {
                    "true：全部 TCP/UDP 入站使用固定出站，不连接 Rule Core；false：使用规则分流"
                        .into()
                }
                "standalone_outbound" => {
                    "填写节点标签；仅有一个节点时可省略，多个节点时必须指定；需启用 standalone"
                        .into()
                }
                "port" => "输入 1–65535 的整数".into(),
                "encryption" => {
                    "填写完整 mlkem768x25519plus… 字符串；留空或 none 不启用；不是 cipher".into()
                }
                "fingerprint" => "客户端 ClientHello 样式，例如 chrome；不是服务端证书哈希".into(),
                "cert_pin_sha256" => {
                    "服务端证书 SHA-256：64 位十六进制；替代 CA 验证，不与自定义 CA / REALITY 合用"
                        .into()
                }
                "mode" if last_key(path) == Some("transport") => {
                    "auto：普通 TLS 选择 packet-up，REALITY 选择 stream-one；省略默认 stream-one"
                        .into()
                }
                _ => {
                    if field.required {
                        "必填项".into()
                    } else {
                        "可选项；清空或按 d 可移除，使用 core 默认值".into()
                    }
                }
            };
        }
        let mut rows: Vec<Row> = fields.into_iter().map(Row::Field).collect();
        let groups: Vec<(&str, &str)> = if path.is_empty() {
            if self.is_proxy(doc) {
                vec![
                    ("system_proxy", "系统代理（SOCKS）"),
                    ("tun", "TUN 接管"),
                    ("tproxy", "Linux TPROXY（TCP/UDP）"),
                    ("redirect", "Linux REDIRECT（TCP）"),
                    ("outbounds", "出站节点"),
                ]
            } else {
                vec![("rules", "路由规则（按顺序匹配）")]
            }
        } else if get("kind") == "native" && matches!(path.last(), Some(Part::Index(_))) {
            vec![("config", "协议参数")]
        } else if last_key(path) == Some("config") {
            vec![("tls", "TLS / REALITY"), ("transport", "传输参数")]
        } else if last_key(path) == Some("transport") {
            vec![("headers", "自定义请求头")]
        } else {
            vec![]
        };
        rows.extend(groups.into_iter().map(|(key, label)| Row::Group {
            key: key.into(),
            label: label.into(),
        }));
        if let Some(table) = table {
            for (key, item) in table.iter() {
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
    fn rows(&self, doc: &DocumentMut) -> Vec<Row> {
        self.schema_rows(doc, &self.form.path)
    }
    fn expand_node(&mut self, index: usize) {
        if !self.form.expanded_nodes.contains(&index) {
            self.form.expanded_nodes.push(index);
        }
    }
    fn visible_rows(&self, doc: &DocumentMut) -> Vec<FlatRow> {
        let mut rows = Vec::new();
        self.flatten_table(doc, &[], "", &mut rows);
        rows
    }
    fn flatten_table(
        &self,
        doc: &DocumentMut,
        path: &[Part],
        prefix: &str,
        output: &mut Vec<FlatRow>,
    ) {
        if path.len() > 32 {
            return;
        }
        for row in self.schema_rows(doc, path) {
            match row {
                Row::Field(spec) => output.push(FlatRow::Field {
                    path: path.to_vec(),
                    spec,
                    prefix: prefix.into(),
                }),
                Row::Group { key, label } => {
                    let mut child = path.to_vec();
                    child.push(Part::Key(key.clone()));
                    if path.is_empty() && matches!(key.as_str(), "rules" | "outbounds") {
                        output.push(FlatRow::Collection {
                            path: child.clone(),
                            label,
                        });
                        if let Some(array) = collection(doc, &child) {
                            for (index, table) in array.iter().enumerate() {
                                let mut item = child.clone();
                                item.push(Part::Index(index));
                                if key == "outbounds" {
                                    let tag = table.get("tag").map(scalar).unwrap_or_default();
                                    let kind = table
                                        .get("config")
                                        .and_then(Item::as_table_like)
                                        .and_then(|c| c.get("protocol"))
                                        .or_else(|| table.get("kind"))
                                        .map(scalar)
                                        .unwrap_or_default();
                                    output.push(FlatRow::Node {
                                        index,
                                        label: format!("节点 {} · {} [{}]", index + 1, tag, kind),
                                    });
                                    if self.form.expanded_nodes.contains(&index) {
                                        self.flatten_table(doc, &item, "", output);
                                    }
                                } else {
                                    let label = format!(
                                        "规则 {} · {} {} → {}",
                                        index + 1,
                                        table.get("kind").map(scalar).unwrap_or_default(),
                                        table.get("value").map(scalar).unwrap_or_default(),
                                        table.get("action").map(scalar).unwrap_or_default()
                                    );
                                    output.push(FlatRow::Rule { index, label });
                                    self.flatten_table(
                                        doc,
                                        &item,
                                        &format!("规则 {}", index + 1),
                                        output,
                                    );
                                }
                            }
                        }
                    } else {
                        output.push(FlatRow::Section {
                            path: child.clone(),
                            label: label.clone(),
                        });
                        let field_prefix = match key.as_str() {
                            "config" => "协议",
                            "tls" => "TLS",
                            "transport" => "传输",
                            "headers" => "请求头",
                            _ => &label,
                        };
                        self.flatten_table(doc, &child, field_prefix, output);
                        if key == "headers" {
                            output.push(FlatRow::AddHeader { path: child });
                        }
                    }
                }
                Row::Entry { .. } => {}
            }
        }
    }
    fn focus_field(&mut self, doc: &DocumentMut, path: &[Part], key: &str) {
        if let Some(index) = node_index(path) {
            self.expand_node(index);
        }
        if let Some(selected) = self.visible_rows(doc).iter().position(
            |r| matches!(r,FlatRow::Field{path:p,spec,..} if p == path && spec.key == key),
        ) {
            self.form.selected = selected;
        }
    }
    fn focus_item(&mut self, doc: &DocumentMut, kind: &str, index: usize) {
        if let Some(selected) = self.visible_rows(doc).iter().position(|r| match r {
            FlatRow::Node { index: i, .. } => kind == "outbounds" && *i == index,
            FlatRow::Rule { index: i, .. } => kind == "rules" && *i == index,
            _ => false,
        }) {
            self.form.selected = selected;
        }
    }
    fn collapse_node(&mut self, doc: &DocumentMut, index: usize) {
        self.form.expanded_nodes.retain(|i| *i != index);
        self.focus_item(doc, "outbounds", index);
    }
    fn current_spec(&self, doc: &DocumentMut, key: &str) -> Result<Spec> {
        self.rows(doc)
            .into_iter()
            .find_map(|r| {
                if let Row::Field(s) = r {
                    if s.key == key {
                        Some(s)
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .context("字段已不存在")
    }
    fn commit_field(&mut self) -> Result<()> {
        let Mode::Field { key, input, .. } = &self.form.mode else {
            return Ok(());
        };
        let key = key.clone();
        let input = input.text.clone();
        let mut doc = self.document()?;
        let spec = self.current_spec(&doc, &key)?;
        let remove = input.is_empty() && !spec.required;
        let item = match &spec.kind {
            Type::Number(min, max) if !remove => {
                let n: i64 = input.parse().context("请输入整数")?;
                anyhow::ensure!(
                    (*min..=*max).contains(&n),
                    "{} 必须在 {}–{} 之间",
                    spec.label,
                    min,
                    max
                );
                value(n)
            }
            Type::Choice(choices) if !remove => {
                anyhow::ensure!(
                    choices.contains(&input.as_str()),
                    "请用 ↑↓ 选择支持的值：{}",
                    choices.join(", ")
                );
                value(&input)
            }
            Type::StringList if !remove => {
                let parsed: DocumentMut = format!("values = {input}")
                    .parse()
                    .context("请输入 TOML 字符串列表，例如 [\"en0\", \"eth0\"]")?;
                let array = parsed["values"].as_array().context("必须是字符串列表")?;
                anyhow::ensure!(
                    array.iter().all(|value| value.as_str().is_some()),
                    "列表中的值必须是字符串"
                );
                Item::Value(toml_edit::Value::Array(array.clone()))
            }
            Type::Boolean if !remove => {
                value(input.parse::<bool>().context("请选择 true 或 false")?)
            }
            _ => {
                anyhow::ensure!(
                    !spec.required || !input.trim().is_empty(),
                    "{} 不能为空",
                    spec.label
                );
                value(&input)
            }
        };
        if !remove
            && last_key(&self.form.path) == Some("tls")
            && table_at(doc.as_table(), &self.form.path).is_none()
        {
            let parent = &self.form.path[..self.form.path.len() - 1];
            let server = table_at(doc.as_table(), parent)
                .and_then(|t| t.get("server"))
                .and_then(Item::as_str)
                .unwrap_or("node.example.com")
                .to_owned();
            table_at_mut(doc.as_table_mut(), &self.form.path)?.insert("server_name", value(server));
        }
        let table = table_at_mut(doc.as_table_mut(), &self.form.path)?;
        if remove {
            table.remove(&key);
        } else {
            // Retain the existing field's whitespace and trailing comment.
            let mut item = item;
            if let (Some(old), Some(new)) = (
                table.get(&key).and_then(Item::as_value),
                item.as_value_mut(),
            ) {
                *new.decor_mut() = old.decor().clone();
            }
            let previous = table.get(&key).map(scalar).unwrap_or_default();
            if key == "kind"
                && matches!(self.form.path.last(), Some(Part::Index(_)))
                && last_key(&self.form.path) == Some("outbounds")
                && previous != input
            {
                let tag = table.get("tag").cloned();
                let replacement = outbound_template(&input, "edge");
                let keys: Vec<String> = table.iter().map(|(k, _)| k.to_owned()).collect();
                for k in keys {
                    table.remove(&k);
                }
                for (k, v) in replacement.iter() {
                    table.insert(k, v.clone());
                }
                if let Some(tag) = tag {
                    table.insert("tag", tag);
                }
            } else if key == "protocol"
                && last_key(&self.form.path) == Some("config")
                && previous != input
            {
                let shared: Vec<_> = ["server", "port", "uuid", "password"]
                    .into_iter()
                    .filter_map(|key| table.get(key).cloned().map(|v| (key, v)))
                    .collect();
                let tls = table.get("tls").cloned();
                let mut replacement = native_config(&input);
                for (key, item) in shared {
                    if replacement.contains_key(key) {
                        replacement.insert(key, item);
                    }
                }
                if input != "shadowsocks" {
                    if let Some(tls) = tls {
                        replacement.insert("tls", tls);
                    } else if let Some(server) = replacement
                        .get("server")
                        .and_then(Item::as_str)
                        .map(str::to_owned)
                    {
                        replacement["tls"]["server_name"] = value(server);
                    }
                }
                let keys: Vec<String> = table.iter().map(|(k, _)| k.to_owned()).collect();
                for k in keys {
                    table.remove(&k);
                }
                for (k, v) in replacement.iter() {
                    table.insert(k, v.clone());
                }
            } else {
                replace_field(table, &key, item);
                if key == "kind" && last_key(&self.form.path) == Some("transport") {
                    if input != "xhttp" {
                        table.remove("mode");
                    }
                    if matches!(input.as_str(), "tcp" | "grpc") {
                        table.remove("headers");
                    }
                }
                if key == "kind"
                    && last_key(&self.form.path) == Some("rules")
                    && previous != input
                    && !(previous.starts_with("domain_") && input.starts_with("domain_"))
                {
                    let default = match input.as_str() {
                        "port" => "443",
                        "network" => "tcp",
                        "ip_cidr" => "10.0.0.0/8",
                        _ => "example.com",
                    };
                    replace_field(table, "value", value(default));
                }
            }
        }
        self.update(doc);
        self.form.mode = Mode::Browse;
        let path = self.form.path.clone();
        let doc = self.document()?;
        self.focus_field(&doc, &path, &key);
        Ok(())
    }
    fn enter(&mut self, expand_only: bool) -> Result<()> {
        let mut doc = self.document()?;
        let Some(row) = self.visible_rows(&doc).get(self.form.selected).cloned() else {
            return Ok(());
        };
        match row {
            FlatRow::Field { path, spec, .. } => {
                let current = table_at(doc.as_table(), &path)
                    .and_then(|t| t.get(&spec.key))
                    .map(scalar)
                    .unwrap_or(spec.default);
                self.form.path = path;
                self.form.mode = Mode::Field {
                    key: spec.key,
                    input: TextInput::new(current),
                    reveal: false,
                };
            }
            FlatRow::Node { index, .. } => {
                if self.form.expanded_nodes.contains(&index) && !expand_only {
                    self.collapse_node(&doc, index);
                } else {
                    self.expand_node(index);
                }
            }
            FlatRow::AddHeader { path } => {
                self.form.path = path;
                self.form.mode = Mode::Header(TextInput::default());
            }
            FlatRow::Section { path, .. }
                if matches!(
                    last_key(&path),
                    Some("tls" | "system_proxy" | "tun" | "tproxy" | "redirect")
                ) =>
            {
                let (Part::Key(key), parent) = path.split_last().context("TLS 参数路径无效")?
                else {
                    bail!("TLS 参数路径无效");
                };
                let server = table_at(doc.as_table(), parent)
                    .and_then(|t| t.get("server"))
                    .and_then(Item::as_str)
                    .unwrap_or("node.example.com")
                    .to_owned();
                let table = table_at_mut(doc.as_table_mut(), parent)?;
                if table.get(key).is_some() {
                    table.remove(key);
                } else {
                    let mut tls = Table::new();
                    if key == "tls" {
                        tls["server_name"] = value(server);
                    }
                    if key == "tproxy" || key == "redirect" {
                        tls["listen"] = value("0.0.0.0:7893");
                    }
                    table.insert(key, Item::Table(tls));
                }
                self.update(doc);
            }
            FlatRow::Collection { .. } | FlatRow::Rule { .. } => {
                self.form.selected =
                    (self.form.selected + 1).min(self.visible_rows(&doc).len().saturating_sub(1));
            }
            FlatRow::Section { .. } => {}
        }
        Ok(())
    }
    fn append(&mut self, mut table: Table) -> Result<()> {
        let mut doc = self.document()?;
        let array = collection_mut(&mut doc, &self.form.path)?;
        if last_key(&self.form.path) == Some("outbounds") {
            let mut n = array.len() + 1;
            while array
                .iter()
                .any(|t| t.get("tag").and_then(Item::as_str) == Some(format!("edge-{n}").as_str()))
            {
                n += 1;
            }
            table["tag"] = value(format!("edge-{n}"));
        }
        let index = array.len();
        array.push(table);
        self.update(doc);
        self.form.mode = Mode::Browse;
        self.form.path.push(Part::Index(index));
        let path = self.form.path.clone();
        let doc = self.document()?;
        self.focus_field(&doc, &path, "kind");
        Ok(())
    }
    pub fn key(&mut self, key: KeyEvent) -> Result<()> {
        if matches!(self.form.mode, Mode::Field { .. }) {
            if key.code == KeyCode::Esc {
                self.cancel_input();
                return Ok(());
            }
            if key.code == KeyCode::Enter {
                return self.commit_field();
            }
            let doc = self.document()?;
            let Mode::Field {
                key: field,
                input: _,
                reveal,
            } = &mut self.form.mode
            else {
                unreachable!()
            };
            let field = field.clone();
            if key.code == KeyCode::F(2) {
                *reveal = !*reveal;
                return Ok(());
            }
            if matches!(key.code, KeyCode::Up | KeyCode::Down) {
                let spec = self.current_spec(&doc, &field)?;
                let choices: Vec<&str> = match spec.kind {
                    Type::Choice(c) => c.to_vec(),
                    Type::Boolean => vec!["false", "true"],
                    _ => vec![],
                };
                if !choices.is_empty() {
                    let Mode::Field { input, .. } = &mut self.form.mode else {
                        unreachable!()
                    };
                    let at = choices.iter().position(|c| *c == input.text).unwrap_or(0);
                    let next = if key.code == KeyCode::Up {
                        (at + choices.len() - 1) % choices.len()
                    } else {
                        (at + 1) % choices.len()
                    };
                    *input = TextInput::new(choices[next].into());
                    return Ok(());
                }
            }
            let Mode::Field { input, .. } = &mut self.form.mode else {
                unreachable!()
            };
            input.key(key);
            return Ok(());
        }
        if let Mode::Header(input) = &mut self.form.mode {
            match key.code {
                KeyCode::Esc => self.cancel_input(),
                KeyCode::Enter => {
                    let key = input.text.trim().to_owned();
                    anyhow::ensure!(
                        !key.is_empty()
                            && key
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric()
                                    || b"!#$%&'*+-.^_`|~".contains(&b)),
                        "请输入有效的 HTTP 请求头名称"
                    );
                    let mut doc = self.document()?;
                    let table = table_at_mut(doc.as_table_mut(), &self.form.path)?;
                    anyhow::ensure!(table.get(&key).is_none(), "该请求头已存在");
                    table.insert(&key, value(""));
                    self.update(doc);
                    let path = self.form.path.clone();
                    let doc = self.document()?;
                    self.focus_field(&doc, &path, &key);
                    self.form.mode = Mode::Field {
                        key,
                        input: TextInput::new(String::new()),
                        reveal: false,
                    };
                }
                _ => input.key(key),
            }
            return Ok(());
        }
        if let Mode::NewOutbound(selected) = &mut self.form.mode {
            match key.code {
                KeyCode::Esc => self.cancel_input(),
                KeyCode::Up => *selected = selected.saturating_sub(1),
                KeyCode::Down => *selected = (*selected + 1).min(OUTBOUND_KINDS.len() - 1),
                KeyCode::Enter => {
                    let kind = [
                        "socks5",
                        "http",
                        "native:vless",
                        "native:vmess",
                        "native:shadowsocks",
                        "native:trojan",
                        "native:hysteria2",
                        "native:tuic",
                        "trojan_tls",
                        "vless_tls",
                    ][*selected];
                    self.append(outbound_template(kind, "edge"))?;
                }
                _ => {}
            }
            return Ok(());
        }
        if let Mode::Delete { key: field, index } = &self.form.mode {
            match key.code {
                KeyCode::Esc | KeyCode::Char('n') => self.cancel_input(),
                KeyCode::Char('y') => {
                    let mut doc = self.document()?;
                    if let Some(index) = index {
                        collection_mut(&mut doc, &self.form.path)?.remove(*index);
                        if self.form.path == vec![Part::Key("outbounds".into())] {
                            let removed = *index;
                            self.form.expanded_nodes = self
                                .form
                                .expanded_nodes
                                .iter()
                                .filter_map(|i| {
                                    if *i == removed {
                                        None
                                    } else {
                                        Some(if *i > removed { *i - 1 } else { *i })
                                    }
                                })
                                .collect();
                        }
                    } else if let Some(field) = field {
                        if table_at(doc.as_table(), &self.form.path)
                            .and_then(|t| t.get(field))
                            .is_some()
                        {
                            table_at_mut(doc.as_table_mut(), &self.form.path)?.remove(field);
                        }
                    }
                    self.update(doc);
                    self.cancel_input();
                    self.form.selected = self
                        .form
                        .selected
                        .min(self.visible_rows(&self.document()?).len().saturating_sub(1));
                }
                _ => {}
            }
            return Ok(());
        }
        let doc = self.document()?;
        let rows = self.visible_rows(&doc);
        let selected = rows.get(self.form.selected).cloned();
        match key.code {
            KeyCode::Up => self.form.selected = self.form.selected.saturating_sub(1),
            KeyCode::Down => {
                self.form.selected = (self.form.selected + 1).min(rows.len().saturating_sub(1))
            }
            KeyCode::PageUp => self.form.selected = self.form.selected.saturating_sub(10),
            KeyCode::PageDown => {
                self.form.selected = (self.form.selected + 10).min(rows.len().saturating_sub(1))
            }
            KeyCode::Home => self.form.selected = 0,
            KeyCode::End => self.form.selected = rows.len().saturating_sub(1),
            KeyCode::Enter | KeyCode::Right => self.enter(key.code == KeyCode::Right)?,
            KeyCode::Esc | KeyCode::Left => {
                if let Some(index) = selected.as_ref().and_then(FlatRow::node) {
                    self.collapse_node(&doc, index);
                }
            }
            KeyCode::Char('n') => {
                let path = selected.as_ref().map(FlatRow::path).unwrap_or_default();
                if last_key(&path) == Some("headers") {
                    self.form.path = path;
                    self.form.mode = Mode::Header(TextInput::default());
                } else if self.is_proxy(&doc) {
                    self.form.path = vec![Part::Key("outbounds".into())];
                    self.form.mode = Mode::NewOutbound(0);
                } else {
                    self.form.path = vec![Part::Key("rules".into())];
                    let mut table = Table::new();
                    table["kind"] = value("domain_suffix");
                    table["value"] = value("example.com");
                    table["action"] = value("direct");
                    self.append(table)?;
                }
            }
            KeyCode::Char('d') => match selected {
                Some(FlatRow::Field { path, spec, .. }) => {
                    anyhow::ensure!(!spec.required, "必填项不能删除，请按 Enter 修改");
                    self.form.path = path;
                    self.form.mode = Mode::Delete {
                        key: Some(spec.key),
                        index: None,
                    };
                }
                Some(FlatRow::Node { index, .. }) => {
                    self.form.path = vec![Part::Key("outbounds".into())];
                    self.form.mode = Mode::Delete {
                        key: None,
                        index: Some(index),
                    };
                }
                Some(FlatRow::Rule { index, .. }) => {
                    self.form.path = vec![Part::Key("rules".into())];
                    self.form.mode = Mode::Delete {
                        key: None,
                        index: Some(index),
                    };
                }
                Some(FlatRow::Section { mut path, .. } | FlatRow::Collection { mut path, .. }) => {
                    if let Some(Part::Key(key)) = path.pop() {
                        self.form.path = path;
                        self.form.mode = Mode::Delete {
                            key: Some(key),
                            index: None,
                        };
                    }
                }
                _ => {}
            },
            KeyCode::Char('K') | KeyCode::Char('J') => {
                let path = selected.as_ref().map(FlatRow::path).unwrap_or_default();
                let (kind, index) = item_index(&path).context("请先选择要调整顺序的规则或节点")?;
                let array_path = vec![Part::Key(kind.into())];
                let mut doc = self.document()?;
                let array = collection_mut(&mut doc, &array_path)?;
                let next = if key.code == KeyCode::Char('K') {
                    index.saturating_sub(1)
                } else {
                    (index + 1).min(array.len().saturating_sub(1))
                };
                let mut tables: Vec<Table> = array.iter().cloned().collect();
                tables.swap(index, next);
                *array = tables.into_iter().collect();
                if kind == "outbounds" {
                    for expanded in &mut self.form.expanded_nodes {
                        if *expanded == index {
                            *expanded = next;
                        } else if *expanded == next {
                            *expanded = index;
                        }
                    }
                }
                self.update(doc);
                let doc = self.document()?;
                self.focus_item(&doc, kind, next);
            }
            _ => {}
        }
        Ok(())
    }
    pub fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let dirty = if self.text() != self.original {
            " *未保存"
        } else {
            ""
        };
        let title = format!("条目编辑 · {}{} · 单页 / 仅节点折叠", self.name, dirty);
        let Ok(doc) = self.document() else {
            frame.render_widget(
                Paragraph::new(
                    "配置格式无法解析，草稿已保留。请修复配置文件后重新打开。Esc 返回。",
                )
                .wrap(Wrap { trim: false })
                .block(Block::default().title(title).borders(Borders::ALL)),
                area,
            );
            return;
        };
        let rows = self.visible_rows(&doc);
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(3)])
            .split(area);
        let items: Vec<ListItem> = rows
            .iter()
            .map(|row| {
                let (content, style) = match row {
                    FlatRow::Field { path, spec, prefix } => {
                        let table = table_at(doc.as_table(), path);
                        let current = table.and_then(|t| t.get(&spec.key));
                        let tls_disabled = last_key(path) == Some("tls") && table.is_none();
                        let text = if tls_disabled {
                            "[TLS 未启用；可直接填写或在 TLS 行启用]".into()
                        } else if let Some(item) = current {
                            let text = scalar(item);
                            if spec.secret() && !text.is_empty() {
                                "••••••••".into()
                            } else {
                                text.replace('\n', " ↵ ")
                            }
                        } else if spec.required {
                            format!(
                                "[必填，尚未设置；建议 {}]",
                                if spec.secret() {
                                    "输入凭据"
                                } else {
                                    &spec.default
                                }
                            )
                        } else {
                            format!(
                                "[未设置{}]",
                                if spec.default.is_empty() {
                                    String::new()
                                } else {
                                    format!("，默认 {}", spec.default)
                                }
                            )
                        };
                        let prefix = if prefix.is_empty() {
                            String::new()
                        } else {
                            format!("{prefix} / ")
                        };
                        (
                            format!(
                                "{}{}{}{}  :  {}",
                                if node_index(path).is_some() { "  " } else { "" },
                                prefix,
                                spec.label,
                                if spec.required && !tls_disabled {
                                    " *"
                                } else {
                                    ""
                                },
                                text
                            ),
                            Style::default(),
                        )
                    }
                    FlatRow::Collection { path, label } => {
                        let count = collection(&doc, path).map(|a| a.len()).unwrap_or(0);
                        (
                            format!("── {label}（{count} 条） · n 新增 ──"),
                            Style::default().fg(Color::Cyan),
                        )
                    }
                    FlatRow::Node { index, label } => (
                        format!(
                            "{} {label}",
                            if self.form.expanded_nodes.contains(index) {
                                "▾"
                            } else {
                                "▸"
                            }
                        ),
                        Style::default().fg(Color::Cyan),
                    ),
                    FlatRow::Rule { label, .. } => {
                        (format!("── {label} ──"), Style::default().fg(Color::Cyan))
                    }
                    FlatRow::Section { path, label } => {
                        let suffix = if matches!(
                            last_key(path),
                            Some("tls" | "system_proxy" | "tun" | "tproxy" | "redirect")
                        ) {
                            if table_at(doc.as_table(), path).is_some() {
                                " · 已启用 / Enter 关闭"
                            } else {
                                " · 未启用 / Enter 启用"
                            }
                        } else {
                            ""
                        };
                        (
                            format!("  ── {label}{suffix} ──"),
                            Style::default().fg(Color::Gray),
                        )
                    }
                    FlatRow::AddHeader { .. } => (
                        "  ＋ 新增请求头（Enter）".into(),
                        Style::default().fg(Color::Cyan),
                    ),
                };
                ListItem::new(content).style(style)
            })
            .collect();
        let mut state = ListState::default();
        if !items.is_empty() {
            let selected = self.form.selected.min(items.len() - 1);
            state = state.with_offset(
                selected.saturating_sub(sections[0].height.saturating_sub(2) as usize / 3),
            );
            state.select(Some(selected));
        }
        frame.render_stateful_widget(
            List::new(items)
                .highlight_symbol("› ")
                .highlight_style(Style::default().bg(Color::Blue))
                .block(Block::default().title(title).borders(Borders::ALL)),
            sections[0],
            &mut state,
        );
        let hint = match rows.get(self.form.selected) {
            Some(FlatRow::Field { spec, .. }) => format!("{} · {}", spec.key, spec.help),
            Some(FlatRow::Node { .. }) => {
                "Enter 展开/收起节点；展开后全部参数平铺；d 删除；K/J 调整顺序".into()
            }
            Some(FlatRow::Rule { .. }) => {
                "规则字段已在下方平铺；d 删除整条规则；K/J 上移/下移".into()
            }
            Some(FlatRow::Section { path, .. }) if last_key(path) == Some("tls") => {
                "Enter 启用/关闭 TLS；字段始终平铺；d 删除本分组参数".into()
            }
            Some(FlatRow::AddHeader { .. }) => "Enter 新增请求头；已有请求头直接在上方修改".into(),
            _ => "全部参数在同一页；↑↓ 选择，Enter 编辑，n 新增，Ctrl-S 校验保存".into(),
        };
        frame.render_widget(
            Paragraph::new(hint)
                .wrap(Wrap { trim: false })
                .block(Block::default().borders(Borders::ALL)),
            sections[1],
        );
        if !matches!(self.form.mode, Mode::Browse) {
            let height = match &self.form.mode {
                Mode::NewOutbound(_) => 15,
                Mode::Field { key, .. } => self
                    .current_spec(&doc, key)
                    .ok()
                    .map(|s| {
                        if let Type::Choice(c) = s.kind {
                            c.len() as u16 + 7
                        } else {
                            8
                        }
                    })
                    .unwrap_or(8),
                _ => 7,
            };
            let popup = Rect {
                x: area.x + 2.min(area.width / 2),
                y: area.y + area.height.saturating_sub(height) / 2,
                width: area.width.saturating_sub(4),
                height: height.min(area.height),
            };
            frame.render_widget(Clear, popup);
            match &self.form.mode {
                Mode::Field{key,input,reveal} => {
                    let spec = self.current_spec(&doc,key).ok();
                    let secret = spec.as_ref().is_some_and(Spec::secret) && !reveal;
                    let display = if secret { "•".repeat(input.text.chars().count()) } else { input.text.replace('\n'," ↵ ") };
                    let mut display: Vec<char> = display.chars().collect(); display.insert(input.cursor.min(display.len()),'▏');
                    let choices = spec.as_ref().map(|s| if let Type::Choice(c) = &s.kind { c.iter().map(|c| format!("{} {}",if *c == input.text { "›" } else { " " },if c.is_empty() { "(未设置)" } else { c })).collect::<Vec<_>>().join("\n") } else { String::new() }).unwrap_or_default();
                    let text = format!("> {}\n{}\nEnter 确认 · Esc 取消 · ↑↓ 选择 · Ctrl-U 清空\nF2 显示/隐藏凭据；输入会替换选中值\n{}",display.iter().collect::<String>(),choices,spec.as_ref().map(|s|s.help.as_str()).unwrap_or(""));
                    frame.render_widget(Paragraph::new(text).wrap(Wrap{trim:false}).block(Block::default().title(spec.as_ref().map(|s|s.label.as_str()).unwrap_or(key)).borders(Borders::ALL).style(Style::default().fg(Color::Cyan))),popup);
                }
                Mode::NewOutbound(selected) => {
                    let mut state = ListState::default().with_selected(Some(*selected));
                    frame.render_stateful_widget(List::new(OUTBOUND_KINDS.iter().map(|s|ListItem::new(*s))).highlight_symbol("› ").highlight_style(Style::default().bg(Color::Blue)).block(Block::default().title("新增出站节点 · ↑↓ 选择 · Enter 确认 · Esc 取消").borders(Borders::ALL)),popup,&mut state);
                }
                Mode::Header(input) => frame.render_widget(Paragraph::new(format!("请求头名称：{}▏\nEnter 继续设置值 · Esc 取消",input.text)).block(Block::default().title("新增请求头").borders(Borders::ALL)),popup),
                Mode::Delete{..} => frame.render_widget(Paragraph::new("删除所选条目/分组？\n只修改草稿，Ctrl-S 后写入配置。\ny 确认删除 · n / Esc 取消").wrap(Wrap{trim:false}).block(Block::default().title("确认删除").borders(Borders::ALL).style(Style::default().fg(Color::Yellow))),popup),
                Mode::Browse => {}
            }
        }
    }
}

fn native_config(protocol: &str) -> Table {
    let mut table = Table::new();
    table["protocol"] = value(protocol);
    table["server"] = value("node.example.com");
    table["port"] = value(443);
    if matches!(protocol, "vless" | "vmess" | "tuic") {
        table["uuid"] = value("00000000-0000-0000-0000-000000000001");
    }
    if matches!(protocol, "shadowsocks" | "trojan" | "hysteria2" | "tuic") {
        table["password"] = value("change-me");
    }
    if protocol == "shadowsocks" {
        table["cipher"] = value("aes-128-gcm");
    }
    if protocol != "shadowsocks" {
        let mut tls = Table::new();
        tls["server_name"] = value("node.example.com");
        table["tls"] = Item::Table(tls);
    }
    table
}
fn outbound_template(kind: &str, tag: &str) -> Table {
    let mut table = Table::new();
    table["tag"] = value(tag);
    if kind.starts_with("native") {
        table["kind"] = value("native");
        table["config"] = Item::Table(native_config(
            kind.strip_prefix("native:").unwrap_or("vless"),
        ));
    } else {
        table["kind"] = value(kind);
        table["server"] = value(if matches!(kind, "socks5" | "http") {
            "127.0.0.1:7890"
        } else {
            "node.example.com:443"
        });
        if kind == "trojan_tls" {
            table["sni"] = value("node.example.com");
            table["password"] = value("change-me");
        }
        if kind == "vless_tls" {
            table["sni"] = value("node.example.com");
            table["uuid"] = value("00000000-0000-0000-0000-000000000001");
        }
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn press(editor: &mut Editor, code: KeyCode) {
        editor.key(key(code)).unwrap();
    }
    fn field(editor: &mut Editor, key: &str, text: &str) {
        let doc = editor.document().unwrap();
        let path = editor.form.path.clone();
        editor.focus_field(&doc, &path, key);
        assert!(
            matches!(editor.visible_rows(&doc).get(editor.form.selected), Some(FlatRow::Field{path:p,spec,..}) if p == &path && spec.key == key)
        );
        press(editor, KeyCode::Enter);
        editor.paste(text);
        press(editor, KeyCode::Enter);
    }
    fn rules() -> Editor {
        Editor::new(
            "rules".into(),
            include_str!("../../../configs/rule.toml").into(),
        )
    }
    fn native() -> Editor {
        Editor::new(
            "proxy".into(),
            include_str!("../../../configs/native.toml").into(),
        )
    }
    fn node_path() -> Path {
        vec![Part::Key("outbounds".into()), Part::Index(0)]
    }
    fn path(keys: &[&str]) -> Path {
        keys.iter().map(|k| Part::Key((*k).into())).collect()
    }
    #[test]
    fn proxy_modes_and_route_lists_keep_toml_types() {
        let mut e = native();
        field(&mut e, "watch_config", "true");
        e.form.path = path(&["tun"]);
        field(&mut e, "routes", "[\"0.0.0.0/1\", \"128.0.0.0/1\"]");
        field(&mut e, "interface", "en0");
        e.form.path = path(&["system_proxy"]);
        field(&mut e, "services", "[\"Wi-Fi\"]");
        let parsed: toml::Value = toml::from_str(&e.text()).unwrap();
        assert_eq!(parsed["watch_config"].as_bool(), Some(true));
        assert_eq!(parsed["tun"]["routes"].as_array().unwrap().len(), 2);
        assert_eq!(
            parsed["system_proxy"]["services"][0].as_str(),
            Some("Wi-Fi")
        );
    }

    #[test]
    fn standalone_without_rule_socket_is_editable_as_proxy() {
        let mut e = Editor::new(
            "custom".into(),
            "standalone = true\nsocks5_listen = '127.0.0.1:1080'\n".into(),
        );
        let doc = e.document().unwrap();
        assert!(e.is_proxy(&doc));
        assert!(e
            .schema_rows(&doc, &[])
            .iter()
            .any(|r| matches!(r, Row::Field(s) if s.key == "rule_socket" && !s.required)));
        field(&mut e, "standalone", "false");
        assert_eq!(e.document().unwrap()["standalone"].as_bool(), Some(false));
        field(&mut e, "standalone", "true");
        field(&mut e, "standalone_outbound", "selected");
        let parsed: toml::Value = toml::from_str(&e.text()).unwrap();
        assert_eq!(parsed["standalone"].as_bool(), Some(true));
        assert_eq!(parsed["standalone_outbound"].as_str(), Some("selected"));
        assert!(parsed.get("rule_socket").is_none());
    }

    #[test]
    fn vless_encryption_and_distinct_tls_pins_are_editable_and_round_trip() {
        let mut e = native();
        let mut config_path = node_path();
        config_path.push(Part::Key("config".into()));
        e.form.path = config_path.clone();
        let encryption =
            "mlkem768x25519plus.native.0rtt.AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA";
        field(&mut e, "encryption", encryption);
        let mut tls_path = config_path.clone();
        tls_path.push(Part::Key("tls".into()));
        e.form.path = tls_path.clone();
        field(&mut e, "fingerprint", "chrome");
        let pin = "11".repeat(32);
        field(&mut e, "cert_pin_sha256", &pin);
        let mut transport_path = config_path.clone();
        transport_path.push(Part::Key("transport".into()));
        e.form.path = transport_path.clone();
        field(&mut e, "mode", "auto");
        let parsed: toml::Value = toml::from_str(&e.text()).unwrap();
        let config = &parsed["outbounds"][0]["config"];
        assert_eq!(config["encryption"].as_str(), Some(encryption));
        assert_eq!(config["tls"]["fingerprint"].as_str(), Some("chrome"));
        assert_eq!(
            config["tls"]["cert_pin_sha256"].as_str(),
            Some(pin.as_str())
        );
        assert_eq!(config["transport"]["mode"].as_str(), Some("auto"));
        let restored = Editor::new("proxy".into(), e.text());
        let doc = restored.document().unwrap();
        let tls_fields = restored.schema_rows(&doc, &tls_path);
        assert!(tls_fields.iter().any(|r| matches!(r, Row::Field(spec) if spec.key == "fingerprint" && spec.label.contains("客户端"))));
        assert!(tls_fields.iter().any(|r| matches!(r, Row::Field(spec) if spec.key == "cert_pin_sha256" && spec.label.contains("服务端"))));
    }

    #[test]
    fn strings_are_escaped_and_comments_are_preserved() {
        let mut e = Editor::new(
            "rules".into(),
            "# keep header\nsocket = \"/tmp/a.sock\" # keep inline\ndefault_action = \"direct\"\n"
                .into(),
        );
        let original = e.text();
        assert_eq!(e.prepare_save().unwrap(), original);
        field(&mut e, "socket", "/tmp/中文 \"quoted\".sock");
        assert!(e.text().starts_with("# keep header\n"), "{}", e.text());
        assert!(e.text().contains("# keep inline"));
        assert_eq!(
            e.document().unwrap()["socket"].as_str(),
            Some("/tmp/中文 \"quoted\".sock")
        );
    }
    #[test]
    fn typed_numbers_reject_invalid_values_and_keep_pending_input_for_retry() {
        let mut e = Editor::new(
            "proxy".into(),
            include_str!("../../../configs/proxy.toml").into(),
        );
        let doc = e.document().unwrap();
        e.form.selected = e
            .rows(&doc)
            .iter()
            .position(|r| matches!(r,Row::Field(s) if s.key == "ipc_connections"))
            .unwrap();
        press(&mut e, KeyCode::Enter);
        e.paste("0");
        assert!(e.key(key(KeyCode::Enter)).is_err());
        assert_eq!(
            e.document().unwrap()["ipc_connections"].as_integer(),
            Some(8)
        );
        let snapshot = serde_json::to_vec(&e).unwrap();
        let mut e: Editor = serde_json::from_slice(&snapshot).unwrap();
        assert!(matches!(&e.form.mode,Mode::Field{input,..} if input.text == "0"));
        e.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL))
            .unwrap();
        e.paste("16");
        press(&mut e, KeyCode::Enter);
        assert_eq!(
            e.document().unwrap()["ipc_connections"].as_integer(),
            Some(16)
        );
    }
    #[test]
    fn rule_add_edit_reorder_and_confirm_delete_keep_matching_order() {
        let mut e = rules();
        e.form.path = path(&["rules"]);
        press(&mut e, KeyCode::Char('n'));
        field(&mut e, "value", "new.example");
        field(&mut e, "action", "proxy:edge");
        press(&mut e, KeyCode::Esc);
        press(&mut e, KeyCode::Char('K'));
        let doc = e.document().unwrap();
        let a = doc["rules"].as_array_of_tables().unwrap();
        assert_eq!(a.get(1).unwrap()["value"].as_str(), Some("new.example"));
        let before = e.text();
        press(&mut e, KeyCode::Char('d'));
        press(&mut e, KeyCode::Char('n'));
        assert_eq!(e.text(), before);
        press(&mut e, KeyCode::Char('d'));
        press(&mut e, KeyCode::Char('y'));
        assert_eq!(
            e.document().unwrap()["rules"]
                .as_array_of_tables()
                .unwrap()
                .len(),
            2
        );
    }
    #[test]
    fn new_outbound_wizard_and_protocol_switch_use_valid_field_shapes() {
        let mut e = Editor::new(
            "proxy".into(),
            include_str!("../../../configs/proxy.toml").into(),
        );
        e.form.path = path(&["outbounds"]);
        press(&mut e, KeyCode::Char('n'));
        press(&mut e, KeyCode::Down);
        press(&mut e, KeyCode::Down);
        press(&mut e, KeyCode::Enter);
        assert_eq!(
            e.document().unwrap()["outbounds"][0]["kind"].as_str(),
            Some("native")
        );
        e.form.path.push(Part::Key("config".into()));
        field(&mut e, "server", "my.node.example");
        field(&mut e, "protocol", "tuic");
        let doc = e.document().unwrap();
        let config = table_at(doc.as_table(), &e.form.path).unwrap();
        assert_eq!(
            config.get("server").unwrap().as_str(),
            Some("my.node.example")
        );
        assert_eq!(config.get("protocol").unwrap().as_str(), Some("tuic"));
        assert!(config.get("password").is_some());
        field(&mut e, "tuic_idle_timeout_secs", "30");
        field(&mut e, "protocol", "vless");
        let doc = e.document().unwrap();
        assert!(table_at(doc.as_table(), &e.form.path)
            .unwrap()
            .get("tuic_idle_timeout_secs")
            .is_none());
    }
    #[test]
    fn nested_tls_transport_headers_and_inline_tables_are_editable() {
        let mut e = native();
        e.form.path = node_path();
        e.form.path.extend(path(&["config", "tls"]));
        field(&mut e, "ca_file", "private CA.pem");
        e.form.path = node_path();
        e.form
            .path
            .extend(path(&["config", "transport", "headers"]));
        let doc = e.document().unwrap();
        e.form.selected = e
            .visible_rows(&doc)
            .iter()
            .position(|r| matches!(r,FlatRow::AddHeader{path} if path == &e.form.path))
            .unwrap();
        press(&mut e, KeyCode::Enter);
        e.paste("X-Custom");
        press(&mut e, KeyCode::Enter);
        e.paste("中文 header");
        press(&mut e, KeyCode::Enter);
        let doc = e.document().unwrap();
        assert_eq!(
            table_at(doc.as_table(), &e.form.path)
                .unwrap()
                .get("X-Custom")
                .unwrap()
                .as_str(),
            Some("中文 header")
        );
        assert!(e.text().contains("private CA.pem"));
        let mut inline = Editor::new("rules".into(), "socket = '/tmp/a'\ndefault_action = 'direct'\nrules = [{ kind = 'domain_exact', value = 'a.example', action = 'block' }]\n".into());
        inline.form.path = vec![Part::Key("rules".into()), Part::Index(0)];
        field(&mut inline, "value", "b.example");
        assert_eq!(
            inline.document().unwrap()["rules"][0]["value"].as_str(),
            Some("b.example")
        );
    }
    #[test]
    fn moving_native_nodes_keeps_each_nested_configuration_with_its_node() {
        let mut e = Editor::new(
            "proxy".into(),
            include_str!("../../../configs/proxy.toml").into(),
        );
        let mut doc = e.document().unwrap();
        let mut array = ArrayOfTables::new();
        for (tag, server) in [("first", "first.example"), ("second", "second.example")] {
            let mut t = outbound_template("native:vless", tag);
            t["config"]["server"] = value(server);
            t["config"]["tls"]["server_name"] = value(server);
            array.push(t);
        }
        doc["outbounds"] = Item::ArrayOfTables(array);
        e.update(doc);
        e.form.path = path(&["outbounds"]);
        let doc = e.document().unwrap();
        e.focus_item(&doc, "outbounds", 1);
        press(&mut e, KeyCode::Char('K'));
        let doc = e.document().unwrap();
        assert_eq!(doc["outbounds"][0]["tag"].as_str(), Some("second"));
        assert_eq!(
            doc["outbounds"][0]["config"]["server"].as_str(),
            Some("second.example")
        );
        assert_eq!(
            doc["outbounds"][0]["config"]["tls"]["server_name"].as_str(),
            Some("second.example")
        );
        assert_eq!(
            doc["outbounds"][1]["config"]["server"].as_str(),
            Some("first.example")
        );
    }
    #[test]
    fn optional_fields_delete_and_required_fields_remain_protected() {
        let mut e = native();
        e.form.path = node_path();
        e.form.path.extend(path(&["config"]));
        let doc = e.document().unwrap();
        let config_path = e.form.path.clone();
        e.focus_field(&doc, &config_path, "port");
        assert!(e.key(key(KeyCode::Char('d'))).is_err());
        let mut tls_path = config_path;
        tls_path.push(Part::Key("tls".into()));
        e.form.selected = e
            .visible_rows(&doc)
            .iter()
            .position(|r| matches!(r,FlatRow::Section{path,..} if path == &tls_path))
            .unwrap();
        press(&mut e, KeyCode::Char('d'));
        press(&mut e, KeyCode::Char('y'));
        assert!(table_at(e.document().unwrap().as_table(), &e.form.path)
            .unwrap()
            .get("tls")
            .is_none());
    }
    #[test]
    fn visual_form_masks_credentials_and_supports_small_terminals() {
        let mut e = native();
        e.form.path = node_path();
        e.form.path.extend(path(&["config"]));
        field(&mut e, "uuid", "private-credential-for-display-test");
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|f| e.render(f, f.area())).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.replace(' ', "").contains("条目编辑"));
        assert!(text.replace(' ', "").contains("服务器主机"));
        assert!(!text.contains("private-credential"));
        let doc = e.document().unwrap();
        let path = e.form.path.clone();
        e.focus_field(&doc, &path, "uuid");
        press(&mut e, KeyCode::Enter);
        terminal.draw(|f| e.render(f, f.area())).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(!text.contains("private-credential"));
        press(&mut e, KeyCode::F(2));
        terminal.draw(|f| e.render(f, f.area())).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("private-credential"));
        let mut tiny = Terminal::new(TestBackend::new(12, 6)).unwrap();
        tiny.draw(|f| e.render(f, f.area())).unwrap();
    }
    #[test]
    fn legacy_draft_migrates_to_forms_without_losing_source() {
        let old = serde_json::json!({"name":"rules","original":"socket = '/tmp/a'\ndefault_action = 'direct'\n","lines":["socket = '/tmp/b'","default_action = 'direct'", ""],"row":1,"col":3});
        let mut e: Editor = serde_json::from_value(old).unwrap();
        e.normalize();
        assert!(e.text().contains("/tmp/b"));
        assert!(e.original.contains("/tmp/a"));
        field(&mut e, "socket", "/tmp/c");
        assert_eq!(e.document().unwrap()["socket"].as_str(), Some("/tmp/c"));
    }
    #[test]
    fn rule_fields_are_all_visible_without_opening_any_menu() {
        let mut e = rules();
        let doc = e.document().unwrap();
        let rows = e.visible_rows(&doc);
        assert!(!rows.iter().any(|r| matches!(r, FlatRow::Node { .. })));
        for index in 0..2 {
            for key in ["kind", "value", "action"] {
                assert!(rows.iter().any(|r|matches!(r,FlatRow::Field{path,spec,..} if path == &vec![Part::Key("rules".into()),Part::Index(index)] && spec.key == key)));
            }
        }
        e.form.selected = rows
            .iter()
            .position(|r| matches!(r, FlatRow::Collection { .. }))
            .unwrap();
        press(&mut e, KeyCode::Enter);
        assert_eq!(e.visible_rows(&doc).len(), rows.len());
        assert!(e.form.path.is_empty());
        assert!(e.can_close());
    }
    #[test]
    fn only_nodes_fold_and_their_nested_fields_share_the_same_page() {
        let mut e = native();
        let doc = e.document().unwrap();
        let original = e.text();
        assert!(!e
            .visible_rows(&doc)
            .iter()
            .any(|r| matches!(r,FlatRow::Field{path,..} if node_index(path).is_some())));
        e.focus_item(&doc, "outbounds", 0);
        press(&mut e, KeyCode::Enter);
        let rows = e.visible_rows(&doc);
        for (suffix, key) in [
            (vec!["config"], "protocol"),
            (vec!["config", "tls"], "server_name"),
            (vec!["config", "transport"], "kind"),
        ] {
            let mut full_path = node_path();
            full_path.extend(path(&suffix));
            assert!(rows.iter().any(|r|matches!(r,FlatRow::Field{path,spec,..} if path == &full_path && spec.key == key)));
        }
        assert!(rows.iter().any(|r| matches!(r, FlatRow::AddHeader { .. })));
        e.form.selected = rows
            .iter()
            .position(
                |r| matches!(r,FlatRow::Section{path,..} if last_key(path) == Some("transport")),
            )
            .unwrap();
        press(&mut e, KeyCode::Enter);
        assert_eq!(e.visible_rows(&doc).len(), rows.len());
        assert!(!e.can_close());
        press(&mut e, KeyCode::Esc);
        assert!(e.can_close());
        assert!(!e
            .visible_rows(&doc)
            .iter()
            .any(|r| matches!(r,FlatRow::Field{path,..} if node_index(path).is_some())));
        assert_eq!(e.text(), original);
    }
    #[test]
    fn node_fold_state_follows_reordering_and_deletion_and_survives_session_restore() {
        let mut e = native();
        let mut doc = e.document().unwrap();
        doc["outbounds"]
            .as_array_of_tables_mut()
            .unwrap()
            .push(outbound_template("socks5", "second"));
        e.update(doc);
        let doc = e.document().unwrap();
        e.focus_item(&doc, "outbounds", 0);
        press(&mut e, KeyCode::Enter);
        press(&mut e, KeyCode::Char('J'));
        assert_eq!(e.form.expanded_nodes, vec![1]);
        let doc = e.document().unwrap();
        assert_eq!(doc["outbounds"][1]["tag"].as_str(), Some("edge"));
        e.focus_item(&doc, "outbounds", 0);
        press(&mut e, KeyCode::Char('d'));
        press(&mut e, KeyCode::Char('y'));
        assert_eq!(e.form.expanded_nodes, vec![0]);
        let mut restored: Editor =
            serde_json::from_slice(&serde_json::to_vec(&e).unwrap()).unwrap();
        restored.normalize();
        assert_eq!(restored.form.expanded_nodes, vec![0]);
        assert_eq!(
            restored.document().unwrap()["outbounds"][0]["tag"].as_str(),
            Some("edge")
        );
    }
    #[test]
    fn legacy_nested_field_input_migrates_to_flat_page_without_losing_draft() {
        let mut e = native();
        e.form.flat = false;
        e.form.path = node_path();
        e.form.path.extend(path(&["config", "tls"]));
        e.form.mode = Mode::Field {
            key: "ca_file".into(),
            input: TextInput::new("remembered CA.pem".into()),
            reveal: false,
        };
        let mut e: Editor = serde_json::from_slice(&serde_json::to_vec(&e).unwrap()).unwrap();
        e.normalize();
        assert!(e.form.flat);
        assert_eq!(e.form.expanded_nodes, vec![0]);
        assert!(matches!(&e.form.mode,Mode::Field{input,..} if input.text == "remembered CA.pem"));
        press(&mut e, KeyCode::Enter);
        let doc = e.document().unwrap();
        assert_eq!(
            doc["outbounds"][0]["config"]["tls"]["ca_file"].as_str(),
            Some("remembered CA.pem")
        );
    }
    #[test]
    fn tls_switch_is_inline_and_editing_a_disabled_tls_field_sets_a_valid_server_name() {
        let mut e = native();
        let doc = e.document().unwrap();
        e.expand_node(0);
        e.form.selected = e
            .visible_rows(&doc)
            .iter()
            .position(|r| matches!(r,FlatRow::Section{path,..} if last_key(path) == Some("tls")))
            .unwrap();
        press(&mut e, KeyCode::Enter);
        assert!(e.document().unwrap()["outbounds"][0]["config"]
            .get("tls")
            .is_none());
        e.form.path = node_path();
        e.form.path.extend(path(&["config", "tls"]));
        field(&mut e, "ca_file", "new CA.pem");
        let doc = e.document().unwrap();
        assert_eq!(
            doc["outbounds"][0]["config"]["tls"]["server_name"].as_str(),
            Some("your.server.example")
        );
        assert_eq!(
            doc["outbounds"][0]["config"]["tls"]["ca_file"].as_str(),
            Some("new CA.pem")
        );
    }
    #[tokio::test]
    #[ignore = "run after cargo build --workspace"]
    async fn live_visual_outbound_templates_pass_core_validation() {
        let directory = std::env::temp_dir().join(format!("astra-form-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        astra_ipc::platform::set_private(&directory, 0o700).unwrap();
        let bin = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(astra_ipc::platform::executable("astra-proxy"));
        for (selected, kind) in OUTBOUND_KINDS.iter().enumerate() {
            let mut e = Editor::new(
                "proxy".into(),
                include_str!("../../../configs/proxy.toml").into(),
            );
            e.form.path = path(&["outbounds"]);
            e.form.mode = Mode::NewOutbound(selected);
            press(&mut e, KeyCode::Enter);
            let file = directory.join(format!("node-{selected}.toml"));
            std::fs::write(&file, e.prepare_save().unwrap()).unwrap();
            let output = tokio::process::Command::new(&bin)
                .arg("--check")
                .arg(&file)
                .output()
                .await
                .unwrap();
            assert!(
                output.status.success(),
                "{}: {}{}",
                kind,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::fs::remove_dir_all(directory).unwrap();
    }
}
