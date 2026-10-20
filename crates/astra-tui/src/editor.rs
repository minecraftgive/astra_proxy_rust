//! Entry-oriented configuration forms. TOML stays an implementation detail of saving.
use crate::protocol::Kind as CoreKind;
use crate::{i18n::t, tr};
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
    #[serde(default)]
    rename_rule_set: Option<String>,
    #[serde(default)]
    expanded_rule_sets: Vec<String>,
    #[serde(default)]
    expanded_collections: Vec<String>,
    #[serde(default)]
    expanded_rule_groups: Vec<usize>,
    #[serde(default)]
    expanded_proxy_groups: Vec<String>,
    #[serde(default)]
    rename_proxy_group: Option<String>,
    #[serde(skip)]
    vim_g: bool,
    #[serde(skip)]
    vim_z: bool,
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
    Members {
        options: Vec<String>,
        chosen: Vec<String>,
        cursor: usize,
    },
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
    #[serde(default)]
    candidate: Option<String>,
}
impl TextInput {
    fn new(text: String) -> Self {
        let cursor = text.chars().count();
        Self {
            text,
            cursor,
            replace: true,
            candidate: None,
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
        self.candidate = None;
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
        self.candidate = None;
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
    #[serde(default)]
    node_tags: Vec<String>,
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
    RuleGroup {
        start: usize,
        end: usize,
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
            Self::RuleGroup { .. } => vec![Part::Key("rules".into())],
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
        bail!(t("无效的条目路径"));
    };
    if let Some(Part::Index(index)) = path.get(1) {
        let child = table
            .get_mut(key)
            .and_then(Item::as_array_of_tables_mut)
            .and_then(|a| a.get_mut(*index))
            .context(t("条目已不存在，请返回上一级"))?;
        table_at_mut(child, &path[2..])
    } else {
        if table.get(key).is_none() {
            table.insert(key, Item::Table(Table::new()));
        }
        let child = table
            .get_mut(key)
            .and_then(Item::as_table_like_mut)
            .context(t("该条目不是参数分组"))?;
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
fn rule_set_name(path: &[Part]) -> Option<&str> {
    match path {
        [Part::Key(root), Part::Key(name), ..] if root == "rule_sets" => Some(name),
        _ => None,
    }
}
fn proxy_group_name(path: &[Part]) -> Option<&str> {
    match path {
        [Part::Key(root), Part::Key(name), ..] if root == "proxy_groups" => Some(name),
        _ => None,
    }
}
fn proxy_targets(doc: &DocumentMut) -> Vec<String> {
    let mut tags: Vec<String> = doc
        .get("outbounds")
        .and_then(Item::as_array_of_tables)
        .into_iter()
        .flat_map(|nodes| nodes.iter())
        .filter_map(|node| node.get("tag").and_then(Item::as_str))
        .map(str::to_owned)
        .collect();
    if let Some(groups) = doc.get("proxy_groups").and_then(Item::as_table_like) {
        tags.extend(groups.iter().map(|(name, _)| name.to_owned()));
    }
    tags.sort();
    if !tags.iter().any(|v| v == "PROXY") {
        tags.push("PROXY".into());
        tags.sort();
    }
    tags.dedup();
    tags
}
fn expanded_proxy_groups(
    doc: &DocumentMut,
) -> Result<std::collections::BTreeMap<String, astra_ipc::groups::Group>> {
    let value: toml::Value = toml::from_str(&doc.to_string())?;
    let mut groups = value
        .get("proxy_groups")
        .cloned()
        .map(|g| g.try_into())
        .transpose()?
        .unwrap_or_default();
    let nodes = doc
        .get("outbounds")
        .and_then(Item::as_array_of_tables)
        .into_iter()
        .flat_map(|a| a.iter())
        .filter_map(|n| n.get("tag").and_then(Item::as_str))
        .map(str::to_owned)
        .collect();
    astra_ipc::groups::ensure_default(&mut groups, &nodes).map_err(anyhow::Error::msg)?;
    astra_ipc::groups::expand(&groups, &nodes).map_err(anyhow::Error::msg)
}
fn proxy_references(doc: &DocumentMut, target: &str) -> bool {
    doc.get("standalone_outbound").and_then(Item::as_str) == Some(target)
        || doc
            .get("proxy_groups")
            .and_then(Item::as_table_like)
            .is_some_and(|groups| {
                groups.iter().any(|(_, group)| {
                    group
                        .as_table_like()
                        .and_then(|g| g.get("selected"))
                        .and_then(Item::as_str)
                        == Some(target)
                        || group
                            .as_table_like()
                            .and_then(|g| g.get("members"))
                            .and_then(Item::as_array)
                            .is_some_and(|a| a.iter().any(|m| m.as_str() == Some(target)))
                })
            })
}
fn rename_proxy_references(doc: &mut DocumentMut, previous: &str, next: &str) {
    if doc.get("standalone_outbound").and_then(Item::as_str) == Some(previous) {
        doc["standalone_outbound"] = value(next);
    }
    if let Some(groups) = doc
        .get_mut("proxy_groups")
        .and_then(Item::as_table_like_mut)
    {
        for (_, group) in groups.iter_mut() {
            if let Some(group) = group.as_table_like_mut() {
                if let Some(members) = group.get_mut("members").and_then(Item::as_array_mut) {
                    for member in members.iter_mut() {
                        if member.as_str() == Some(previous) {
                            *member = toml_edit::Value::from(next);
                        }
                    }
                }
                if group.get("selected").and_then(Item::as_str) == Some(previous) {
                    group.insert("selected", value(next));
                }
            }
        }
    }
}
fn rule_category(table: &Table) -> &'static str {
    match table.get("kind").and_then(Item::as_str).unwrap_or("") {
        "domain_exact" | "domain_suffix" | "domain_subdomain" | "domain_pattern"
        | "domain_keyword" | "domain_regex" => "Domain rules",
        "geosite" => "GEOSITE",
        "ip_cidr" | "geoip" => "IP rules",
        "process_name" | "process_path" => "Process rules",
        "rule_set" => "Rule set references",
        "port" | "network" => "Network / port rules",
        _ => "Other rules",
    }
}
fn rule_groups(array: &ArrayOfTables) -> Vec<(usize, usize, &'static str)> {
    let mut groups: Vec<(usize, usize, &'static str)> = vec![];
    for (index, table) in array.iter().enumerate() {
        let category = rule_category(table);
        if let Some(last) = groups.last_mut().filter(|last| last.2 == category) {
            last.1 = index;
        } else {
            groups.push((index, index, category));
        }
    }
    groups
}
fn rule_set_references(doc: &DocumentMut, name: Option<&str>) -> usize {
    doc.get("rules")
        .and_then(Item::as_array_of_tables)
        .map(|rules| {
            rules
                .iter()
                .filter(|rule| {
                    rule.get("kind").and_then(Item::as_str) == Some("rule_set")
                        && name.is_none_or(|name| {
                            rule.get("value").and_then(Item::as_str) == Some(name)
                        })
                })
                .count()
        })
        .unwrap_or(0)
}
fn rule_set_summary(doc: &DocumentMut, path: &[Part]) -> String {
    let Some(table) = table_at(doc.as_table(), path) else {
        return String::new();
    };
    let enabled = table.get("enabled").and_then(Item::as_bool).unwrap_or(true);
    let behavior = table
        .get("behavior")
        .and_then(Item::as_str)
        .unwrap_or("classical");
    let format = table.get("format").and_then(Item::as_str).unwrap_or("yaml");
    let source = if table.get("url").and_then(Item::as_str).is_some() {
        "HTTP".into()
    } else if let Some(path) = table.get("path").and_then(Item::as_str) {
        std::path::Path::new(path)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    } else {
        tr!(
            "inline · {} entries",
            table
                .get("payload")
                .and_then(Item::as_array)
                .map(|v| v.len())
                .unwrap_or(0)
        )
    };
    tr!(
        " · {} · {} · {} · {} references",
        if enabled { t("Enabled") } else { t("Disabled") },
        format!("{behavior}/{format}"),
        source,
        rule_set_references(doc, rule_set_name(path))
    )
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
    let (Part::Key(key), parent) = path.split_last().context(t("没有选择条目列表"))? else {
        bail!(t("没有选择条目列表"));
    };
    let table = table_at_mut(doc.as_table_mut(), parent)?;
    if table.get(key).is_none() {
        table.insert(key, Item::ArrayOfTables(ArrayOfTables::new()));
    }
    table
        .get_mut(key)
        .and_then(Item::as_array_of_tables_mut)
        .context(t("该分组不是条目列表"))
}

impl Editor {
    fn rename_rule_set(&mut self, previous: &str, name: &str) -> Result<()> {
        anyhow::ensure!(
            !name.trim().is_empty() && name.len() <= 128 && !name.chars().any(char::is_control),
            t("请输入规则集名称")
        );
        let mut doc = self.document()?;
        let sets = doc
            .get_mut("rule_sets")
            .and_then(Item::as_table_like_mut)
            .context(t("规则集"))?;
        anyhow::ensure!(
            previous == name || sets.get(name).is_none(),
            t("规则集名称已存在")
        );
        let item = sets.remove(previous).context(t("规则集"))?;
        sets.insert(name, item);
        if let Some(rules) = doc.get_mut("rules").and_then(Item::as_array_of_tables_mut) {
            for rule in rules.iter_mut() {
                if rule.get("kind").and_then(Item::as_str) == Some("rule_set")
                    && rule.get("value").and_then(Item::as_str) == Some(previous)
                {
                    rule["value"] = value(name);
                }
            }
        }
        self.update(doc);
        self.form.expanded_rule_sets.retain(|key| key != previous);
        self.form.path = vec![Part::Key("rule_sets".into()), Part::Key(name.into())];
        let doc = self.document()?;
        self.focus_field(&doc, &self.form.path.clone(), "enabled");
        Ok(())
    }
    pub fn set_node_tags(&mut self, tags: Vec<String>) {
        self.node_tags = tags;
    }
    fn reference_values(&self, doc: &DocumentMut, key: &str) -> Option<Vec<String>> {
        if key == "selected" && proxy_group_name(&self.form.path).is_some() {
            if let Ok(groups) = expanded_proxy_groups(doc) {
                if let Some(group) = groups.get(proxy_group_name(&self.form.path).unwrap()) {
                    return Some(group.members.clone());
                }
            }
            return Some(
                table_at(doc.as_table(), &self.form.path)
                    .and_then(|g| g.get("members"))
                    .and_then(Item::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|m| m.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default(),
            );
        }
        let action = (key == "default_action" && self.form.path.is_empty() && !self.is_proxy(doc))
            || (key == "action" && last_key(&self.form.path) == Some("rules"));
        if action
            || (key == "standalone_outbound" && self.form.path.is_empty() && self.is_proxy(doc))
        {
            let mut tags = std::collections::BTreeSet::new();
            if self.is_proxy(doc) {
                tags.extend(proxy_targets(doc));
                if let Some(nodes) = doc.get("outbounds").and_then(Item::as_array_of_tables) {
                    tags.extend(
                        nodes
                            .iter()
                            .filter_map(|node| node.get("tag").and_then(Item::as_str))
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned),
                    );
                }
            } else {
                tags.extend(self.node_tags.iter().cloned());
            }
            let mut values = if action {
                vec!["direct".into(), "block".into()]
            } else {
                vec![String::new()]
            };
            values.extend(
                tags.into_iter()
                    .map(|tag| if action { format!("proxy:{tag}") } else { tag }),
            );
            return Some(values);
        }
        if key == "value" && last_key(&self.form.path) == Some("rules") {
            return match table_at(doc.as_table(), &self.form.path)
                .and_then(|t| t.get("kind"))
                .and_then(Item::as_str)
            {
                Some("network") => Some(vec!["tcp".into(), "udp".into()]),
                Some("rule_set") => Some(
                    doc.get("rule_sets")
                        .and_then(Item::as_table_like)
                        .map(|sets| sets.iter().map(|(name, _)| name.to_owned()).collect())
                        .unwrap_or_default(),
                ),
                _ => None,
            };
        }
        None
    }
    fn suggestions(&self, doc: &DocumentMut, key: &str, input: &TextInput) -> Option<Vec<String>> {
        let values = self.reference_values(doc, key).or_else(|| {
            self.current_spec(doc, key).ok().and_then(|s| match s.kind {
                Type::Choice(values) => Some(values.iter().map(|s| (*s).into()).collect()),
                Type::Boolean => Some(vec!["false".into(), "true".into()]),
                _ => None,
            })
        })?;
        let query = if input.replace {
            String::new()
        } else {
            input.text.to_lowercase()
        };
        let mut matches: Vec<_> = values
            .into_iter()
            .filter(|s| s.to_lowercase().contains(&query))
            .collect();
        if !query.is_empty() {
            matches.sort_by_key(|s| {
                !(s.to_lowercase().starts_with(&query)
                    || s.strip_prefix("proxy:")
                        .is_some_and(|tag| tag.to_lowercase().starts_with(&query)))
            });
        }
        Some(matches)
    }
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
            .context(t("配置格式无法解析。原草稿已保留，请修复配置后重新打开"))?;
        // Both [[rules]] and rules = [{...}] are valid core configurations.
        for key in ["rules", "outbounds"] {
            if let Some(array) = doc.get(key).and_then(Item::as_array) {
                let mut tables = ArrayOfTables::new();
                for item in array.iter() {
                    tables.push(
                        item.as_inline_table()
                            .context(t("列表中需要参数条目"))?
                            .clone()
                            .into_table(),
                    );
                }
                doc[key] = Item::ArrayOfTables(tables);
            }
        }
        if self.is_proxy(&doc) {
            let groups = table_at_mut(doc.as_table_mut(), &[Part::Key("proxy_groups".into())])?;
            if groups.get("PROXY").is_none() {
                let mut group = Table::new();
                group["mode"] = value("select");
                group["include_all"] = value(true);
                groups.insert("PROXY", Item::Table(group));
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
        if !matches!(self.form.mode, Mode::Browse) {
            if let Ok(doc) = self.document() {
                let path = self.form.path.clone();
                self.expand_rule_path(&doc, &path);
                if let Some(name) = rule_set_name(&path) {
                    if !self.form.expanded_rule_sets.iter().any(|key| key == name) {
                        self.form.expanded_rule_sets.push(name.into());
                    }
                }
                if let Some(name) = proxy_group_name(&path) {
                    if !self
                        .form
                        .expanded_proxy_groups
                        .iter()
                        .any(|key| key == name)
                    {
                        self.form.expanded_proxy_groups.push(name.into());
                    }
                }
            }
        }
        let platform = crate::availability::Platform::current();
        if self
            .form
            .path
            .first()
            .is_some_and(|part| matches!(part, Part::Key(key) if !platform.option(None, key)))
            || matches!(&self.form.mode, Mode::Field { key, .. } if !platform.option(last_key(&self.form.path), key))
        {
            self.form.path.clear();
            self.form.mode = Mode::Browse;
        }
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
    pub fn browsing(&self) -> bool {
        matches!(self.form.mode, Mode::Browse)
    }
    pub fn import(&mut self, text: String) -> Result<()> {
        let mut candidate = Self::new(self.name.clone(), text);
        candidate.kind = self.kind;
        candidate.node_tags = self.node_tags.clone();
        candidate.document()?;
        candidate.original = self.original.clone();
        *self = candidate;
        Ok(())
    }
    pub fn cancel_input(&mut self) {
        self.form.mode = Mode::Browse;
        self.form.rename_rule_set = None;
        self.form.rename_proxy_group = None;
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
            _ => bail!(t("请先确认或取消当前条目窗口，再保存配置")),
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
                        "shutdown_timeout_secs",
                        t("停止等待时间（秒）"),
                        10,
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
                ]
            }
        } else if last_key(path) == Some("subscription") {
            vec![
                text("url", t("Subscription URL"), "", true),
                number("interval", t("Update interval (seconds)"), 3600, 60, 604800),
                text("user_agent", "User-Agent", "clash.meta/astra", false),
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
                choice("mode", t("Group mode"), "select", &["select", "auto"], true),
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
            if get("mode") == "auto" {
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
            } else {
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
        } else if last_key(path) == Some("tun") {
            vec![
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
                    t("原出口接口（如 en0 / eth0 / Ethernet 或 Windows 索引）"),
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
        } else if matches!(path.last(), Some(Part::Index(_))) && last_key(path) == Some("outbounds")
        {
            let mut f = vec![
                choice(
                    "kind",
                    t("节点类型"),
                    "socks5",
                    &["socks5", "http", "native", "trojan_tls", "vless_tls"],
                    true,
                ),
                text("tag", t("节点标签"), "edge", true),
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
        for field in &mut fields {
            if field.key == "port" && last_key(path) == Some("config") {
                field.required = true;
            }
            field.help = match field.key.as_str() {
                "action" | "default_action" => {
                    t("Select direct / block / a proxy node; manual proxy:tag is supported").into()
                }
                "kind" if last_key(path) == Some("rules") => {
                    t("↑↓ 选择匹配类型；更改类型后请检查匹配内容；规则按列表顺序生效").into()
                }
                "kind" if last_key(path) == Some("transport") => {
                    t("↑↓ 选择传输方式；切换方式会移除不适用的模式和请求头").into()
                }
                "kind" => t("↑↓ 选择；切换节点类型会重置专用字段并保留标签，保存前检查凭据").into(),
                "protocol" => {
                    t("↑↓ 选择；保留可复用参数，重置不适用的协议字段，保存前检查凭据").into()
                }
                "standalone" => t(
                    "true：全部 TCP/UDP 入站使用固定出站，不连接 Rule Core；false：使用规则分流",
                )
                .into(),
                "standalone_outbound" => {
                    t("Select an existing node; leave empty to use the only node").into()
                }
                "port" => t("输入 1–65535 的整数").into(),
                "encryption" => {
                    t("填写完整 mlkem768x25519plus… 字符串；留空或 none 不启用；不是 cipher").into()
                }
                "fingerprint" => {
                    t("客户端 ClientHello 样式，例如 chrome；不是服务端证书哈希").into()
                }
                "cert_pin_sha256" => t(
                    "服务端证书 SHA-256：64 位十六进制；替代 CA 验证，不与自定义 CA / REALITY 合用",
                )
                .into(),
                "mode" if last_key(path) == Some("transport") => t(
                    "auto：普通 TLS 选择 packet-up，REALITY 选择 stream-one；省略默认 stream-one",
                )
                .into(),
                _ => {
                    if field.required {
                        t("必填项").into()
                    } else {
                        t("可选项；清空或按 d 可移除，使用 core 默认值").into()
                    }
                }
            };
        }
        let platform = crate::availability::Platform::current();
        let mut rows: Vec<Row> = fields
            .into_iter()
            .filter(|field| platform.option(last_key(path), &field.key))
            .map(Row::Field)
            .collect();
        let groups: Vec<(&str, &str)> = if path.is_empty() {
            if self.is_proxy(doc) {
                vec![
                    ("system_proxy", t("系统代理（SOCKS）")),
                    ("tun", t("TUN 接管")),
                    ("tproxy", "TPROXY (TCP/UDP)"),
                    ("redirect", "REDIRECT (TCP)"),
                    ("outbounds", t("出站节点")),
                    ("proxy_groups", t("Proxy groups")),
                    ("subscription", t("Subscription")),
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
                if last_key(path) == Some("subscription")
                    && matches!(key, "managed_nodes" | "managed_groups")
                {
                    continue;
                }
                if proxy_group_name(path).is_some()
                    && ((get("mode") == "auto" && key == "selected")
                        || (get("mode") != "auto"
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
                        if key == "rules" && !self.form.expanded_collections.contains(&key) {
                            continue;
                        }
                        if let Some(array) = collection(doc, &child) {
                            let groups = if key == "rules" {
                                rule_groups(array)
                            } else {
                                vec![]
                            };
                            let mut group = 0;
                            for (index, table) in array.iter().enumerate() {
                                if key == "rules" {
                                    while index > groups[group].1 {
                                        group += 1;
                                    }
                                    let (start, end, label) = groups[group];
                                    if index == start {
                                        output.push(FlatRow::RuleGroup {
                                            start,
                                            end,
                                            label: t(label).into(),
                                        });
                                    }
                                    if !self.form.expanded_rule_groups.contains(&start) {
                                        continue;
                                    }
                                }
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
                                        label: tr!("节点 {} · {} [{}]", index + 1, tag, kind),
                                    });
                                    if self.form.expanded_nodes.contains(&index) {
                                        self.flatten_table(doc, &item, "", output);
                                    }
                                } else {
                                    let label = tr!(
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
                                        &tr!("规则 {}", index + 1),
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
                        if child.len() == 1
                            && matches!(key.as_str(), "rule_sets" | "proxy_groups")
                            && !self.form.expanded_collections.contains(&key)
                        {
                            continue;
                        }
                        if let Some(name) = rule_set_name(&child) {
                            if child.len() == 2
                                && !self.form.expanded_rule_sets.iter().any(|key| key == name)
                            {
                                continue;
                            }
                        }
                        if let Some(name) = proxy_group_name(&child) {
                            if child.len() == 2
                                && !self
                                    .form
                                    .expanded_proxy_groups
                                    .iter()
                                    .any(|key| key == name)
                            {
                                continue;
                            }
                        }
                        let field_prefix = match key.as_str() {
                            "config" => t("协议"),
                            "tls" => "TLS",
                            "transport" => t("传输"),
                            "headers" => t("请求头"),
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
        self.expand_rule_path(doc, path);
        if let Some(name) = proxy_group_name(path) {
            if !self
                .form
                .expanded_proxy_groups
                .iter()
                .any(|key| key == name)
            {
                self.form.expanded_proxy_groups.push(name.into());
            }
        }
        if let Some(name) = rule_set_name(path) {
            if !self.form.expanded_rule_sets.iter().any(|key| key == name) {
                self.form.expanded_rule_sets.push(name.to_owned());
            }
        }
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
        self.expand_rule_path(doc, &[Part::Key(kind.into()), Part::Index(index)]);
        if let Some(selected) = self.visible_rows(doc).iter().position(|r| match r {
            FlatRow::Node { index: i, .. } => kind == "outbounds" && *i == index,
            FlatRow::Rule { index: i, .. } => kind == "rules" && *i == index,
            _ => false,
        }) {
            self.form.selected = selected;
        }
    }
    fn expand_rule_path(&mut self, doc: &DocumentMut, path: &[Part]) {
        if let Some(Part::Key(root)) = path.first() {
            if matches!(root.as_str(), "rules" | "rule_sets" | "proxy_groups")
                && !self.form.expanded_collections.contains(root)
            {
                self.form.expanded_collections.push(root.clone());
            }
            if root == "rules" {
                if let Some(Part::Index(index)) = path.get(1) {
                    if let Some(array) = collection(doc, &[Part::Key("rules".into())]) {
                        if let Some((start, _, _)) = rule_groups(array)
                            .into_iter()
                            .find(|(start, end, _)| (*start..=*end).contains(index))
                        {
                            if !self.form.expanded_rule_groups.contains(&start) {
                                self.form.expanded_rule_groups.push(start);
                            }
                        }
                    }
                }
            }
        }
    }
    fn collapse_selection(&mut self, doc: &DocumentMut, row: &FlatRow) {
        let path = row.path();
        if let Some(index) = row.node() {
            self.collapse_node(doc, index);
            return;
        }
        if let Some(name) = proxy_group_name(&path) {
            let open = self
                .form
                .expanded_proxy_groups
                .iter()
                .any(|key| key == name);
            self.form.expanded_proxy_groups.retain(|key| key != name);
            if !open {
                self.form
                    .expanded_collections
                    .retain(|key| key != "proxy_groups");
            }
            let target = if open {
                path[..2].to_vec()
            } else {
                path[..1].to_vec()
            };
            self.form.selected = self
                .visible_rows(doc)
                .iter()
                .position(|r| r.path() == target)
                .unwrap_or(0);
            return;
        }
        if let Some(name) = rule_set_name(&path) {
            let open = self.form.expanded_rule_sets.iter().any(|key| key == name);
            self.form.expanded_rule_sets.retain(|key| key != name);
            if matches!(row, FlatRow::Section { .. }) && !open {
                self.form
                    .expanded_collections
                    .retain(|key| key != "rule_sets");
            }
            self.form.selected = self.visible_rows(doc).iter().position(|row| matches!(row,FlatRow::Section {path,..} if if open {rule_set_name(path) == Some(name)} else {path == &vec![Part::Key("rule_sets".into())]})).unwrap_or(0);
            return;
        }
        if path.first() == Some(&Part::Key("rules".into())) {
            let group_start = match row {
                FlatRow::RuleGroup { start, .. } => Some(*start),
                _ => {
                    if let Some(Part::Index(index)) = path.get(1) {
                        collection(doc, &[Part::Key("rules".into())]).and_then(|array| {
                            rule_groups(array)
                                .into_iter()
                                .find(|(s, e, _)| (*s..=*e).contains(index))
                                .map(|v| v.0)
                        })
                    } else {
                        None
                    }
                }
            };
            if let Some(start) =
                group_start.filter(|start| self.form.expanded_rule_groups.contains(start))
            {
                self.form.expanded_rule_groups.retain(|key| *key != start);
                self.form.selected = self
                    .visible_rows(doc)
                    .iter()
                    .position(|row| matches!(row,FlatRow::RuleGroup {start:s,..} if *s == start))
                    .unwrap_or(0);
                return;
            }
            self.form.expanded_collections.retain(|key| key != "rules");
        } else if path == vec![Part::Key("rule_sets".into())]
            || path == vec![Part::Key("proxy_groups".into())]
        {
            self.form
                .expanded_collections
                .retain(|key| Some(key.as_str()) != last_key(&path));
        } else {
            return;
        }
        self.form.selected = self.visible_rows(doc).iter().position(|row| matches!(row,FlatRow::Collection {path:p,..} | FlatRow::Section {path:p,..} if *p == path[..1])).unwrap_or(0);
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
            .context(t("字段已不存在"))
    }
    fn commit_field(&mut self) -> Result<()> {
        let Mode::Field { key, input, .. } = &self.form.mode else {
            return Ok(());
        };
        let key = key.clone();
        let mut doc = self.document()?;
        let mut text = input
            .candidate
            .clone()
            .filter(|candidate| {
                self.suggestions(&doc, &key, input)
                    .is_some_and(|values| values.contains(candidate))
            })
            .unwrap_or_else(|| input.text.clone());
        // Accept a complete known tag without requiring the proxy: prefix.
        if matches!(key.as_str(), "action" | "default_action") {
            let candidate = format!("proxy:{text}");
            if text != "direct"
                && text != "block"
                && !text.starts_with("proxy:")
                && self
                    .reference_values(&doc, &key)
                    .is_some_and(|v| v.contains(&candidate))
            {
                text = candidate;
            }
        }
        let input = text;
        let spec = self.current_spec(&doc, &key)?;
        let previous_tag = if key == "tag" && node_index(&self.form.path).is_some() {
            let previous = table_at(doc.as_table(), &self.form.path)
                .and_then(|t| t.get("tag"))
                .and_then(Item::as_str)
                .unwrap_or("")
                .to_owned();
            anyhow::ensure!(
                input == previous || !proxy_targets(&doc).contains(&input),
                t("Proxy target name already exists")
            );
            Some(previous)
        } else {
            None
        };
        if key == "selected" && proxy_group_name(&self.form.path).is_some() && !input.is_empty() {
            anyhow::ensure!(
                self.reference_values(&doc, &key)
                    .is_some_and(|v| v.contains(&input)),
                t("Select a member of this group")
            );
        }
        let remove = input.is_empty() && !spec.required;
        let item = match &spec.kind {
            Type::Number(min, max) if !remove => {
                let n: i64 = input.parse().context(t("请输入整数"))?;
                anyhow::ensure!(
                    (*min..=*max).contains(&n),
                    tr!("{} 必须在 {}–{} 之间", spec.label, min, max)
                );
                value(n)
            }
            Type::Choice(choices) if !remove => {
                anyhow::ensure!(
                    choices.contains(&input.as_str()),
                    tr!("请用 ↑↓ 选择支持的值：{}", choices.join(", "))
                );
                value(&input)
            }
            Type::StringList if !remove => {
                let parsed: DocumentMut = format!("values = {input}")
                    .parse()
                    .context(t("请输入 TOML 字符串列表，例如 [\"en0\", \"eth0\"]"))?;
                let array = parsed["values"].as_array().context(t("必须是字符串列表"))?;
                anyhow::ensure!(
                    array.iter().all(|value| value.as_str().is_some()),
                    t("列表中的值必须是字符串")
                );
                Item::Value(toml_edit::Value::Array(array.clone()))
            }
            Type::Boolean if !remove => {
                value(input.parse::<bool>().context(t("请选择 true 或 false"))?)
            }
            _ => {
                anyhow::ensure!(
                    !spec.required || !input.trim().is_empty(),
                    tr!("{} 不能为空", spec.label)
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
        let rule_set_default =
            if key == "kind" && input == "rule_set" && last_key(&self.form.path) == Some("rules") {
                Some(
                    doc.get("rule_sets")
                        .and_then(Item::as_table)
                        .and_then(|sets| sets.iter().next().map(|(name, _)| name.to_owned()))
                        .context(t("Create a rule set before adding a reference"))?,
                )
            } else {
                None
            };
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
                    && !([
                        "domain_exact",
                        "domain_suffix",
                        "domain_subdomain",
                        "domain_keyword",
                    ]
                    .contains(&previous.as_str())
                        && [
                            "domain_exact",
                            "domain_suffix",
                            "domain_subdomain",
                            "domain_keyword",
                        ]
                        .contains(&input.as_str()))
                {
                    let default = match input.as_str() {
                        "port" => "443",
                        "network" => "tcp",
                        "ip_cidr" => "10.0.0.0/8",
                        "geoip" => "CN",
                        "geosite" => "cn",
                        "domain_regex" => r"^example\.com$",
                        "domain_pattern" => "*.example.com",
                        "rule_set" => rule_set_default.as_deref().unwrap(),
                        "process_name" => "browser.exe",
                        "process_path" => {
                            if cfg!(windows) {
                                "C:/Program Files/Browser/browser.exe"
                            } else {
                                "/usr/bin/browser"
                            }
                        }
                        _ => "example.com",
                    };
                    replace_field(table, "value", value(default));
                }
            }
        }
        if let Some(previous) = previous_tag {
            if previous != input {
                rename_proxy_references(&mut doc, &previous, &input);
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
            FlatRow::RuleGroup { start, .. } => {
                if self.form.expanded_rule_groups.contains(&start) && !expand_only {
                    self.form.expanded_rule_groups.retain(|key| *key != start);
                } else if !self.form.expanded_rule_groups.contains(&start) {
                    self.form.expanded_rule_groups.push(start);
                }
            }
            FlatRow::Collection { path, .. } | FlatRow::Section { path, .. }
                if path.len() == 1
                    && matches!(
                        last_key(&path),
                        Some("rules" | "rule_sets" | "proxy_groups")
                    ) =>
            {
                let key = last_key(&path).unwrap().to_owned();
                if self.form.expanded_collections.contains(&key) && !expand_only {
                    self.form.expanded_collections.retain(|item| item != &key);
                } else if !self.form.expanded_collections.contains(&key) {
                    self.form.expanded_collections.push(key);
                }
            }
            FlatRow::Field { path, spec, .. } => {
                if proxy_group_name(&path).is_some() && spec.key == "members" {
                    let name = proxy_group_name(&path).unwrap();
                    let chosen: Vec<String> = table_at(doc.as_table(), &path)
                        .and_then(|g| g.get("members"))
                        .and_then(Item::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(|m| m.as_str().map(str::to_owned))
                                .collect()
                        })
                        .unwrap_or_default();
                    let mut options = proxy_targets(&doc);
                    options.extend(["DIRECT".into(), "REJECT".into()]);
                    options.extend(chosen.iter().cloned());
                    options.sort();
                    options.dedup();
                    options.retain(|v| v != name);
                    self.form.path = path;
                    self.form.mode = Mode::Members {
                        options,
                        chosen,
                        cursor: 0,
                    };
                    return Ok(());
                }
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
            FlatRow::Section { path, .. } if rule_set_name(&path).is_some() => {
                let name = rule_set_name(&path).unwrap().to_owned();
                if self.form.expanded_rule_sets.contains(&name) && !expand_only {
                    self.form.expanded_rule_sets.retain(|key| key != &name);
                } else if !self.form.expanded_rule_sets.contains(&name) {
                    self.form.expanded_rule_sets.push(name);
                }
            }
            FlatRow::Section { path, .. } if proxy_group_name(&path).is_some() => {
                let name = proxy_group_name(&path).unwrap().to_owned();
                if self.form.expanded_proxy_groups.contains(&name) && !expand_only {
                    self.form.expanded_proxy_groups.retain(|key| key != &name);
                } else if !self.form.expanded_proxy_groups.contains(&name) {
                    self.form.expanded_proxy_groups.push(name);
                }
            }
            FlatRow::Section { path, .. }
                if matches!(
                    last_key(&path),
                    Some("tls" | "system_proxy" | "tun" | "tproxy" | "redirect")
                ) =>
            {
                let (Part::Key(key), parent) = path.split_last().context(t("TLS 参数路径无效"))?
                else {
                    bail!(t("TLS 参数路径无效"));
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
    pub fn key(&mut self, mut key: KeyEvent) -> Result<()> {
        if matches!(self.form.mode, Mode::Browse) {
            if key.code == KeyCode::Char('g') && key.modifiers.is_empty() {
                if !self.form.vim_g {
                    self.form.vim_g = true;
                    return Ok(());
                }
                key.code = KeyCode::Home;
            }
            self.form.vim_g = false;
            if key.code == KeyCode::Char('z') && key.modifiers.is_empty() {
                self.form.vim_z = true;
                return Ok(());
            }
            if std::mem::take(&mut self.form.vim_z) {
                match key.code {
                    KeyCode::Char('M') => {
                        self.form.expanded_collections.clear();
                        self.form.expanded_rule_groups.clear();
                        self.form.expanded_rule_sets.clear();
                        self.form.expanded_proxy_groups.clear();
                        self.form.expanded_nodes.clear();
                        self.form.selected = 0;
                        return Ok(());
                    }
                    KeyCode::Char('R') => {
                        let doc = self.document()?;
                        self.form.expanded_collections =
                            vec!["rules".into(), "rule_sets".into(), "proxy_groups".into()];
                        self.form.expanded_rule_groups =
                            collection(&doc, &[Part::Key("rules".into())])
                                .map(|a| rule_groups(a).into_iter().map(|v| v.0).collect())
                                .unwrap_or_default();
                        self.form.expanded_rule_sets = doc
                            .get("rule_sets")
                            .and_then(Item::as_table_like)
                            .map(|t| t.iter().map(|(name, _)| name.to_owned()).collect())
                            .unwrap_or_default();
                        self.form.expanded_proxy_groups = doc
                            .get("proxy_groups")
                            .and_then(Item::as_table_like)
                            .map(|t| t.iter().map(|(name, _)| name.to_owned()).collect())
                            .unwrap_or_default();
                        self.form.expanded_nodes =
                            (0..collection(&doc, &[Part::Key("outbounds".into())])
                                .map(|a| a.len())
                                .unwrap_or(0))
                                .collect();
                        return Ok(());
                    }
                    KeyCode::Char('c') => key.code = KeyCode::Left,
                    KeyCode::Char('o') => key.code = KeyCode::Right,
                    _ => return Ok(()),
                }
            }
            key.code = match key.code {
                KeyCode::Char('j') if key.modifiers.is_empty() => KeyCode::Down,
                KeyCode::Char('k') if key.modifiers.is_empty() => KeyCode::Up,
                KeyCode::Char('h') if key.modifiers.is_empty() => KeyCode::Left,
                KeyCode::Char('l') if key.modifiers.is_empty() => KeyCode::Right,
                KeyCode::Char('G') => KeyCode::End,
                KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    KeyCode::PageDown
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    KeyCode::PageUp
                }
                code => code,
            };
        }
        if matches!(self.form.mode, Mode::Members { .. }) {
            let dynamic =
                table_at(self.document()?.as_table(), &self.form.path).is_some_and(|group| {
                    ["include_all", "include_all_groups"]
                        .iter()
                        .any(|key| group.get(key).and_then(Item::as_bool) == Some(true))
                });
            let mut commit = None;
            if let Mode::Members {
                options,
                chosen,
                cursor,
            } = &mut self.form.mode
            {
                match key.code {
                    KeyCode::Esc => {
                        self.cancel_input();
                        return Ok(());
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        *cursor = (*cursor + 1).min(options.len().saturating_sub(1))
                    }
                    KeyCode::Up | KeyCode::Char('k') => *cursor = cursor.saturating_sub(1),
                    KeyCode::Char(' ') => {
                        if let Some(name) = options.get(*cursor) {
                            if chosen.contains(name) {
                                chosen.retain(|m| m != name);
                            } else {
                                chosen.push(name.clone());
                            }
                        }
                    }
                    KeyCode::Enter => {
                        anyhow::ensure!(
                            !chosen.is_empty() || dynamic,
                            t("Choose at least one group member")
                        );
                        commit = Some(chosen.clone());
                    }
                    _ => {}
                }
            }
            if let Some(chosen) = commit {
                let mut doc = self.document()?;
                let group = table_at_mut(doc.as_table_mut(), &self.form.path)?;
                group.insert(
                    "members",
                    value(
                        chosen
                            .iter()
                            .map(String::as_str)
                            .collect::<toml_edit::Array>(),
                    ),
                );
                let name = proxy_group_name(&self.form.path).unwrap();
                if let Some(effective) = expanded_proxy_groups(&doc)?.get(name) {
                    if effective
                        .selected
                        .as_ref()
                        .is_some_and(|s| !effective.members.contains(s))
                    {
                        let group = table_at_mut(doc.as_table_mut(), &self.form.path)?;
                        if let Some(first) = effective.members.first() {
                            group.insert("selected", value(first));
                        } else {
                            group.remove("selected");
                        }
                    }
                }
                self.update(doc);
                self.form.mode = Mode::Browse;
                self.focus_field(&self.document()?, &self.form.path.clone(), "members");
            }
            return Ok(());
        }
        if matches!(self.form.mode, Mode::Field { .. }) {
            if key.code == KeyCode::Esc {
                self.cancel_input();
                return Ok(());
            }
            if key.code == KeyCode::Enter {
                return self.commit_field();
            }
            let doc = self.document()?;
            if matches!(
                key.code,
                KeyCode::Up | KeyCode::Down | KeyCode::Tab | KeyCode::BackTab
            ) {
                let Mode::Field {
                    key: field, input, ..
                } = &self.form.mode
                else {
                    unreachable!()
                };
                if let Some(values) = self.suggestions(&doc, field, input) {
                    if !values.is_empty() {
                        let selected = input.candidate.as_ref().unwrap_or(&input.text);
                        let at = values.iter().position(|value| value == selected);
                        let backward = matches!(key.code, KeyCode::Up | KeyCode::BackTab);
                        let next = if key.code == KeyCode::Tab {
                            at.unwrap_or(0)
                        } else if backward {
                            at.map_or(values.len() - 1, |at| {
                                (at + values.len() - 1) % values.len()
                            })
                        } else {
                            at.map_or(0, |at| (at + 1) % values.len())
                        };
                        let Mode::Field { input, .. } = &mut self.form.mode else {
                            unreachable!()
                        };
                        if key.code == KeyCode::Tab {
                            *input = TextInput::new(values[next].clone());
                            input.replace = false;
                        } else {
                            input.candidate = Some(values[next].clone());
                        }
                    }
                    return Ok(());
                }
            }
            let Mode::Field { reveal, .. } = &mut self.form.mode else {
                unreachable!()
            };
            if key.code == KeyCode::F(2) {
                *reveal = !*reveal;
                return Ok(());
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
                    if last_key(&self.form.path) == Some("proxy_groups") {
                        anyhow::ensure!(
                            !key.is_empty()
                                && key.len() <= 64
                                && !key.chars().any(char::is_control)
                                && !astra_ipc::groups::builtin(&key),
                            t("Enter a proxy group name")
                        );
                        let mut doc = self.document()?;
                        let previous = self.form.rename_proxy_group.clone();
                        let creating = previous.is_none();
                        anyhow::ensure!(
                            previous.as_deref() == Some(&key)
                                || !proxy_targets(&doc).contains(&key),
                            t("Proxy target name already exists")
                        );
                        let groups = table_at_mut(doc.as_table_mut(), &self.form.path)?;
                        if let Some(previous) = previous {
                            let group = groups.remove(&previous).context(t("Proxy groups"))?;
                            groups.insert(&key, group);
                            rename_proxy_references(&mut doc, &previous, &key);
                            self.form
                                .expanded_proxy_groups
                                .retain(|name| name != &previous);
                        } else {
                            let mut group = Table::new();
                            group["mode"] = value("select");
                            group["members"] = value(toml_edit::Array::new());
                            groups.insert(&key, Item::Table(group));
                        }
                        self.update(doc);
                        self.cancel_input();
                        self.form.path.push(Part::Key(key));
                        self.focus_field(&self.document()?, &self.form.path.clone(), "members");
                        if creating {
                            self.enter(false)?;
                        }
                        return Ok(());
                    }
                    if last_key(&self.form.path) == Some("rule_sets") {
                        anyhow::ensure!(
                            !key.is_empty()
                                && key.len() <= 128
                                && !key.chars().any(char::is_control),
                            t("请输入规则集名称")
                        );
                        if let Some(previous) = self.form.rename_rule_set.clone() {
                            self.rename_rule_set(&previous, &key)?;
                            self.cancel_input();
                            return Ok(());
                        }
                        let mut doc = self.document()?;
                        let table = table_at_mut(doc.as_table_mut(), &self.form.path)?;
                        anyhow::ensure!(table.get(&key).is_none(), t("规则集名称已存在"));
                        let mut source = Table::new();
                        source["behavior"] = value("domain");
                        source["format"] = value("yaml");
                        source["payload"] = value(toml_edit::Array::new());
                        table.insert(&key, Item::Table(source));
                        self.update(doc);
                        self.form.path.push(Part::Key(key));
                        self.form.mode = Mode::Browse;
                        self.focus_field(&self.document()?, &self.form.path.clone(), "payload");
                        return Ok(());
                    }

                    anyhow::ensure!(
                        !key.is_empty()
                            && key
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric()
                                    || b"!#$%&'*+-.^_`|~".contains(&b)),
                        t("请输入有效的 HTTP 请求头名称")
                    );
                    let mut doc = self.document()?;
                    let table = table_at_mut(doc.as_table_mut(), &self.form.path)?;
                    anyhow::ensure!(table.get(&key).is_none(), t("该请求头已存在"));
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
                        if self.form.path == vec![Part::Key("outbounds".into())] {
                            if let Some(tag) = collection(&doc, &self.form.path)
                                .and_then(|a| a.get(*index))
                                .and_then(|node| node.get("tag"))
                                .and_then(Item::as_str)
                            {
                                anyhow::ensure!(!proxy_references(&doc, tag), t("Proxy target is referenced; change its groups or default before deleting"));
                            }
                        }
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
                        if self.form.path == vec![Part::Key("proxy_groups".into())] {
                            anyhow::ensure!(!proxy_references(&doc, field), t("Proxy target is referenced; change its groups or default before deleting"));
                        }
                        if self.form.path == vec![Part::Key("rule_sets".into())] {
                            anyhow::ensure!(
                                rule_set_references(&doc, Some(field)) == 0,
                                t("Rule set is referenced; change its rules before deleting")
                            );
                        } else if self.form.path.is_empty() && field == "rule_sets" {
                            anyhow::ensure!(
                                rule_set_references(&doc, None) == 0,
                                t("Rule set is referenced; change its rules before deleting")
                            );
                        }
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
            KeyCode::Char('r') => {
                if let Some(name) = selected
                    .as_ref()
                    .and_then(|r| proxy_group_name(&r.path()).map(str::to_owned))
                {
                    self.form.path = vec![Part::Key("proxy_groups".into())];
                    anyhow::ensure!(
                        name != "PROXY",
                        t("Default PROXY group cannot be renamed or deleted")
                    );
                    self.form.rename_proxy_group = Some(name.clone());
                    self.form.mode = Mode::Header(TextInput::new(name));
                    return Ok(());
                }
                if let Some(name) = selected
                    .as_ref()
                    .and_then(|row| rule_set_name(&row.path()).map(str::to_owned))
                {
                    self.form.path = vec![Part::Key("rule_sets".into())];
                    self.form.rename_rule_set = Some(name.clone());
                    self.form.mode = Mode::Header(TextInput::new(name));
                }
            }
            KeyCode::Char('e') => {
                if let Some(name) = selected
                    .as_ref()
                    .and_then(|row| rule_set_name(&row.path()).map(str::to_owned))
                {
                    let mut doc = self.document()?;
                    let path = vec![Part::Key("rule_sets".into()), Part::Key(name)];
                    let table = table_at_mut(doc.as_table_mut(), &path)?;
                    let enabled = table.get("enabled").and_then(Item::as_bool).unwrap_or(true);
                    table.insert("enabled", value(!enabled));
                    self.update(doc);
                }
            }
            KeyCode::PageUp => self.form.selected = self.form.selected.saturating_sub(10),
            KeyCode::PageDown => {
                self.form.selected = (self.form.selected + 10).min(rows.len().saturating_sub(1))
            }
            KeyCode::Home => self.form.selected = 0,
            KeyCode::End => self.form.selected = rows.len().saturating_sub(1),
            KeyCode::Enter | KeyCode::Right => self.enter(key.code == KeyCode::Right)?,
            KeyCode::Left => {
                if let Some(row) = &selected {
                    self.collapse_selection(&doc, row);
                }
            }
            KeyCode::Esc => {
                if let Some(row) = &selected {
                    if row.node().is_some()
                        || rule_set_name(&row.path()).is_some()
                        || proxy_group_name(&row.path()).is_some()
                    {
                        self.collapse_selection(&doc, row);
                    }
                }
            }
            KeyCode::Char('n') => {
                let path = selected.as_ref().map(FlatRow::path).unwrap_or_default();
                if path.first() == Some(&Part::Key("proxy_groups".into())) {
                    self.form.path = vec![Part::Key("proxy_groups".into())];
                    self.form.mode = Mode::Header(TextInput::default());
                    return Ok(());
                }
                if path
                    .iter()
                    .any(|part| matches!(part,Part::Key(key) if key=="rule_sets"))
                {
                    self.form.path = vec![Part::Key("rule_sets".into())];
                    self.form.mode = Mode::Header(TextInput::default());
                } else if last_key(&path) == Some("headers") {
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
                    anyhow::ensure!(!spec.required, t("必填项不能删除，请按 Enter 修改"));
                    self.form.path = path;
                    self.form.mode = Mode::Delete {
                        key: Some(spec.key),
                        index: None,
                    };
                }
                Some(FlatRow::Node { index, .. }) => {
                    if let Some(tag) = collection(&doc, &[Part::Key("outbounds".into())])
                        .and_then(|a| a.get(index))
                        .and_then(|node| node.get("tag"))
                        .and_then(Item::as_str)
                    {
                        anyhow::ensure!(!proxy_references(&doc, tag), t("Proxy target is referenced; change its groups or default before deleting"));
                    }
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
                    if let Some(name) = proxy_group_name(&path) {
                        anyhow::ensure!(
                            name != "PROXY",
                            t("Default PROXY group cannot be renamed or deleted")
                        );
                        anyhow::ensure!(!proxy_references(&doc, name), t("Proxy target is referenced; change its groups or default before deleting"));
                    }
                    anyhow::ensure!(
                        path != vec![Part::Key("proxy_groups".into())],
                        t("Default PROXY group cannot be renamed or deleted")
                    );
                    if path == vec![Part::Key("outbounds".into())] {
                        anyhow::ensure!(!collection(&doc, &path).is_some_and(|nodes| nodes.iter().filter_map(|n| n.get("tag").and_then(Item::as_str)).any(|tag| proxy_references(&doc, tag))), t("Proxy target is referenced; change its groups or default before deleting"));
                    }
                    if let Some(name) = rule_set_name(&path) {
                        anyhow::ensure!(
                            rule_set_references(&doc, Some(name)) == 0,
                            t("Rule set is referenced; change its rules before deleting")
                        );
                    } else if last_key(&path) == Some("rule_sets") {
                        anyhow::ensure!(
                            rule_set_references(&doc, None) == 0,
                            t("Rule set is referenced; change its rules before deleting")
                        );
                    }
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
                let (kind, index) =
                    item_index(&path).context(t("请先选择要调整顺序的规则或节点"))?;
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
            t(" *未保存")
        } else {
            ""
        };
        let title = tr!("Configuration · {}{}", self.name, dirty);
        let Ok(doc) = self.document() else {
            frame.render_widget(
                Paragraph::new(t(
                    "配置格式无法解析，草稿已保留。请修复配置文件后重新打开。Esc 返回。",
                ))
                .wrap(Wrap { trim: false })
                .block(Block::default().title(title).borders(Borders::ALL)),
                area,
            );
            return;
        };
        let rows = self.visible_rows(&doc);
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(4)])
            .split(area);
        let expanded_groups = if self.is_proxy(&doc) {
            expanded_proxy_groups(&doc).ok()
        } else {
            None
        };
        let items: Vec<ListItem> = rows
            .iter()
            .map(|row| {
                let (content, style) = match row {
                    FlatRow::Field { path, spec, prefix } => {
                        let table = table_at(doc.as_table(), path);
                        let current = table.and_then(|t| t.get(&spec.key));
                        let tls_disabled = last_key(path) == Some("tls") && table.is_none();
                        let text = if tls_disabled {
                            t("[TLS 未启用；可直接填写或在 TLS 行启用]").into()
                        } else if let Some(item) = current {
                            let text = scalar(item);
                            if spec.secret() && !text.is_empty() {
                                "••••••••".into()
                            } else {
                                text.replace('\n', " ↵ ")
                            }
                        } else if spec.required {
                            tr!(
                                "[必填，尚未设置；建议 {}]",
                                if spec.secret() {
                                    t("输入凭据")
                                } else {
                                    &spec.default
                                }
                            )
                        } else {
                            tr!(
                                "[未设置{}]",
                                if spec.default.is_empty() {
                                    String::new()
                                } else {
                                    tr!("，默认 {}", spec.default)
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
                            if last_key(path) == Some("rules") {
                                format!(
                                    "{} {} ({count})",
                                    if self.form.expanded_collections.contains(&"rules".into()) {
                                        "▾"
                                    } else {
                                        "▸"
                                    },
                                    label
                                )
                            } else {
                                tr!("── {}（{} 条） · n 新增 ──", label, count)
                            },
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
                    FlatRow::RuleGroup { start, end, label } => (
                        tr!(
                            "{} {} · rules {}–{}",
                            if self.form.expanded_rule_groups.contains(start) {
                                "▾"
                            } else {
                                "▸"
                            },
                            label,
                            start + 1,
                            end + 1
                        ),
                        Style::default().fg(Color::Cyan),
                    ),
                    FlatRow::Section { path, label } => {
                        let suffix = if proxy_group_name(path).is_some() {
                            let group = table_at(doc.as_table(), path);
                            let mode = group
                                .and_then(|g| g.get("mode"))
                                .and_then(Item::as_str)
                                .unwrap_or("select");
                            let members = group
                                .and_then(|g| g.get("members"))
                                .and_then(Item::as_array);
                            let selected = group
                                .and_then(|g| g.get("selected"))
                                .and_then(Item::as_str)
                                .or_else(|| {
                                    members
                                        .and_then(|m| m.get(0))
                                        .and_then(toml_edit::Value::as_str)
                                })
                                .unwrap_or("—");
                            let effective = expanded_groups
                                .as_ref()
                                .and_then(|g| g.get(proxy_group_name(path).unwrap()));
                            let selected = effective
                                .as_ref()
                                .and_then(|g| g.selected.as_ref().or_else(|| g.members.first()))
                                .map(String::as_str)
                                .unwrap_or(selected);
                            tr!(
                                " · {} members · {} → {}",
                                effective
                                    .as_ref()
                                    .map(|g| g.members.len())
                                    .unwrap_or_else(|| members.map(|m| m.len()).unwrap_or(0)),
                                mode,
                                if mode == "auto" {
                                    t("Automatic")
                                } else {
                                    selected
                                }
                            )
                        } else if rule_set_name(path).is_some() {
                            rule_set_summary(&doc, path)
                        } else if matches!(
                            last_key(path),
                            Some("tls" | "system_proxy" | "tun" | "tproxy" | "redirect")
                        ) {
                            if table_at(doc.as_table(), path).is_some() {
                                t(" · 已启用 / Enter 关闭").to_owned()
                            } else {
                                t(" · 未启用 / Enter 启用").to_owned()
                            }
                        } else {
                            String::new()
                        };
                        let mark = if path.len() == 1
                            && matches!(last_key(path), Some("rule_sets" | "proxy_groups"))
                        {
                            if self
                                .form
                                .expanded_collections
                                .iter()
                                .any(|v| Some(v.as_str()) == last_key(path))
                            {
                                "▾"
                            } else {
                                "▸"
                            }
                        } else if let Some(name) = proxy_group_name(path) {
                            if self.form.expanded_proxy_groups.iter().any(|v| v == name) {
                                "▾"
                            } else {
                                "▸"
                            }
                        } else {
                            rule_set_name(path)
                                .map(|name| {
                                    if self.form.expanded_rule_sets.iter().any(|key| key == name) {
                                        "▾"
                                    } else {
                                        "▸"
                                    }
                                })
                                .unwrap_or("──")
                        };
                        (
                            format!("  {mark} {label}{suffix}"),
                            Style::default().fg(Color::Gray),
                        )
                    }
                    FlatRow::AddHeader { .. } => (
                        t("  ＋ 新增请求头（Enter）").into(),
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
            Some(row)
                if proxy_group_name(&row.path()).is_some()
                    || row.path() == vec![Part::Key("proxy_groups".into())] =>
            {
                t("Enter edit/fold · n new group · r rename · d delete · Ctrl-S apply").into()
            }
            Some(FlatRow::RuleGroup { .. }) | Some(FlatRow::Collection { .. }) => {
                t("j/k move · h/l fold/open · gg/G first/last · zM/zR fold/open all").into()
            }
            Some(row) if rule_set_name(&row.path()).is_some() => {
                t("Enter expand/collapse · r rename · e enable/disable · d delete").into()
            }
            Some(FlatRow::Field { spec, .. }) => spec.help.clone(),
            Some(FlatRow::Node { .. }) => {
                t("Enter 展开/收起节点；展开后全部参数平铺；d 删除；K/J 调整顺序").into()
            }
            Some(FlatRow::Rule { .. }) => {
                t("规则字段已在下方平铺；d 删除整条规则；K/J 上移/下移").into()
            }
            Some(FlatRow::Section { path, .. }) if last_key(path) == Some("tls") => {
                t("Enter 启用/关闭 TLS；字段始终平铺；d 删除本分组参数").into()
            }
            Some(FlatRow::AddHeader { .. }) => {
                t("Enter 新增请求头；已有请求头直接在上方修改").into()
            }
            _ => t("全部参数在同一页；↑↓ 选择，Enter 编辑，n 新增，Ctrl-S 校验保存").into(),
        };
        frame.render_widget(
            Paragraph::new(format!(
                "{}\n{hint}",
                t("F4 external editor · F5 read external changes · Ctrl-S save")
            ))
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL)),
            sections[1],
        );
        if !matches!(self.form.mode, Mode::Browse) {
            let height = match &self.form.mode {
                Mode::NewOutbound(_) => 15,
                Mode::Members { .. } => 15,
                Mode::Field { key, input, .. } => self
                    .suggestions(&doc, key, input)
                    .map(|values| (values.len().min(6) as u16 + 6).max(8))
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
                Mode::Members { options, chosen, cursor } => {
                    let mut state = ListState::default().with_selected(Some(*cursor));
                    let items = options.iter().map(|name| ListItem::new(format!("[{}] {}", if chosen.contains(name) { "×" } else { " " }, name)));
                    frame.render_stateful_widget(List::new(items).highlight_symbol("› ").highlight_style(Style::default().bg(Color::Blue))
                        .block(Block::default().title(t("Group members")).title_bottom(t("j/k move · Space toggle · Enter confirm · Esc cancel")).borders(Borders::ALL)), popup, &mut state);
                }
                Mode::Field{key,input,reveal} => {
                    let spec = self.current_spec(&doc,key).ok();
                    let secret = spec.as_ref().is_some_and(Spec::secret) && !reveal;
                    let display = if secret { "•".repeat(input.text.chars().count()) } else { input.text.replace('\n'," ↵ ") };
                    let mut display: Vec<char> = display.chars().collect(); display.insert(input.cursor.min(display.len()),'▏');
                    let suggestions = self.suggestions(&doc, key, input);
                    let choices = suggestions.as_ref().map(|values| {
                        if values.is_empty() { return t("No matches").to_owned(); }
                        let selected = input.candidate.as_ref().unwrap_or(&input.text);
                        let at = values.iter().position(|value| value == selected).unwrap_or(0);
                        let start = at.saturating_sub(2).min(values.len().saturating_sub(6));
                        values.iter().skip(start).take(6).map(|value| format!("{} {}",if value == selected {"›"} else {" "},if value.is_empty() {t("(未设置)")} else {value})).collect::<Vec<_>>().join("\n")
                    }).unwrap_or_default();
                    let hint = if suggestions.is_some() { t("Type to filter · ↑↓ select · Tab complete · Enter confirm · Esc cancel") } else { t("Enter confirm · Esc cancel · Ctrl-U clear · F2 reveal") };
                    let text = format!("> {}\n{}\n{}\n{}",display.iter().collect::<String>(),choices,hint,spec.as_ref().map(|s|s.help.as_str()).unwrap_or(""));
                    frame.render_widget(Paragraph::new(text).wrap(Wrap{trim:false}).block(Block::default().title(spec.as_ref().map(|s|s.label.as_str()).unwrap_or(key)).borders(Borders::ALL).style(Style::default().fg(Color::Cyan))),popup);
                }
                Mode::NewOutbound(selected) => {
                    let mut state = ListState::default().with_selected(Some(*selected));
                    frame.render_stateful_widget(List::new(OUTBOUND_KINDS.iter().map(|s|ListItem::new(*s))).highlight_symbol("› ").highlight_style(Style::default().bg(Color::Blue)).block(Block::default().title(t("新增出站节点 · ↑↓ 选择 · Enter 确认 · Esc 取消")).borders(Borders::ALL)),popup,&mut state);
                }
                Mode::Header(input) => frame.render_widget(Paragraph::new(if last_key(&self.form.path)==Some("proxy_groups") {tr!("Proxy group name: {}▏\nEnter confirm · Esc cancel", input.text)} else if self.form.rename_rule_set.is_some() {tr!("Rename rule set: {}▏\nEnter confirm · Esc cancel",input.text)}else if last_key(&self.form.path)==Some("rule_sets") {tr!("规则集名称：{}▏\nEnter 创建 · Esc 取消",input.text)}else{tr!("请求头名称：{}▏\nEnter 继续设置值 · Esc 取消",input.text)}).block(Block::default().title(if last_key(&self.form.path)==Some("proxy_groups") {t("Proxy groups")}else if last_key(&self.form.path)==Some("rule_sets"){t("规则集")}else{t("新增请求头")}).borders(Borders::ALL)),popup),
                Mode::Delete{..} => frame.render_widget(Paragraph::new(t("删除所选条目/分组？\n只修改草稿，Ctrl-S 后写入配置。\ny 确认删除 · n / Esc 取消")).wrap(Wrap{trim:false}).block(Block::default().title(t("确认删除")).borders(Borders::ALL).style(Style::default().fg(Color::Yellow))),popup),
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
    #[test]
    fn rule_set_management_renames_references_toggles_and_blocks_dangling_deletion() {
        let mut editor = Editor::new("rules".into(), "socket='r'\ndefault_action='direct'\nrules=[{kind='rule_set',value='ads',action='block'},{kind='rule_set',value='ads',action='direct'}]\n[rule_sets.ads]\nbehavior='domain'\npayload=['+.ads.test']\n[rule_sets.other]\npayload=[]\n".into());
        let original = editor.text();
        assert!(editor.rename_rule_set("ads", "other").is_err());
        assert_eq!(editor.text(), original);
        let doc = editor.document().unwrap();
        let set_path = vec![Part::Key("rule_sets".into()), Part::Key("ads".into())];
        editor.focus_field(&doc, &set_path, "enabled");
        press(&mut editor, KeyCode::Char('r'));
        editor.paste("renamed");
        let saved = serde_json::to_string(&editor).unwrap();
        editor = serde_json::from_str(&saved).unwrap();
        press(&mut editor, KeyCode::Enter);
        let doc = editor.document().unwrap();
        assert!(doc["rule_sets"].get("ads").is_none());
        assert_eq!(rule_set_references(&doc, Some("renamed")), 2);
        assert_eq!(doc["rules"][0]["action"].as_str(), Some("block"));
        press(&mut editor, KeyCode::Char('e'));
        assert_eq!(
            editor.document().unwrap()["rule_sets"]["renamed"]["enabled"].as_bool(),
            Some(false)
        );
        let doc = editor.document().unwrap();
        editor.form.selected = editor.visible_rows(&doc).iter().position(|row| matches!(row, FlatRow::Section { path, .. } if rule_set_name(path) == Some("renamed"))).unwrap();
        assert!(editor.key(key(KeyCode::Char('d'))).is_err());
        assert_eq!(
            rule_set_references(&editor.document().unwrap(), Some("renamed")),
            2
        );
        press(&mut editor, KeyCode::Char('r'));
        editor.paste("cancelled");
        press(&mut editor, KeyCode::Esc);
        assert!(editor.document().unwrap()["rule_sets"]
            .get("renamed")
            .is_some());
        for language in [
            crate::i18n::Language::English,
            crate::i18n::Language::Simplified,
            crate::i18n::Language::Traditional,
        ] {
            crate::i18n::with_language(language, || {
                let mut terminal = Terminal::new(TestBackend::new(56, 12)).unwrap();
                terminal
                    .draw(|frame| editor.render(frame, frame.area()))
                    .unwrap();
            });
        }
    }
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
    fn open_field(editor: &mut Editor, name: &str) {
        let doc = editor.document().unwrap();
        let path = editor.form.path.clone();
        editor.focus_field(&doc, &path, name);
        press(editor, KeyCode::Enter);
    }
    #[test]
    fn node_references_filter_complete_select_cancel_and_follow_draft_tags() {
        let mut e = Editor::new("proxy".into(), "standalone=true\nstandalone_outbound='edge-a'\n[[outbounds]]\nkind='socks5'\ntag='edge-a'\nserver='127.0.0.1:1'\n[[outbounds]]\nkind='socks5'\ntag='中文节点'\nserver='127.0.0.1:2'\n".into()).with_kind(CoreKind::Proxy);
        open_field(&mut e, "standalone_outbound");
        e.paste("中文");
        press(&mut e, KeyCode::Tab);
        assert!(matches!(&e.form.mode, Mode::Field {input,..} if input.text=="中文节点"));
        press(&mut e, KeyCode::Esc);
        assert_eq!(
            e.document().unwrap()["standalone_outbound"].as_str(),
            Some("edge-a")
        );
        open_field(&mut e, "standalone_outbound");
        e.paste("中文");
        press(&mut e, KeyCode::Down);
        let mut restored: Editor =
            serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
        let saved = restored.prepare_save().unwrap();
        assert_eq!(
            saved.parse::<DocumentMut>().unwrap()["standalone_outbound"].as_str(),
            Some("中文节点")
        );
        restored.form.path = node_path();
        field(&mut restored, "tag", "renamed");
        restored.form.path.clear();
        let values = restored
            .reference_values(&restored.document().unwrap(), "standalone_outbound")
            .unwrap();
        assert!(values.contains(&"renamed".into()));
        assert!(!values.contains(&"edge-a".into()));
    }
    #[test]
    fn route_actions_accept_picked_or_short_tags_and_keep_explicit_manual_values() {
        let mut e = Editor::new("rules".into(), "socket='r'\ndefault_action='direct'\n[[rules]]\nkind='domain_exact'\nvalue='a.test'\naction='direct'\n".into()).with_kind(CoreKind::Rule);
        e.set_node_tags(vec!["edge".into(), "proxy:edge".into(), "other".into()]);
        open_field(&mut e, "default_action");
        e.paste("ed");
        press(&mut e, KeyCode::Down);
        press(&mut e, KeyCode::Enter);
        assert_eq!(
            e.document().unwrap()["default_action"].as_str(),
            Some("proxy:edge")
        );
        e.form.path = vec![Part::Key("rules".into()), Part::Index(0)];
        field(&mut e, "action", "other");
        assert_eq!(
            e.document().unwrap()["rules"][0]["action"].as_str(),
            Some("proxy:other")
        );
        field(&mut e, "action", "proxy:edge");
        assert_eq!(
            e.document().unwrap()["rules"][0]["action"].as_str(),
            Some("proxy:edge")
        );
        field(&mut e, "action", "proxy:external-unregistered");
        assert_eq!(
            e.document().unwrap()["rules"][0]["action"].as_str(),
            Some("proxy:external-unregistered")
        );
        let text = e.text();
        e.import(text).unwrap();
        assert_eq!(e.node_tags.len(), 3);
    }
    #[test]
    fn default_proxy_dynamic_candidates_and_empty_explicit_members_are_editable() {
        let mut e=Editor::new("p".into(),"standalone=true\nstandalone_outbound='PROXY'\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\nkind='socks5'\ntag='JP-a'\nserver='127.0.0.1:1081'\n[[outbounds]]\nkind='socks5'\ntag='US-b'\nserver='127.0.0.1:1082'".into());
        e.form.path = vec![Part::Key("proxy_groups".into()), Part::Key("PROXY".into())];
        let doc = e.document().unwrap();
        assert_eq!(
            doc["proxy_groups"]["PROXY"]["include_all"].as_bool(),
            Some(true)
        );
        assert_eq!(
            e.reference_values(&doc, "selected").unwrap(),
            ["JP-a", "US-b"]
        );
        field(&mut e, "filter", "^JP-");
        assert_eq!(
            e.reference_values(&e.document().unwrap(), "selected")
                .unwrap(),
            ["JP-a"]
        );
        field(&mut e, "selected", "JP-a");
        let doc = e.document().unwrap();
        let path = e.form.path.clone();
        e.focus_field(&doc, &path, "members");
        press(&mut e, KeyCode::Enter);
        press(&mut e, KeyCode::Enter);
        assert!(e.document().unwrap()["proxy_groups"]["PROXY"]["members"]
            .as_array()
            .unwrap()
            .is_empty());
        assert_eq!(
            e.document().unwrap()["proxy_groups"]["PROXY"]["selected"].as_str(),
            Some("JP-a")
        );
        assert!(e.key(key(KeyCode::Char('r'))).is_err());
    }
    #[test]
    fn proxy_group_picker_selection_renaming_and_references_survive_draft_restore() {
        let mut e = Editor::new("p".into(), "standalone=true\nstandalone_outbound='main'\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\ntag='a'\nkind='socks5'\nserver='127.0.0.1:1081'\n[[outbounds]]\ntag='b'\nkind='socks5'\nserver='127.0.0.1:1082'\n[proxy_groups.main]\nmode='select'\nmembers=['a','b']\nselected='b'".into());
        e.form.path = vec![Part::Key("proxy_groups".into()), Part::Key("main".into())];
        for flag in [
            "disconnect_on_manual",
            "disconnect_on_auto",
            "disconnect_on_failure",
            "disconnect_on_recovery",
            "disconnect_on_config_change",
        ] {
            field(&mut e, flag, "true");
            assert_eq!(
                e.document().unwrap()["proxy_groups"]["main"][flag].as_bool(),
                Some(true)
            );
        }
        field(&mut e, "selected", "a");
        assert_eq!(
            e.document().unwrap()["proxy_groups"]["main"]["selected"].as_str(),
            Some("a")
        );
        field(&mut e, "mode", "auto");
        assert!(!e
            .rows(&e.document().unwrap())
            .iter()
            .any(|row| matches!(row,Row::Field(s) if s.key=="selected")));
        field(&mut e, "mode", "select");
        let doc = e.document().unwrap();
        let path = e.form.path.clone();
        e.focus_field(&doc, &path, "members");
        press(&mut e, KeyCode::Enter);
        if let Mode::Members {
            options, cursor, ..
        } = &mut e.form.mode
        {
            *cursor = options.iter().position(|v| v == "a").unwrap();
        } else {
            panic!("expected member picker");
        }
        press(&mut e, KeyCode::Char(' '));
        let encoded = serde_json::to_string(&e).unwrap();
        let mut e: Editor = serde_json::from_str(&encoded).unwrap();
        e.normalize();
        press(&mut e, KeyCode::Enter);
        assert_eq!(
            e.document().unwrap()["proxy_groups"]["main"]["selected"].as_str(),
            Some("b")
        );
        press(&mut e, KeyCode::Char('r'));
        e.paste("renamed");
        press(&mut e, KeyCode::Enter);
        assert_eq!(
            e.document().unwrap()["standalone_outbound"].as_str(),
            Some("renamed")
        );
        assert!(e.document().unwrap()["proxy_groups"].get("main").is_none());
        e.form.path = vec![Part::Key("outbounds".into()), Part::Index(1)];
        field(&mut e, "tag", "new-b");
        assert_eq!(
            e.document().unwrap()["proxy_groups"]["renamed"]["members"][0].as_str(),
            Some("new-b")
        );
        assert_eq!(
            e.document().unwrap()["proxy_groups"]["renamed"]["selected"].as_str(),
            Some("new-b")
        );
        let path = vec![
            Part::Key("proxy_groups".into()),
            Part::Key("renamed".into()),
        ];
        e.form.selected = e
            .visible_rows(&e.document().unwrap())
            .iter()
            .position(|r| r.path() == path)
            .unwrap();
        assert!(
            e.key(key(KeyCode::Char('d'))).is_err(),
            "default group cannot be deleted"
        );
        for language in [
            crate::i18n::Language::English,
            crate::i18n::Language::Simplified,
            crate::i18n::Language::Traditional,
        ] {
            crate::i18n::with_language(language, || {
                let doc = e.document().unwrap();
                e.focus_field(&doc, &path, "members");
                press(&mut e, KeyCode::Enter);
                for (width, height) in [(100, 24), (30, 8)] {
                    let mut terminal =
                        ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                            .unwrap();
                    terminal
                        .draw(|frame| e.render(frame, frame.area()))
                        .unwrap();
                }
                press(&mut e, KeyCode::Esc);
            });
        }
    }
    #[test]
    fn new_proxy_group_opens_member_picker_and_is_available_as_default() {
        let mut e=Editor::new("p".into(),"standalone=true\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\nkind='socks5'\ntag='a'\nserver='127.0.0.1:1081'".into());
        let path = vec![Part::Key("proxy_groups".into())];
        e.form.selected = e
            .visible_rows(&e.document().unwrap())
            .iter()
            .position(|r| r.path() == path)
            .unwrap();
        press(&mut e, KeyCode::Char('n'));
        e.paste("my-group");
        press(&mut e, KeyCode::Enter);
        if let Mode::Members {
            options, cursor, ..
        } = &mut e.form.mode
        {
            *cursor = options.iter().position(|v| v == "a").unwrap();
        } else {
            panic!("expected picker");
        }
        press(&mut e, KeyCode::Char(' '));
        press(&mut e, KeyCode::Enter);
        e.form.path.clear();
        assert!(e
            .reference_values(&e.document().unwrap(), "standalone_outbound")
            .unwrap()
            .contains(&"my-group".into()));
        field(&mut e, "standalone_outbound", "my-group");
        assert_eq!(
            e.document().unwrap()["proxy_groups"]["my-group"]["members"][0].as_str(),
            Some("a")
        );
    }
    #[test]
    fn changing_rule_kind_uses_real_references_and_resets_incompatible_values() {
        let mut e = Editor::new("rules".into(), "socket='r'\ndefault_action='direct'\n[[rules]]\nkind='domain_regex'\nvalue='^rx[0-9]+$'\naction='block'\n[rule_sets.ads]\npayload=[]\n".into());
        e.form.path = vec![Part::Key("rules".into()), Part::Index(0)];
        field(&mut e, "kind", "domain_exact");
        assert_eq!(
            e.document().unwrap()["rules"][0]["value"].as_str(),
            Some("example.com")
        );
        field(&mut e, "kind", "domain_pattern");
        assert_eq!(
            e.document().unwrap()["rules"][0]["value"].as_str(),
            Some("*.example.com")
        );
        field(&mut e, "kind", "rule_set");
        assert_eq!(
            e.document().unwrap()["rules"][0]["value"].as_str(),
            Some("ads")
        );
        assert_eq!(
            e.document().unwrap()["rules"][0]["action"].as_str(),
            Some("block")
        );
        let mut e = Editor::new("rules".into(), "socket='r'\ndefault_action='direct'\n[[rules]]\nkind='domain_exact'\nvalue='example.com'\naction='block'".into());
        e.form.path = vec![Part::Key("rules".into()), Part::Index(0)];
        let before = e.text();
        open_field(&mut e, "kind");
        e.paste("rule_set");
        assert!(e.commit_field().is_err());
        assert_eq!(e.text(), before);
    }
    #[test]
    fn rule_set_network_and_enum_fields_share_filter_and_completion() {
        let mut e = Editor::new("rules".into(), "socket='r'\ndefault_action='direct'\n[[rules]]\nkind='rule_set'\nvalue='old'\naction='direct'\n[rule_sets.local]\npayload=[]\n[rule_sets.remote]\npayload=[]\n".into());
        e.form.path = vec![Part::Key("rules".into()), Part::Index(0)];
        open_field(&mut e, "value");
        e.paste("loc");
        press(&mut e, KeyCode::Tab);
        press(&mut e, KeyCode::Enter);
        assert_eq!(
            e.document().unwrap()["rules"][0]["value"].as_str(),
            Some("local")
        );
        open_field(&mut e, "kind");
        e.paste("netw");
        press(&mut e, KeyCode::Tab);
        press(&mut e, KeyCode::Enter);
        assert_eq!(
            e.document().unwrap()["rules"][0]["kind"].as_str(),
            Some("network")
        );
        open_field(&mut e, "value");
        e.paste("u");
        press(&mut e, KeyCode::Down);
        press(&mut e, KeyCode::Enter);
        assert_eq!(
            e.document().unwrap()["rules"][0]["value"].as_str(),
            Some("udp")
        );
        open_field(&mut e, "value");
        e.paste("no-match");
        press(&mut e, KeyCode::Tab);
        assert!(matches!(&e.form.mode,Mode::Field{input,..} if input.text=="no-match"));
        press(&mut e, KeyCode::Esc);
    }
    #[test]
    fn reference_popup_keeps_selected_candidate_visible_in_all_locales() {
        let mut e = Editor::new(
            "rules".into(),
            "socket='r'\ndefault_action='direct'\n".into(),
        )
        .with_kind(CoreKind::Rule);
        e.set_node_tags((0..30).map(|i| format!("node-{i:02}")).collect());
        open_field(&mut e, "default_action");
        for _ in 0..25 {
            press(&mut e, KeyCode::Down);
        }
        let candidate = match &e.form.mode {
            Mode::Field { input, .. } => input.candidate.clone().unwrap(),
            _ => unreachable!(),
        };
        for lang in [
            crate::i18n::Language::English,
            crate::i18n::Language::Simplified,
            crate::i18n::Language::Traditional,
        ] {
            crate::i18n::with_language(lang, || {
                for (width, height) in [(100, 28), (45, 14), (20, 8)] {
                    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                    terminal.draw(|f| e.render(f, f.area())).unwrap();
                    if width == 100 {
                        let text: String = terminal
                            .backend()
                            .buffer()
                            .content
                            .iter()
                            .map(|c| c.symbol())
                            .collect();
                        assert!(text.contains(&candidate), "{text}");
                    }
                }
            });
        }
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
        if cfg!(target_os = "macos") {
            e.form.path = path(&["system_proxy"]);
            field(&mut e, "services", "[\"Wi-Fi\"]");
        }
        let parsed: toml::Value = toml::from_str(&e.text()).unwrap();
        assert_eq!(parsed["watch_config"].as_bool(), Some(true));
        assert_eq!(parsed["tun"]["routes"].as_array().unwrap().len(), 2);
        #[cfg(target_os = "macos")]
        assert_eq!(
            parsed["system_proxy"]["services"][0].as_str(),
            Some("Wi-Fi")
        );
    }

    #[test]
    fn hidden_platform_fields_are_preserved_and_not_reintroduced() {
        let mut e = Editor::new("proxy".into(), "# preserve platform settings\nstandalone = true\n[tproxy]\nlisten = '127.0.0.1:7893'\n[redirect]\nlisten = '127.0.0.1:7894'\n[system_proxy]\nservices = ['Wi-Fi']\n".into());
        let platform = crate::availability::Platform::current();
        let doc = e.document().unwrap();
        let rows = e.visible_rows(&doc);
        for key in ["tproxy", "redirect"] {
            assert_eq!(rows.iter().any(|row| matches!(row, FlatRow::Section { path, .. } if last_key(path) == Some(key))), platform.option(None, key));
        }
        let services = e.schema_rows(&doc, &path(&["system_proxy"]));
        assert_eq!(
            services
                .iter()
                .any(|row| matches!(row, Row::Field(spec) if spec.key == "services")),
            platform.option(Some("system_proxy"), "services")
        );
        field(&mut e, "watch_config", "true");
        let saved = e.prepare_save().unwrap();
        let parsed: toml::Value = toml::from_str(&saved).unwrap();
        assert_eq!(parsed["tproxy"]["listen"].as_str(), Some("127.0.0.1:7893"));
        assert_eq!(
            parsed["redirect"]["listen"].as_str(),
            Some("127.0.0.1:7894")
        );
        assert_eq!(
            parsed["system_proxy"]["services"][0].as_str(),
            Some("Wi-Fi")
        );
        assert!(saved.starts_with("# preserve platform settings"));
    }

    #[test]
    fn localized_forms_keep_credentials_hidden_and_handle_small_screens() {
        use crate::i18n::{with_language, Language};
        for (language, title, host) in [
            (Language::English, "Configuration", "Server host"),
            (Language::Simplified, "编辑", "服务器主机"),
            (Language::Traditional, "編輯", "伺服器主機"),
        ] {
            with_language(language, || {
                let mut e = native();
                e.form.path = node_path();
                e.form.path.extend(path(&["config"]));
                field(&mut e, "uuid", "do-not-show-this-secret");
                let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
                terminal
                    .draw(|frame| e.render(frame, frame.area()))
                    .unwrap();
                let text: String = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect();
                assert!(
                    text.replace(' ', "").contains(&title.replace(' ', "")),
                    "{text}"
                );
                let doc = e.document().unwrap();
                assert!(e.schema_rows(&doc, &e.form.path).iter().any(|row| matches!(row, Row::Field(spec) if spec.key == "server" && spec.label == host)));
                assert!(!text.contains("do-not-show-this-secret"));
                let mut tiny = Terminal::new(TestBackend::new(20, 8)).unwrap();
                tiny.draw(|frame| e.render(frame, frame.area())).unwrap();
            });
        }
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
        assert!(tls_fields.iter().any(|r| matches!(r, Row::Field(spec) if spec.key == "cert_pin_sha256" && spec.label.contains("SHA-256"))));
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
    fn process_rules_and_local_default_are_editable_without_losing_rule_order() {
        let mut e = Editor::new("rules".into(), "socket='/tmp/a'\ndefault_action='proxy:edge'\n[[rules]]\nkind='domain_suffix'\nvalue='example.com'\naction='block'\n".into());
        field(&mut e, "direct_local", "false");
        e.form.path = vec![Part::Key("rules".into()), Part::Index(0)];
        field(&mut e, "kind", "process_name");
        field(&mut e, "value", "browser.exe");
        assert_eq!(
            e.document().unwrap()["rules"][0]["kind"].as_str(),
            Some("process_name")
        );
        field(&mut e, "kind", "process_path");
        field(&mut e, "value", "C:/Apps/browser.exe");
        assert_eq!(
            e.document().unwrap()["rules"][0]["value"].as_str(),
            Some("C:/Apps/browser.exe")
        );
        assert_eq!(e.document().unwrap()["direct_local"].as_bool(), Some(false));
    }
    #[test]
    fn geoip_and_named_rule_sets_can_be_created_and_edited() {
        let mut e = Editor::new(
            "rules".into(),
            "socket='/tmp/a'\ndefault_action='direct'\n".into(),
        );
        field(&mut e, "geoip_database", "Country.mmdb");
        field(&mut e, "geosite_database", "geosite.dat");
        let doc = e.document().unwrap();
        e.form.selected = e
            .visible_rows(&doc)
            .iter()
            .position(
                |row| matches!(row,FlatRow::Section{path,..} if last_key(path)==Some("rule_sets")),
            )
            .unwrap();
        press(&mut e, KeyCode::Char('n'));
        e.paste("ads");
        press(&mut e, KeyCode::Enter);
        field(&mut e, "payload", "['+.ads.test']");
        let doc = e.document().unwrap();
        assert_eq!(doc["rule_sets"]["ads"]["behavior"].as_str(), Some("domain"));
        assert_eq!(
            doc["rule_sets"]["ads"]["payload"]
                .as_array()
                .unwrap()
                .get(0)
                .unwrap()
                .as_str(),
            Some("+.ads.test")
        );
        assert_eq!(doc["geoip_database"].as_str(), Some("Country.mmdb"));
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
        assert!(text.replace(' ', "").contains("编辑"));
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
    fn rule_categories_fold_with_vim_keys_without_changing_configuration() {
        let mut e = rules();
        let original = e.text();
        let doc = e.document().unwrap();
        let rows = e.visible_rows(&doc);
        assert!(!rows.iter().any(|r| matches!(r, FlatRow::Rule { .. })));
        e.form.selected = rows
            .iter()
            .position(
                |row| matches!(row,FlatRow::Collection {path,..} if last_key(path)==Some("rules")),
            )
            .unwrap();
        press(&mut e, KeyCode::Char('l'));
        assert!(e
            .visible_rows(&doc)
            .iter()
            .any(|row| matches!(row, FlatRow::RuleGroup { .. })));
        assert!(!e
            .visible_rows(&doc)
            .iter()
            .any(|row| matches!(row, FlatRow::Rule { .. })));
        press(&mut e, KeyCode::Char('z'));
        press(&mut e, KeyCode::Char('R'));
        let rows = e.visible_rows(&doc);
        for index in 0..2 {
            for key in ["kind", "value", "action"] {
                assert!(rows.iter().any(|r|matches!(r,FlatRow::Field{path,spec,..} if path == &vec![Part::Key("rules".into()),Part::Index(index)] && spec.key == key)));
            }
        }
        let indexes: Vec<_> = rows
            .iter()
            .filter_map(|row| {
                if let FlatRow::Rule { index, .. } = row {
                    Some(*index)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            indexes,
            (0..doc["rules"].as_array_of_tables().unwrap().len()).collect::<Vec<_>>()
        );
        press(&mut e, KeyCode::Char('G'));
        assert_eq!(e.form.selected, rows.len() - 1);
        press(&mut e, KeyCode::Char('g'));
        press(&mut e, KeyCode::Char('g'));
        assert_eq!(e.form.selected, 0);
        press(&mut e, KeyCode::Char('z'));
        press(&mut e, KeyCode::Char('M'));
        assert!(!e
            .visible_rows(&doc)
            .iter()
            .any(|row| matches!(row, FlatRow::Rule { .. })));
        assert_eq!(e.text(), original);
        assert!(e.can_close());
    }
    #[test]
    fn vim_navigation_does_not_capture_field_text_or_reorder_rule_groups() {
        let mut e = Editor::new("rules".into(), "socket='r'\ndefault_action='direct'\nrules=[{kind='domain_exact',value='first.test',action='block'},{kind='ip_cidr',value='10.0.0.0/8',action='direct'},{kind='domain_exact',value='last.test',action='block'}]".into());
        let original = e.text();
        let doc = e.document().unwrap();
        let groups = rule_groups(doc["rules"].as_array_of_tables().unwrap());
        assert_eq!(
            groups.iter().map(|v| v.0).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        e.focus_field(&doc, &[], "default_action");
        press(&mut e, KeyCode::Char('l'));
        for ch in "hjklgg".chars() {
            press(&mut e, KeyCode::Char(ch));
        }
        assert!(matches!(&e.form.mode,Mode::Field{input,..} if input.text == "hjklgg"));
        press(&mut e, KeyCode::Esc);
        assert_eq!(e.text(), original);
        e.focus_field(
            &doc,
            &vec![Part::Key("rules".into()), Part::Index(1)],
            "value",
        );
        press(&mut e, KeyCode::Char('h'));
        assert!(matches!(
            e.visible_rows(&doc).get(e.form.selected),
            Some(FlatRow::RuleGroup { start: 1, .. })
        ));
        press(&mut e, KeyCode::Char('h'));
        assert!(matches!(
            e.visible_rows(&doc).get(e.form.selected),
            Some(FlatRow::Collection { .. })
        ));
        assert_eq!(e.text(), original);
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
