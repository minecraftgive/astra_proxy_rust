//! Entry-oriented configuration forms. TOML stays an implementation detail of saving.
use crate::protocol::Kind as CoreKind;
use crate::{i18n::t, tr};
use anyhow::{bail, Context, Result};
use astra_app::edit::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use toml_edit::ArrayOfTables;
use toml_edit::{value, DocumentMut, Item, Table, TableLike};

use astra_model::document::{DocumentPath as Path, PathPart as Part};
#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// Entry whose placeholder fields are being filled one after another.
    #[serde(default)]
    guide: Option<Path>,
    #[serde(default)]
    last_search: String,
    #[serde(skip)]
    vim_g: bool,
    #[serde(skip)]
    marked_rules: Vec<usize>,
    /// Tags identify the chosen nodes even while unrelated rows fold or expand.
    #[serde(skip)]
    marked_nodes: Vec<String>,
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
        /// `/` query while typing; Enter or Esc keeps the cursor on the match.
        #[serde(default)]
        find: Option<String>,
    },
    Delete {
        key: Option<String>,
        index: Option<usize>,
    },
    Search(TextInput),
}
use astra_tui_kit::TextInput;

// Keep the old draft representation so existing ui-session.json files migrate without loss.
#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Editor {
    #[serde(skip)]
    interfaces: Vec<String>,
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
    #[serde(default)]
    subscription_nodes: std::collections::BTreeMap<String, crate::protocol::SubscriptionNodes>,
    #[serde(skip)]
    history: Vec<String>,
    #[serde(skip)]
    future: Vec<String>,
    #[serde(skip)]
    notice: Option<String>,
}
const HISTORY_LIMIT: usize = 200;
// Template values of a new entry; guided filling stops on each of them.
const PLACEHOLDERS: &[&str] = &[
    "node.example.com",
    "node.example.com:443",
    "127.0.0.1:7890",
    "change-me",
    "00000000-0000-0000-0000-000000000001",
    "example.com",
];

use astra_app::schema::{Row, Spec, Type, OUTBOUND_KINDS};
#[derive(Clone)]
enum FlatRow {
    UpdateSubscriptions,
    Info {
        path: Path,
        label: String,
    },
    Field {
        path: Path,
        spec: Spec,
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
            Self::UpdateSubscriptions => vec![Part::Key("subscriptions".into())],
            Self::Info { path, .. }
            | Self::Field { path, .. }
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
fn expanded_proxy_groups(
    doc: &DocumentMut,
    snapshots: &std::collections::BTreeMap<String, crate::protocol::SubscriptionNodes>,
) -> Result<std::collections::BTreeMap<String, astra_model::groups::Group>> {
    let value: toml::Value = toml::from_str(&doc.to_string())?;
    let mut groups: std::collections::BTreeMap<String, astra_model::groups::Group> = value
        .get("proxy_groups")
        .cloned()
        .map(|g| g.try_into())
        .transpose()?
        .unwrap_or_default();
    let mut nodes: std::collections::HashSet<String> = doc
        .get("outbounds")
        .and_then(Item::as_array_of_tables)
        .into_iter()
        .flat_map(|a| a.iter())
        .filter_map(|n| n.get("tag").and_then(Item::as_str))
        .map(str::to_owned)
        .collect();
    for group in groups.values_mut() {
        let Some(source) = &group.subscription else {
            continue;
        };
        let settings = if source == "__legacy__" {
            value.get("subscription")
        } else {
            value.get("subscriptions").and_then(|v| v.get(source))
        };
        if let Some(snapshot) = snapshots.get(source).filter(|snapshot| {
            settings
                .and_then(|v| v.get("url"))
                .and_then(toml::Value::as_str)
                == Some(snapshot.url.as_str())
        }) {
            group.members = snapshot.tags.clone();
            nodes.extend(snapshot.tags.iter().cloned());
        }
    }
    astra_model::groups::ensure_default(&mut groups, &nodes).map_err(anyhow::Error::msg)?;
    astra_model::groups::expand(&groups, &nodes).map_err(anyhow::Error::msg)
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
    pub fn set_subscription_nodes(
        &mut self,
        nodes: std::collections::BTreeMap<String, crate::protocol::SubscriptionNodes>,
    ) {
        self.subscription_nodes = nodes;
    }
    pub fn requests_subscription_update(&self, key: KeyEvent) -> bool {
        self.document().is_ok_and(|doc| self.is_proxy(&doc))
            && self.browsing()
            && ((key.code == KeyCode::Char('r') && key.modifiers.contains(KeyModifiers::CONTROL))
                || (key.code == KeyCode::Enter
                    && self
                        .document()
                        .ok()
                        .and_then(|doc| {
                            self.visible_rows(&doc)
                                .get(self.form.selected)
                                .map(|row| matches!(row, FlatRow::UpdateSubscriptions))
                        })
                        .unwrap_or(false)))
    }
    fn reference_label(&self, doc: &DocumentMut, key: &str, tag: &str) -> String {
        if key == "interface" && proxy_group_name(&self.form.path).is_some() && tag.is_empty() {
            return t("Inherit default interface").to_owned();
        }
        if key == "interface" && last_key(&self.form.path) == Some("tun") && tag.is_empty() {
            return t("Automatic interface").to_owned();
        }
        if tag.is_empty() {
            return t("(未设置)").to_owned();
        }
        if key == "selected" {
            if let Some(root) = proxy_group_name(&self.form.path) {
                if let Ok(groups) = expanded_proxy_groups(doc, &self.subscription_nodes) {
                    if let Some(path) = astra_model::groups::member_path(&groups, root, tag) {
                        return path.join(" › ");
                    }
                }
            }
        }
        tag.to_owned()
    }
    #[cfg(test)]
    pub fn subscription_page(&self) -> bool {
        self.document().ok().and_then(|doc|self.visible_rows(&doc).get(self.form.selected).map(FlatRow::path)).is_some_and(|path|matches!(path.first(),Some(Part::Key(key)) if matches!(key.as_str(),"subscription"|"subscriptions")))
    }
    pub fn set_node_tags(&mut self, tags: Vec<String>) {
        self.node_tags = tags;
    }
    fn reference_values(&self, doc: &DocumentMut, key: &str) -> Option<Vec<String>> {
        if key == "interface"
            && (last_key(&self.form.path) == Some("tun")
                || proxy_group_name(&self.form.path).is_some())
        {
            let mut values = vec![String::new()];
            values.extend(self.interfaces.iter().cloned());
            // Retain an existing override when an adapter is temporarily absent.
            if let Some(current) = table_at(doc.as_table(), &self.form.path)
                .and_then(|table| table.get(key))
                .and_then(Item::as_str)
                .filter(|name| !name.is_empty() && !values.iter().any(|v| v == name))
            {
                values.push(current.to_owned());
            }
            return Some(values);
        }
        if key == "proxy"
            && (last_key(&self.form.path) == Some("subscription")
                || (self.form.path.len() == 2
                    && self.form.path.first() == Some(&Part::Key("subscriptions".into()))))
        {
            let mut values = vec!["PROXY".into(), "DIRECT".into()];
            for target in proxy_targets(doc) {
                if !values.contains(&target) {
                    values.push(target);
                }
            }
            return Some(values);
        }
        if key == "selected" && proxy_group_name(&self.form.path).is_some() {
            if let Ok(groups) = expanded_proxy_groups(doc, &self.subscription_nodes) {
                if groups.contains_key(proxy_group_name(&self.form.path).unwrap()) {
                    return Some(astra_model::groups::member_choices(
                        &groups,
                        proxy_group_name(&self.form.path).unwrap(),
                    ));
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
                    // direct / block already stand for the built-in DIRECT / REJECT groups.
                    .filter(|tag| !(action && astra_model::groups::builtin(tag)))
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
        let text = self.text();
        let parsed: DocumentMut = text
            .parse()
            .context(t("配置格式无法解析。原草稿已保留，请修复配置后重新打开"))?;
        astra_app::edit::parse_editor_document(&text, self.is_proxy(&parsed), t)
    }
    fn update(&mut self, mut doc: DocumentMut) {
        self.form.marked_rules.clear();
        self.form.marked_nodes.clear();
        position_tables(doc.as_table_mut(), &mut 0);
        let text = doc.to_string();
        let previous = self.text();
        if text != previous {
            self.remember(previous);
        }
        self.lines = text.split('\n').map(str::to_owned).collect();
    }
    fn remember(&mut self, previous: String) {
        astra_app::history::remember(&mut self.history, &mut self.future, previous, HISTORY_LIMIT);
    }
    /// Steps the draft back (or forward) one edit; fold state is clamped to what still exists.
    fn restore(&mut self, redo: bool) -> Result<()> {
        let previous = self.lines.join("\n");
        let Some(text) =
            astra_app::history::restore(&mut self.history, &mut self.future, previous, redo)
        else {
            self.notice = Some(
                t(if redo {
                    "Nothing to redo"
                } else {
                    "Nothing to undo"
                })
                .into(),
            );
            return Ok(());
        };
        self.lines = text.split('\n').map(str::to_owned).collect();
        self.form.marked_rules.clear();
        self.form.marked_nodes.clear();
        self.form.guide = None;
        let doc = self.document()?;
        let count = collection(&doc, &[Part::Key("outbounds".into())]).map_or(0, |a| a.len());
        self.form.expanded_nodes.retain(|i| *i < count);
        self.form.selected = self
            .form
            .selected
            .min(self.visible_rows(&doc).len().saturating_sub(1));
        self.notice = Some(t(if redo { "Redone" } else { "Undone" }).into());
        Ok(())
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
            Mode::Field { input, .. } | Mode::Header(input) | Mode::Search(input) => {
                input.insert(text)
            }
            _ => {}
        }
    }
    pub fn browsing(&self) -> bool {
        matches!(self.form.mode, Mode::Browse)
    }
    /// A member picker or delete confirmation is not a text field.
    pub fn accepts_text_input(&self) -> bool {
        matches!(
            self.form.mode,
            Mode::Field { .. }
                | Mode::Header(_)
                | Mode::Search(_)
                | Mode::Members { find: Some(_), .. }
        )
    }
    pub fn import(&mut self, text: String) -> Result<()> {
        let mut candidate = Self::new(self.name.clone(), text);
        candidate.kind = self.kind;
        candidate.node_tags = self.node_tags.clone();
        candidate.subscription_nodes = self.subscription_nodes.clone();
        candidate.document()?;
        candidate.original = self.original.clone();
        candidate.history = std::mem::take(&mut self.history);
        candidate.remember(self.text());
        *self = candidate;
        Ok(())
    }
    pub fn cancel_input(&mut self) {
        self.form.mode = Mode::Browse;
        self.form.guide = None;
        self.form.rename_rule_set = None;
        self.form.rename_proxy_group = None;
    }
    pub fn can_close(&self) -> bool {
        if !self.browsing() {
            return false;
        }
        let Ok(doc) = self.document() else {
            return true;
        };
        let rows = self.visible_rows(&doc);
        let Some(row) = rows.get(self.form.selected) else {
            return true;
        };
        let path = row.path();
        !row.node().is_some_and(|i| self.form.expanded_nodes.contains(&i))
            && !proxy_group_name(&path).is_some_and(|name| self.form.expanded_proxy_groups.iter().any(|v| v==name))
            && !rule_set_name(&path).is_some_and(|name| self.form.expanded_rule_sets.iter().any(|v|v==name))
            && !path.first().is_some_and(|part| matches!(part,Part::Key(key) if self.form.expanded_collections.contains(key)))
    }
    pub fn find_again(&mut self, backwards: bool) -> Result<()> {
        let query = self.form.last_search.clone();
        if query.is_empty() {
            return Ok(());
        }
        self.search_direction(&query, backwards)
    }
    pub fn prepare_save(&mut self) -> Result<String> {
        match self.form.mode {
            Mode::Field { .. } => self.commit_field()?,
            Mode::Browse => {}
            _ => bail!(t("请先确认或取消当前条目窗口，再保存配置")),
        }
        let mut doc = self.document()?;
        if self.is_proxy(&doc) && doc.remove("allow_lan").is_some() {
            self.update(doc);
        }
        Ok(self.text())
    }
    /// Return the tag for the selected outbound row.
    pub(crate) fn selected_node_tag(&self) -> Result<String> {
        let doc = self.document()?;
        let rows = self.visible_rows(&doc);
        // Only the item's own row counts; its expanded field rows do not.
        let row = rows
            .get(self.form.selected)
            .context(t("Select a node to export"))?;
        let FlatRow::Node { index, .. } = row else {
            bail!(t("Select a node to export"));
        };
        collection(&doc, &[Part::Key("outbounds".into())])
            .and_then(|nodes| nodes.get(*index))
            .and_then(|node| node.get("tag"))
            .and_then(Item::as_str)
            .map(str::to_owned)
            .context(t("Selected node has no tag"))
    }

    /// Marked nodes are exported in source order; without marks use the cursor row.
    pub(crate) fn selected_node_tags(&self) -> Result<Vec<String>> {
        if self.form.marked_nodes.is_empty() {
            return Ok(vec![self.selected_node_tag()?]);
        }
        let doc = self.document()?;
        let tags: Vec<_> = collection(&doc, &[Part::Key("outbounds".into())])
            .into_iter()
            .flat_map(|nodes| nodes.iter())
            .filter_map(|node| node.get("tag").and_then(Item::as_str))
            .filter(|tag| {
                self.form
                    .marked_nodes
                    .iter()
                    .any(|marked| marked.as_str() == *tag)
            })
            .map(str::to_owned)
            .collect();
        anyhow::ensure!(
            tags.len() == self.form.marked_nodes.len()
                && self.form.marked_nodes.iter().all(|marked| tags
                    .iter()
                    .filter(|tag| *tag == marked)
                    .count()
                    == 1),
            t("Selected nodes changed; select them again")
        );
        Ok(tags)
    }

    pub(crate) fn export_selected_node(&self) -> Result<String> {
        crate::export::selected_nodes(&self.text(), &self.selected_node_tags()?)
    }
    pub(crate) fn export_selected_node_share(&self) -> Result<String> {
        crate::export::selected_nodes_share(&self.text(), &self.selected_node_tags()?)
    }

    /// Return the zero-based rule index for the selected rule row.
    pub(crate) fn selected_rule_index(&self) -> Result<usize> {
        let doc = self.document()?;
        let rows = self.visible_rows(&doc);
        // Only the item's own row counts; its expanded field rows do not.
        let row = rows
            .get(self.form.selected)
            .context(t("Select a rule to export"))?;
        let FlatRow::Rule { index, .. } = row else {
            bail!(t("Select a rule to export"));
        };
        anyhow::ensure!(
            collection(&doc, &[Part::Key("rules".into())])
                .is_some_and(|rules| *index < rules.len()),
            t("Select a rule to export")
        );
        Ok(*index)
    }

    /// Export the rule represented by the currently selected visible row.
    pub(crate) fn export_selected_rule(&self) -> Result<String> {
        let index = self.selected_rule_index()?;
        crate::export::selected_rule(&self.text(), index)
    }

    fn schema_rows(&self, doc: &DocumentMut, path: &[Part]) -> Vec<Row> {
        astra_app::schema::rows_for(
            doc,
            path,
            self.kind.unwrap_or(if self.is_proxy(doc) {
                CoreKind::Proxy
            } else {
                CoreKind::Rule
            }),
            crate::availability::Platform::current(),
            t,
        )
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
        self.flatten_table(doc, &[], &mut rows);
        rows
    }
    fn flatten_table(&self, doc: &DocumentMut, path: &[Part], output: &mut Vec<FlatRow>) {
        if path.len() > 32 {
            return;
        }
        if matches!(path, [Part::Key(key)] if matches!(key.as_str(), "subscriptions" | "subscription"))
        {
            output.push(FlatRow::UpdateSubscriptions);
        }
        for row in self.schema_rows(doc, path) {
            match row {
                Row::Field(spec) => output.push(FlatRow::Field {
                    path: path.to_vec(),
                    spec,
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
                                        self.flatten_table(doc, &item, output);
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
                                    self.flatten_table(doc, &item, output);
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
                        self.flatten_table(doc, &child, output);
                        if key == "headers" || (path.is_empty() && key == "subscriptions") {
                            output.push(FlatRow::AddHeader { path: child });
                        }
                    }
                }
                Row::Entry { .. } => {}
            }
        }
        let source = if path == [Part::Key("subscription".into())] {
            Some("__legacy__")
        } else if path.len() == 2 && path.first() == Some(&Part::Key("subscriptions".into())) {
            last_key(path)
        } else {
            None
        };
        if let Some(source) = source {
            let url = table_at(doc.as_table(), path)
                .and_then(|v| v.get("url"))
                .and_then(Item::as_str)
                .unwrap_or_default();
            let nodes = self
                .subscription_nodes
                .get(source)
                .map(|v| {
                    if v.url == url {
                        v.tags.clone()
                    } else {
                        Vec::new()
                    }
                })
                .unwrap_or_else(|| {
                    table_at(doc.as_table(), path)
                        .and_then(|v| v.get("managed_nodes"))
                        .and_then(Item::as_array)
                        .map(|v| {
                            v.iter()
                                .filter_map(|v| v.as_str().map(str::to_owned))
                                .collect()
                        })
                        .unwrap_or_default()
                });
            output.push(FlatRow::Info {
                path: path.to_vec(),
                label: tr!("Subscription nodes ({})", nodes.len()),
            });
            if nodes.is_empty() {
                output.push(FlatRow::Info {
                    path: path.to_vec(),
                    label: t("No nodes available · save and update subscription").into(),
                });
            }
            for node in nodes {
                output.push(FlatRow::Info {
                    path: path.to_vec(),
                    label: format!("  • {node}"),
                });
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
    fn field_item(spec: &Spec, input: &str, remove: bool) -> Result<Item> {
        Ok(match &spec.kind {
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
                    choices.contains(&input),
                    tr!("请用 ↑↓ 选择支持的值：{}", choices.join(", "))
                );
                value(input)
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
                value(input)
            }
        })
    }
    fn replace_outbound_fields(table: &mut dyn TableLike, input: &str) {
        let tag = table.get("tag").cloned();
        let replacement = outbound_template(input, "edge");
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
    }
    fn replace_native_fields(table: &mut dyn TableLike, input: &str) {
        let shared: Vec<_> = ["server", "port", "uuid", "password"]
            .into_iter()
            .filter_map(|key| table.get(key).cloned().map(|v| (key, v)))
            .collect();
        let tls = table.get("tls").cloned();
        let mut replacement = native_config(input);
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
        let interface_selection = key == "interface" && last_key(&self.form.path) == Some("tun");
        if interface_selection
            || (key == "interface" && proxy_group_name(&self.form.path).is_some())
        {
            anyhow::ensure!(
                self.reference_values(&doc, &key)
                    .is_some_and(|values| values.contains(&input)),
                t("Select a local network adapter with ↑↓")
            );
        }
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
        let item = Self::field_item(&spec, &input, remove)?;
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
        if interface_selection {
            replace_field(table, "auto_interface", value(input.is_empty()));
        }
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
                Self::replace_outbound_fields(table, &input);
            } else if key == "protocol"
                && last_key(&self.form.path) == Some("config")
                && previous != input
            {
                Self::replace_native_fields(table, &input);
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
                if spec.key == "interface"
                    && (last_key(&path) == Some("tun") || proxy_group_name(&path).is_some())
                {
                    let exclude = doc
                        .get("tun")
                        .and_then(Item::as_table_like)
                        .and_then(|table| table.get("name"))
                        .and_then(Item::as_str);
                    self.interfaces = crate::network_interfaces::names(exclude)
                        .context(t("Could not list local network adapters"))?;
                }
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
                    // A built-in group keeps its own terminal outbound as a choice.
                    options.retain(|v| v != name || astra_model::groups::builtin(name));
                    self.form.path = path;
                    self.form.mode = Mode::Members {
                        options,
                        chosen,
                        cursor: 0,
                        find: None,
                    };
                    return Ok(());
                }
                let current = table_at(doc.as_table(), &path)
                    .and_then(|t| t.get(&spec.key))
                    .map(scalar)
                    .unwrap_or(spec.default);
                let toggle = matches!(spec.kind, Type::Boolean);
                let input = if toggle {
                    // Booleans flip in place instead of opening a true/false picker.
                    TextInput::new((current != "true").to_string())
                } else {
                    TextInput::new(current)
                };
                self.form.path = path;
                self.form.mode = Mode::Field {
                    key: spec.key,
                    input,
                    reveal: false,
                };
                if toggle {
                    let result = self.commit_field();
                    if result.is_err() {
                        self.form.mode = Mode::Browse;
                    }
                    return result;
                }
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
                    Some("tls" | "system_proxy" | "tun" | "dns" | "tproxy" | "redirect")
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
            FlatRow::UpdateSubscriptions | FlatRow::Info { .. } | FlatRow::Section { .. } => {}
        }
        Ok(())
    }
    fn expand_all(&mut self, doc: &DocumentMut) {
        self.form.expanded_collections =
            vec!["rules".into(), "rule_sets".into(), "proxy_groups".into()];
        self.form.expanded_rule_groups = collection(doc, &[Part::Key("rules".into())])
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
        self.form.expanded_nodes = (0..collection(doc, &[Part::Key("outbounds".into())])
            .map(|a| a.len())
            .unwrap_or(0))
            .collect();
    }
    /// Identifies a row independently of fold state.
    fn row_id(row: &FlatRow) -> (Path, String) {
        let kind = match row {
            FlatRow::Field { spec, .. } => spec.key.clone(),
            FlatRow::Info { label, .. } => format!("info:{label}"),
            FlatRow::RuleGroup { start, .. } => format!("group:{start}"),
            FlatRow::AddHeader { .. } => "add".into(),
            FlatRow::UpdateSubscriptions => "update".into(),
            _ => String::new(),
        };
        (row.path(), kind)
    }
    fn search_text(doc: &DocumentMut, row: &FlatRow) -> String {
        match row {
            FlatRow::Field { path, spec, .. } => {
                let value = table_at(doc.as_table(), path)
                    .and_then(|t| t.get(&spec.key))
                    .filter(|_| !spec.secret())
                    .map(scalar)
                    .unwrap_or_default();
                format!("{} {} {value}", spec.label, spec.key)
            }
            FlatRow::Collection { label, .. }
            | FlatRow::Node { label, .. }
            | FlatRow::Rule { label, .. }
            | FlatRow::RuleGroup { label, .. }
            | FlatRow::Section { label, .. }
            | FlatRow::Info { label, .. } => label.clone(),
            FlatRow::AddHeader { .. } | FlatRow::UpdateSubscriptions => String::new(),
        }
        .to_lowercase()
    }
    /// Jumps to the next row containing `query`, opening folds on the way when needed.
    fn search(&mut self, query: &str) -> Result<()> {
        self.search_direction(query, false)
    }
    fn search_direction(&mut self, query: &str, backwards: bool) -> Result<()> {
        let doc = self.document()?;
        let query = query.to_lowercase();
        let current = self
            .visible_rows(&doc)
            .get(self.form.selected)
            .map(Self::row_id);
        let mut all = self.clone();
        all.expand_all(&doc);
        let rows = all.visible_rows(&doc);
        let start = current
            .and_then(|id| rows.iter().position(|row| Self::row_id(row) == id))
            .unwrap_or(if backwards {
                0
            } else {
                rows.len().saturating_sub(1)
            });
        let Some(target) = (1..=rows.len())
            .map(|offset| {
                &rows[if backwards {
                    (start + rows.len() - offset) % rows.len()
                } else {
                    (start + offset) % rows.len()
                }]
            })
            .find(|row| Self::search_text(&doc, row).contains(&query))
        else {
            self.notice = Some(tr!("No match for \"{}\"", query));
            return Ok(());
        };
        let id = Self::row_id(target);
        let inside = matches!(
            target,
            FlatRow::Field { .. } | FlatRow::Info { .. } | FlatRow::AddHeader { .. }
        );
        self.reveal(&doc, &id.0, inside);
        if let FlatRow::RuleGroup { start, .. } = target {
            if !self.form.expanded_rule_groups.contains(start) {
                self.form.expanded_rule_groups.push(*start);
            }
        }
        if let Some(at) = self
            .visible_rows(&doc)
            .iter()
            .position(|row| Self::row_id(row) == id)
        {
            self.form.selected = at;
        }
        Ok(())
    }
    /// Opens every fold that hides the row at `path`; `inside` rows also need that table open.
    fn reveal(&mut self, doc: &DocumentMut, path: &[Part], inside: bool) {
        self.expand_rule_path(doc, path);
        let open = if inside {
            path
        } else {
            &path[..path.len().saturating_sub(1)]
        };
        if open.len() < 2 {
            return;
        }
        let entry = &open[..2];
        if let Some(name) = proxy_group_name(entry) {
            if !self.form.expanded_proxy_groups.iter().any(|k| k == name) {
                self.form.expanded_proxy_groups.push(name.into());
            }
        }
        if let Some(name) = rule_set_name(entry) {
            if !self.form.expanded_rule_sets.iter().any(|k| k == name) {
                self.form.expanded_rule_sets.push(name.into());
            }
        }
        if let Some(index) = node_index(entry) {
            self.expand_node(index);
        }
    }
    /// Copies the selected node or rule right below itself.
    fn duplicate(&mut self) -> Result<()> {
        let mut doc = self.document()?;
        let path = self
            .visible_rows(&doc)
            .get(self.form.selected)
            .map(FlatRow::path)
            .unwrap_or_default();
        let (kind, index) = item_index(&path).context(t("Select a node or rule to duplicate"))?;
        let kind = kind.to_owned();
        let array = collection_mut(&mut doc, &[Part::Key(kind.clone())])?;
        let mut tables: Vec<Table> = array.iter().cloned().collect();
        let mut copy = tables[index].clone();
        if kind == "outbounds" {
            let base = copy.get("tag").and_then(Item::as_str).unwrap_or("edge");
            let base = base.to_owned();
            let targets = proxy_targets(&doc);
            let tag = (2..)
                .map(|n| format!("{base}-{n}"))
                .find(|tag| !targets.contains(tag))
                .unwrap();
            copy["tag"] = value(tag);
        }
        tables.insert(index + 1, copy);
        *collection_mut(&mut doc, &[Part::Key(kind.clone())])? = tables.into_iter().collect();
        if kind == "outbounds" {
            for expanded in &mut self.form.expanded_nodes {
                if *expanded > index {
                    *expanded += 1;
                }
            }
        }
        self.update(doc);
        let doc = self.document()?;
        self.focus_item(&doc, &kind, index + 1);
        self.notice = Some(t("Duplicated · u / Ctrl-Z undo").into());
        Ok(())
    }
    /// Opens the next field of the guided entry that is required or still holds a template value.
    fn guide_next(&mut self) -> Result<()> {
        let Some(root) = self.form.guide.clone() else {
            return Ok(());
        };
        let doc = self.document()?;
        let rows = self.visible_rows(&doc);
        let next = rows
            .iter()
            .enumerate()
            .skip(self.form.selected + 1)
            .find(|(_, row)| match row {
                FlatRow::Field { path, spec, .. }
                    if path.starts_with(&root) && !matches!(spec.kind, Type::Boolean) =>
                {
                    let current = table_at(doc.as_table(), path)
                        .and_then(|t| t.get(&spec.key))
                        .map(scalar);
                    matches!(spec.key.as_str(), "tag" | "port" | "action")
                        || current
                            .as_deref()
                            .is_some_and(|v| PLACEHOLDERS.contains(&v))
                        || (spec.required && current.is_none())
                }
                _ => false,
            })
            .map(|(at, _)| at);
        match next {
            Some(at) => {
                self.form.selected = at;
                self.enter(false)?;
            }
            None => {
                self.form.guide = None;
                self.notice = Some(t("All template values replaced · Ctrl-S save").into());
            }
        }
        Ok(())
    }
    fn confirm_delete(&mut self) -> Result<()> {
        let Mode::Delete { key: field, index } = self.form.mode.clone() else {
            return Ok(());
        };
        let (field, index) = (field.as_ref(), index.as_ref());
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
                anyhow::ensure!(
                    !proxy_references(&doc, field),
                    t("Proxy target is referenced; change its groups or default before deleting")
                );
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
        self.notice = Some(t("Deleted · u / Ctrl-Z undo").into());
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
        // Walk through the template values so a new entry needs no manual navigation.
        self.form.guide = Some(path);
        if last_key(&self.form.path) == Some("rules") {
            self.enter(false)
        } else {
            self.guide_next()
        }
    }
    pub fn key(&mut self, key: KeyEvent) -> Result<()> {
        self.notice = None;
        match &self.form.mode {
            Mode::Search(_) => self.key_search(key),
            Mode::Browse => self.key_browse(key),
            Mode::Members { .. } => self.key_members(key),
            Mode::Field { .. } => self.key_field(key),
            Mode::Header(_) => self.key_header(key),
            Mode::NewOutbound(_) => self.key_picker(key),
            Mode::Delete { .. } => self.key_delete(key),
        }
    }
    fn key_search(&mut self, key: KeyEvent) -> Result<()> {
        if let Mode::Search(input) = &mut self.form.mode {
            match key.code {
                KeyCode::Esc => self.cancel_input(),
                KeyCode::Enter => {
                    let query = input.text.trim().to_owned();
                    self.cancel_input();
                    if !query.is_empty() {
                        self.form.last_search = query.clone();
                        self.search(&query)?;
                    }
                }
                _ => input.key(key),
            }
            return Ok(());
        }
        Ok(())
    }
    fn key_members(&mut self, key: KeyEvent) -> Result<()> {
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
                find,
            } = &mut self.form.mode
            {
                if let Some(query) = find {
                    // Typing jumps to the first member containing the text; ↑↓ step between matches.
                    let matches = |name: &String, query: &str| {
                        name.to_lowercase().contains(&query.to_lowercase())
                    };
                    match key.code {
                        KeyCode::Enter | KeyCode::Esc => *find = None,
                        KeyCode::Backspace => {
                            query.pop();
                        }
                        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                            query.push(c);
                            if let Some(index) = options.iter().position(|o| matches(o, query)) {
                                *cursor = index;
                            }
                        }
                        KeyCode::Down => {
                            if let Some(index) =
                                (*cursor + 1..options.len()).find(|&i| matches(&options[i], query))
                            {
                                *cursor = index;
                            }
                        }
                        KeyCode::Up => {
                            if let Some(index) =
                                (0..*cursor).rev().find(|&i| matches(&options[i], query))
                            {
                                *cursor = index;
                            }
                        }
                        _ => {}
                    }
                    return Ok(());
                }
                match key.code {
                    KeyCode::Char('/') => *find = Some(String::new()),
                    KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        *find = Some(String::new())
                    }
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
                let groups = expanded_proxy_groups(&doc, &self.subscription_nodes)?;
                if let Some(effective) = groups.get(name) {
                    if effective.selected.as_ref().is_some_and(|s| {
                        astra_model::groups::member_path(&groups, name, s).is_none()
                    }) {
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
        Ok(())
    }
    fn key_field(&mut self, key: KeyEvent) -> Result<()> {
        if matches!(self.form.mode, Mode::Field { .. }) {
            if key.code == KeyCode::Esc {
                self.cancel_input();
                return Ok(());
            }
            if key.code == KeyCode::Enter {
                self.commit_field()?;
                return self.guide_next();
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
        Ok(())
    }
    fn key_header(&mut self, key: KeyEvent) -> Result<()> {
        if let Mode::Header(input) = &mut self.form.mode {
            match key.code {
                KeyCode::Esc => self.cancel_input(),
                KeyCode::Enter => {
                    let key = input.text.trim().to_owned();
                    if last_key(&self.form.path) == Some("subscriptions") {
                        anyhow::ensure!(
                            !key.is_empty() && key.len() <= 64 && key != "__legacy__",
                            t("Enter a subscription name")
                        );
                        let mut doc = self.document()?;
                        let table = table_at_mut(doc.as_table_mut(), &self.form.path)?;
                        anyhow::ensure!(
                            table.get(&key).is_none(),
                            t("Subscription name already exists")
                        );
                        let mut source = Table::new();
                        source["url"] = value("");
                        source["interval"] = value(3600);
                        table.insert(&key, Item::Table(source));
                        let mut data: toml::Table = toml::from_str(&doc.to_string())?;
                        astra_convert::subscription::prepare_groups(&mut data)?;
                        let groups = data.get("proxy_groups").and_then(|v| v.as_table()).unwrap();
                        for (name, group) in groups.iter().filter(|(_, g)| {
                            g.get("subscription").and_then(|v| v.as_str()) == Some(key.as_str())
                        }) {
                            let snippet: DocumentMut = toml::to_string(group)?.parse()?;
                            table_at_mut(doc.as_table_mut(), &[Part::Key("proxy_groups".into())])?
                                .insert(name, Item::Table(snippet.as_table().clone()));
                        }
                        self.update(doc);
                        self.cancel_input();
                        self.form.path.push(Part::Key(key));
                        self.focus_field(&self.document()?, &self.form.path.clone(), "url");
                        self.enter(false)?;
                        return Ok(());
                    }
                    if last_key(&self.form.path) == Some("proxy_groups") {
                        anyhow::ensure!(
                            !key.is_empty()
                                && key.len() <= 64
                                && !key.chars().any(char::is_control)
                                && !astra_model::groups::reserved(&key),
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
        Ok(())
    }
    fn key_picker(&mut self, key: KeyEvent) -> Result<()> {
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
        Ok(())
    }
    fn key_delete(&mut self, key: KeyEvent) -> Result<()> {
        // Only drafts saved by older versions still sit in this confirmation.
        if matches!(self.form.mode, Mode::Delete { .. }) {
            match key.code {
                KeyCode::Esc | KeyCode::Char('n') => self.cancel_input(),
                KeyCode::Char('y') | KeyCode::Enter => self.confirm_delete()?,
                _ => {}
            }
            return Ok(());
        }
        Ok(())
    }
    fn key_browse(&mut self, mut key: KeyEvent) -> Result<()> {
        if matches!(self.form.mode, Mode::Browse) {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Char('z') if ctrl => return self.restore(false),
                KeyCode::Char('u') if key.modifiers.is_empty() => return self.restore(false),
                KeyCode::Char('y') if ctrl => return self.restore(true),
                KeyCode::Char('y') if key.modifiers.is_empty() => return self.duplicate(),
                KeyCode::Char('/') => {
                    let mut input = TextInput::new(self.form.last_search.clone());
                    input.replace = true;
                    self.form.mode = Mode::Search(input);
                    return Ok(());
                }
                KeyCode::Char(' ') => {
                    let doc = self.document()?;
                    let boolean = matches!(self.visible_rows(&doc).get(self.form.selected), Some(FlatRow::Field { spec, .. }) if matches!(spec.kind, Type::Boolean));
                    if boolean {
                        return self.enter(false);
                    }
                    return Ok(());
                }
                _ => {}
            }
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
                        self.expand_all(&doc);
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
                        !astra_model::groups::reserved(&name),
                        t("Built-in groups PROXY, DIRECT and REJECT cannot be renamed or deleted")
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
                    // Back leaves a rule's fields before its group; Left/h remains
                    // the explicit collapse command and folds the group immediately.
                    let path = row.path();
                    if let Some(("rules", index)) =
                        item_index(&path).filter(|_| !matches!(row, FlatRow::Rule { .. }))
                    {
                        self.focus_item(&doc, "rules", index);
                    } else {
                        self.collapse_selection(&doc, row);
                    }
                }
            }
            KeyCode::Char('n') => {
                let path = selected.as_ref().map(FlatRow::path).unwrap_or_default();
                if path.first() == Some(&Part::Key("subscriptions".into())) {
                    self.form.path = vec![Part::Key("subscriptions".into())];
                    self.form.mode = Mode::Header(TextInput::default());
                    return Ok(());
                }
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
            KeyCode::Char('d') => {
                match selected {
                    Some(FlatRow::Field { path, spec, .. }) => {
                        anyhow::ensure!(!spec.required, t("必填项不能删除，请按 Enter 修改"));
                        // DIRECT/REJECT have no dynamic members to fall back on.
                        anyhow::ensure!(
                            !(spec.key == "members"
                                && proxy_group_name(&path)
                                    .is_some_and(astra_model::groups::builtin)),
                            t("Choose at least one group member")
                        );
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
                    Some(
                        FlatRow::Section { mut path, .. } | FlatRow::Collection { mut path, .. },
                    ) => {
                        if let Some(name) = proxy_group_name(&path) {
                            anyhow::ensure!(
                                !astra_model::groups::reserved(name),
                                t("Built-in groups PROXY, DIRECT and REJECT cannot be renamed or deleted")
                            );
                            anyhow::ensure!(!proxy_references(&doc, name), t("Proxy target is referenced; change its groups or default before deleting"));
                        }
                        anyhow::ensure!(
                            path != vec![Part::Key("proxy_groups".into())],
                            t("Built-in groups PROXY, DIRECT and REJECT cannot be renamed or deleted")
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
                }
                // Deleting is immediate; undo replaces the old confirmation step.
                if matches!(self.form.mode, Mode::Delete { .. }) {
                    let result = self.confirm_delete();
                    if result.is_err() {
                        self.cancel_input();
                    }
                    result?;
                }
            }
            KeyCode::Char('v') | KeyCode::Char('V') => {
                let path = selected.as_ref().map(FlatRow::path).unwrap_or_default();
                if matches!(selected, Some(FlatRow::Node { .. })) {
                    self.form.marked_rules.clear();
                    if key.code == KeyCode::Char('V') {
                        let nodes = collection(&doc, &[Part::Key("outbounds".into())])
                            .context(t("Select a node to export"))?;
                        let tags: Vec<String> = nodes
                            .iter()
                            .map(|node| {
                                node.get("tag")
                                    .and_then(Item::as_str)
                                    .map(str::to_owned)
                                    .context(t("Selected node has no tag"))
                            })
                            .collect::<Result<_>>()?;
                        if tags.len() == self.form.marked_nodes.len()
                            && tags.iter().all(|tag| self.form.marked_nodes.contains(tag))
                        {
                            self.form.marked_nodes.clear();
                        } else {
                            self.form.marked_nodes = tags;
                        }
                    } else {
                        let tag = self.selected_node_tag()?;
                        if self.form.marked_nodes.contains(&tag) {
                            self.form.marked_nodes.retain(|marked| *marked != tag);
                        } else {
                            self.form.marked_nodes.push(tag);
                        }
                    }
                } else if let Some(("rules", index)) = item_index(&path) {
                    self.form.marked_nodes.clear();
                    if key.code == KeyCode::Char('V') {
                        let count = doc["rules"].as_array_of_tables().map_or(0, |a| a.len());
                        if self.form.marked_rules.len() == count {
                            self.form.marked_rules.clear();
                        } else {
                            self.form.marked_rules = (0..count).collect();
                        }
                    } else if self.form.marked_rules.contains(&index) {
                        self.form.marked_rules.retain(|i| *i != index);
                    } else {
                        self.form.marked_rules.push(index);
                    }
                }
            }
            KeyCode::Char('m') if !self.form.marked_rules.is_empty() => {
                let path = selected.as_ref().map(FlatRow::path).unwrap_or_default();
                if let Some(("rules", target)) = item_index(&path) {
                    self.move_marked_rules(Some(target), false)?;
                }
            }
            KeyCode::Char('K') | KeyCode::Char('J') => {
                let path = selected.as_ref().map(FlatRow::path).unwrap_or_default();
                let (kind, index) =
                    item_index(&path).context(t("请先选择要调整顺序的规则或节点"))?;
                if kind == "rules" && !self.form.marked_rules.is_empty() {
                    self.move_marked_rules(None, key.code == KeyCode::Char('J'))?;
                    return Ok(());
                }
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
    /// Reorder by original indices so non-adjacent selections keep their relative order.
    fn move_marked_rules(&mut self, target: Option<usize>, down: bool) -> Result<()> {
        let mut doc = self.document()?;
        let array = collection_mut(&mut doc, &[Part::Key("rules".into())])?;
        let marked = self.form.marked_rules.clone();
        let mut order: Vec<usize> = (0..array.len()).collect();
        if let Some(target) = target {
            // Dropping on the selection itself is a no-op.
            if marked.contains(&target) {
                return Ok(());
            }
            let picked: Vec<_> = order
                .iter()
                .copied()
                .filter(|i| marked.contains(i))
                .collect();
            order.retain(|i| !marked.contains(i));
            let at = order
                .iter()
                .position(|i| *i == target)
                .unwrap_or(order.len());
            order.splice(at..at, picked);
        } else if down {
            for i in (0..order.len().saturating_sub(1)).rev() {
                if marked.contains(&order[i]) && !marked.contains(&order[i + 1]) {
                    order.swap(i, i + 1);
                }
            }
        } else {
            for i in 1..order.len() {
                if marked.contains(&order[i]) && !marked.contains(&order[i - 1]) {
                    order.swap(i, i - 1);
                }
            }
        }
        let tables: Vec<_> = array.iter().cloned().collect();
        *array = order.iter().map(|i| tables[*i].clone()).collect();
        let next: Vec<_> = order
            .iter()
            .enumerate()
            .filter_map(|(i, old)| marked.contains(old).then_some(i))
            .collect();
        self.update(doc);
        self.form.marked_rules = next;
        let doc = self.document()?;
        if let Some(index) = self.form.marked_rules.first().copied() {
            self.focus_item(&doc, "rules", index);
        }
        Ok(())
    }
    /// Indentation level of a row; replaces the old "Rule 3 / " style label prefixes.
    fn depth(row: &FlatRow) -> usize {
        let depth = match row {
            FlatRow::Field { path, .. } => {
                path.len() + usize::from(path.first() == Some(&Part::Key("rules".into())))
            }
            FlatRow::Section { path, .. } => path.len().saturating_sub(1),
            FlatRow::Info { path, .. } | FlatRow::AddHeader { path } => path.len(),
            FlatRow::Collection { .. } => 0,
            FlatRow::Node { .. } | FlatRow::RuleGroup { .. } | FlatRow::UpdateSubscriptions => 1,
            FlatRow::Rule { .. } => 2,
        };
        depth.min(6)
    }
    /// Browse-time hints that say nothing once a field is already open.
    fn generic_help(help: &str) -> bool {
        [
            "Enter / Space toggle · d reset to default",
            "Enter edit · required",
            "Enter edit · d reset to default",
        ]
        .iter()
        .any(|generic| t(generic) == help)
    }
    fn hint(&self, row: Option<&FlatRow>) -> String {
        if !self.form.marked_nodes.is_empty() {
            return format!(
                "{} · {}",
                self.form.marked_nodes.len(),
                t("nodes selected · v toggle · V all/clear · F2 export")
            );
        }
        if !self.form.marked_rules.is_empty() {
            return format!(
                "{} · {}",
                self.form.marked_rules.len(),
                t("selected · v toggle · V all/clear · K/J move · m move before cursor")
            );
        }
        if self.form.guide.is_some() {
            return t("Filling new entry · Enter next field · Esc stop").into();
        }
        match row {
            Some(row)
                if proxy_group_name(&row.path()).is_some()
                    || row.path() == vec![Part::Key("proxy_groups".into())] =>
            {
                t("Enter edit/fold · n new group · r rename · d delete").into()
            }
            Some(FlatRow::RuleGroup { .. }) | Some(FlatRow::Collection { .. }) => {
                t("Enter / ←→ fold · n add · zM/zR fold/open all").into()
            }
            Some(row) if rule_set_name(&row.path()).is_some() => {
                t("Enter expand/collapse · r rename · e enable/disable · d delete").into()
            }
            Some(FlatRow::Field { spec, .. }) => spec.help.clone(),
            Some(FlatRow::Node { .. }) => {
                t("Enter fold · v select · V all/clear · y duplicate · d delete · K/J move").into()
            }
            Some(FlatRow::Rule { .. }) => {
                t("v select · V all/clear · K/J move · m move selection here").into()
            }
            Some(FlatRow::Section { path, .. }) if last_key(path) == Some("tls") => {
                t("Enter 启用/关闭 TLS；字段始终平铺；d 删除本分组参数").into()
            }
            Some(FlatRow::AddHeader { path }) => t(if last_key(path) == Some("subscriptions") {
                "Enter add subscription · n add · d remove · Ctrl-S save and update"
            } else {
                "Enter 新增请求头；已有请求头直接在上方修改"
            })
            .into(),
            _ => t("Enter edit · n add").into(),
        }
    }
    /// Highlighting style for a field; rule values follow the rule's kind.
    fn flavor(spec: &Spec, table: Option<&dyn TableLike>) -> crate::highlight::Flavor {
        let kind = table.and_then(|t| t.get("kind")).and_then(Item::as_str);
        crate::highlight::flavor(&spec.key, matches!(spec.kind, Type::Number(..)), kind)
    }
    pub fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let dirty = self.text() != self.original;
        let mut title = vec![Span::raw(format!(
            " {} ",
            tr!("Configuration · {}{}", self.name, "")
        ))];
        if dirty {
            title.push(Span::styled(
                tr!("● {} ", t("unsaved")),
                Style::default().fg(Color::Yellow),
            ));
        }
        if !self.form.marked_nodes.is_empty() {
            title.push(Span::styled(
                format!(
                    " · {} ",
                    tr!("{} nodes selected", self.form.marked_nodes.len())
                ),
                Style::default().fg(Color::Cyan),
            ));
        }
        let title = Line::from(title);
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
            .constraints([Constraint::Min(3), Constraint::Length(2)])
            .split(area);
        let list = self.render_rows(frame, sections[0], &doc, &rows, title);
        self.render_footer(frame, sections[1], &rows);
        self.render_popup(frame, area, sections[0], &doc, &list);
    }

    fn render_rows(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        doc: &DocumentMut,
        rows: &[FlatRow],
        title: Line<'_>,
    ) -> (ListState, usize, bool) {
        // Fields changed since the last save carry a marker in the left gutter.
        let mut saved = Editor::new(String::new(), self.original.clone());
        saved.kind = self.kind;
        let saved = saved.document().ok();
        let expanded_groups = if self.is_proxy(doc) {
            expanded_proxy_groups(doc, &self.subscription_nodes).ok()
        } else {
            None
        };
        let label_column = rows
            .iter()
            .filter_map(|row| match row {
                FlatRow::Field { spec, .. } => {
                    Some(Self::depth(row) * 2 + Span::raw(spec.label.as_str()).width() + 2)
                }
                _ => None,
            })
            .max()
            .unwrap_or(0)
            .min((area.width as usize * 2 / 5).max(16));
        let fold = |open: bool| if open { "▾ " } else { "▸ " };
        let dim = Style::default().fg(Color::DarkGray);
        let heading = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        let items: Vec<ListItem> = rows
            .iter()
            .map(|row| {
                let indent = "  ".repeat(Self::depth(row));
                let mut changed = false;
                let mut spans: Vec<Span> = match row {
                    FlatRow::Field { path, spec, .. } => {
                        let table = table_at(doc.as_table(), path);
                        let current = table.and_then(|t| t.get(&spec.key));
                        changed = saved.as_ref().is_some_and(|saved| {
                            table_at(saved.as_table(), path)
                                .and_then(|t| t.get(&spec.key))
                                .map(scalar)
                                != current.map(scalar)
                        });
                        let tls_disabled = last_key(path) == Some("tls") && table.is_none();
                        let mut label = format!("{indent}{}", spec.label);
                        let width = Span::raw(label.as_str()).width();
                        label.push_str(&" ".repeat(label_column.saturating_sub(width).max(2)));
                        let value = if tls_disabled {
                            vec![Span::styled(
                                t("TLS off · filling any field enables it"),
                                dim,
                            )]
                        } else if matches!(spec.kind, Type::Boolean) {
                            let on = current.map(scalar).unwrap_or(spec.default.clone()) == "true";
                            let text = format!("{} {on}", if on { "[x]" } else { "[ ]" });
                            vec![if current.is_some() {
                                Span::styled(
                                    text,
                                    Style::default().fg(if on {
                                        Color::Green
                                    } else {
                                        Color::Reset
                                    }),
                                )
                            } else {
                                Span::styled(tr!("{} · {}", text, t("default")), dim)
                            }]
                        } else if let Some(item) = current {
                            let text = scalar(item);
                            if spec.secret() && !text.is_empty() {
                                vec![Span::raw("••••••••")]
                            } else if PLACEHOLDERS.contains(&text.as_str()) {
                                vec![Span::styled(
                                    tr!("{}  ← {}", text, t("template value")),
                                    Style::default().fg(Color::Yellow),
                                )]
                            } else {
                                crate::highlight::spans(
                                    &text.replace('\n', " ↵ "),
                                    Self::flavor(spec, table),
                                )
                            }
                        } else if spec.required {
                            vec![Span::styled(
                                tr!(
                                    "{} · {}",
                                    t("required"),
                                    if spec.secret() {
                                        t("输入凭据")
                                    } else {
                                        &spec.default
                                    }
                                ),
                                Style::default().fg(Color::Yellow),
                            )]
                        } else if spec.default.is_empty() {
                            vec![Span::styled("—", dim)]
                        } else {
                            vec![Span::styled(tr!("{} {}", t("default"), spec.default), dim)]
                        };
                        let mut spans = vec![Span::raw(label)];
                        spans.extend(value);
                        spans
                    }
                    FlatRow::Collection { path, label } => {
                        let count = collection(doc, path).map(|a| a.len()).unwrap_or(0);
                        let mark = if last_key(path) == Some("rules") {
                            fold(self.form.expanded_collections.contains(&"rules".into()))
                        } else {
                            "  "
                        };
                        vec![
                            Span::styled(format!("{indent}{mark}{label}"), heading),
                            Span::styled(tr!("  {} · n add", count), dim),
                        ]
                    }
                    FlatRow::Node { index, label } => vec![Span::styled(
                        format!(
                            "{indent}{}{label}",
                            fold(self.form.expanded_nodes.contains(index))
                        ),
                        Style::default().add_modifier(Modifier::BOLD),
                    )],
                    FlatRow::Rule { label, .. } => vec![Span::styled(
                        format!("{indent}{label}"),
                        Style::default().fg(Color::Cyan),
                    )],
                    FlatRow::RuleGroup { start, end, label } => vec![
                        Span::styled(
                            format!(
                                "{indent}{}{label}",
                                fold(self.form.expanded_rule_groups.contains(start))
                            ),
                            Style::default().fg(Color::Cyan),
                        ),
                        Span::styled(tr!(" · rules {}–{}", start + 1, end + 1), dim),
                    ],
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
                            rule_set_summary(doc, path)
                        } else if matches!(
                            last_key(path),
                            Some("tls" | "system_proxy" | "tun" | "dns" | "tproxy" | "redirect")
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
                            fold(
                                self.form
                                    .expanded_collections
                                    .iter()
                                    .any(|v| Some(v.as_str()) == last_key(path)),
                            )
                        } else if let Some(name) = proxy_group_name(path) {
                            fold(self.form.expanded_proxy_groups.iter().any(|v| v == name))
                        } else {
                            rule_set_name(path)
                                .map(|name| {
                                    fold(self.form.expanded_rule_sets.iter().any(|key| key == name))
                                })
                                .unwrap_or("§ ")
                        };
                        let style = if path.len() == 1 {
                            heading
                        } else {
                            Style::default().fg(Color::Cyan)
                        };
                        vec![
                            Span::styled(format!("{indent}{mark}{label}"), style),
                            Span::styled(suffix, dim),
                        ]
                    }
                    FlatRow::UpdateSubscriptions => vec![Span::styled(
                        tr!("{}↻ {} · Ctrl-R", indent, t("Update subscriptions")),
                        Style::default().fg(Color::Cyan),
                    )],
                    FlatRow::Info { label, .. } => {
                        vec![Span::styled(format!("{indent}{label}"), dim)]
                    }
                    FlatRow::AddHeader { path } => vec![Span::styled(
                        format!(
                            "{indent}{}",
                            t(if last_key(path) == Some("subscriptions") {
                                "+ Add subscription (Enter)"
                            } else {
                                "＋ 新增请求头（Enter）"
                            })
                        ),
                        Style::default().fg(Color::Cyan),
                    )],
                };
                spans.insert(
                    0,
                    if changed {
                        Span::styled("● ", Style::default().fg(Color::Yellow))
                    } else {
                        Span::raw("  ")
                    },
                );
                if item_index(&row.path())
                    .is_some_and(|(kind, i)| kind == "rules" && self.form.marked_rules.contains(&i))
                    || matches!(row, FlatRow::Node { index, .. }
                        if collection(doc, &[Part::Key("outbounds".into())])
                            .and_then(|nodes| nodes.get(*index))
                            .and_then(|node| node.get("tag"))
                            .and_then(Item::as_str)
                            .is_some_and(|tag| self.form.marked_nodes.iter().any(|marked| marked == tag)))
                {
                    spans.insert(0, Span::styled("✓ ", Style::default().fg(Color::Cyan)));
                }
                ListItem::new(Line::from(spans))
            })
            .collect();
        let items_empty = items.is_empty();
        let mut state = ListState::default();
        if !items_empty {
            let selected = self.form.selected.min(items.len() - 1);
            state = state
                .with_offset(selected.saturating_sub(area.height.saturating_sub(2) as usize / 3));
            state.select(Some(selected));
        }
        frame.render_stateful_widget(
            List::new(items)
                .highlight_style(crate::theme::selected_keep_colors())
                .block(Block::default().title(title).borders(Borders::ALL)),
            area,
            &mut state,
        );
        (state, label_column, items_empty)
    }

    fn render_footer(&self, frame: &mut Frame<'_>, area: Rect, rows: &[FlatRow]) {
        let dim = Style::default().fg(Color::DarkGray);
        let first = if let Mode::Search(input) = &self.form.mode {
            let mut text: Vec<char> = input.text.chars().collect();
            text.insert(input.cursor.min(text.len()), '▏');
            Line::from(vec![
                Span::styled(
                    format!(" / {}", text.iter().collect::<String>()),
                    Style::default().fg(Color::Cyan),
                ),
                Span::styled(tr!("   {}", t("Enter jump · Esc cancel")), dim),
            ])
        } else if let Some(notice) = &self.notice {
            Line::styled(format!(" {notice}"), Style::default().fg(Color::Green))
        } else {
            Line::styled(
                format!(" {}", self.hint(rows.get(self.form.selected))),
                Style::default().fg(Color::Cyan),
            )
        };
        let second = Line::styled(
            format!(
                " {}",
                t("/ search · u undo · Ctrl-Y redo · Ctrl-S save · ? all keys · Esc back")
            ),
            dim,
        );
        frame.render_widget(Paragraph::new(vec![first, second]), area);
    }

    fn render_popup(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        list_area: Rect,
        doc: &DocumentMut,
        list: &(ListState, usize, bool),
    ) {
        let (state, label_column, items_empty) = (&list.0, list.1, list.2);
        if !matches!(self.form.mode, Mode::Browse | Mode::Search(_)) {
            let height = match &self.form.mode {
                Mode::NewOutbound(_) => 15,
                Mode::Members { .. } => 15,
                Mode::Field { key, input, .. } => {
                    let choices = self
                        .suggestions(doc, key, input)
                        .map_or(0, |values| values.len().clamp(1, 6) as u16);
                    let help = self
                        .current_spec(doc, key)
                        .is_ok_and(|spec| !Self::generic_help(&spec.help));
                    choices + u16::from(help) + 4
                }
                _ => 7,
            };
            let width = area.width.saturating_sub(4).min(76);
            let mut popup = Rect {
                x: area.x + area.width.saturating_sub(width) / 2,
                y: area.y + area.height.saturating_sub(height) / 2,
                width,
                height: height.min(area.height),
            };
            // A field editor opens right under its row, so the label stays in sight.
            if matches!(self.form.mode, Mode::Field { .. }) && !items_empty {
                let row =
                    list_area.y + 1 + (self.form.selected.saturating_sub(state.offset())) as u16;
                let left = area.x + 3 + label_column as u16;
                let room = area.right().saturating_sub(left + 1);
                if room >= 32 {
                    popup.x = left;
                    popup.width = room.min(76);
                } else {
                    popup.x = area.right().saturating_sub(width + 1).max(area.x);
                }
                popup.y = if row + 1 + popup.height <= list_area.bottom() {
                    row + 1
                } else {
                    row.saturating_sub(popup.height).max(area.y)
                };
            }
            frame.render_widget(Clear, popup);
            match &self.form.mode {
                Mode::Members { options, chosen, cursor, find } => {
                    let mut state = ListState::default().with_selected(Some(*cursor));
                    let query = find.as_ref().map(|q| q.to_lowercase()).filter(|q| !q.is_empty());
                    let items = options.iter().map(|name| {
                        let item = ListItem::new(format!("[{}] {}", if chosen.contains(name) { "×" } else { " " }, name));
                        // While searching, members that do not match are dimmed rather than hidden.
                        if query.as_ref().is_some_and(|q| !name.to_lowercase().contains(q)) { item.style(Style::default().fg(Color::DarkGray)) } else { item }
                    });
                    let title = match find {
                        Some(query) => format!("{} · /{}▏", t("Group members"), query),
                        None => t("Group members").to_owned(),
                    };
                    let hint = if find.is_some() { t("Type to find · ↑↓ next match · Enter / Esc done") } else { t("j/k move · Space toggle · / find · Enter confirm · Esc cancel") };
                    frame.render_stateful_widget(List::new(items).highlight_symbol("› ").highlight_style(crate::theme::selected())
                        .block(Block::default().title(title).title_bottom(hint).borders(Borders::ALL)), popup, &mut state);
                }
                Mode::Field{key,input,reveal} => {
                    let spec = self.current_spec(doc,key).ok();
                    let secret = spec.as_ref().is_some_and(Spec::secret) && !reveal;
                    let display = if secret { "•".repeat(input.text.chars().count()) } else { input.text.replace('\n'," ↵ ") };
                    let flavor = spec.as_ref().map(|spec| Self::flavor(spec, table_at(doc.as_table(), &self.form.path))).unwrap_or(crate::highlight::Flavor::Auto);
                    let colored = if secret { vec![Span::raw(display.clone())] } else { crate::highlight::spans(&display, flavor) };
                    let mut first = vec![Span::raw("> ")];
                    first.extend(crate::highlight::with_cursor(colored, input.cursor.min(display.chars().count())));
                    let suggestions = self.suggestions(doc, key, input);
                    let choices = suggestions.as_ref().map(|values| {
                        if values.is_empty() { return t("No matches").to_owned(); }
                        let selected = input.candidate.as_ref().unwrap_or(&input.text);
                        let at = values.iter().position(|value| value == selected).unwrap_or(0);
                        let start = at.saturating_sub(2).min(values.len().saturating_sub(6));
                        values.iter().skip(start).take(6).map(|value| format!("{} {}",if value == selected {"›"} else {" "},self.reference_label(doc,key,value))).collect::<Vec<_>>().join("\n")
                    }).unwrap_or_default();
                    let hint = if suggestions.is_some() { t("Type to filter · ↑↓ select · Tab complete · Enter confirm · Esc cancel") } else { t("Enter confirm · Esc cancel · Ctrl-U clear · F2 reveal") };
                    let help = spec.as_ref().map(|s| s.help.as_str()).filter(|help| !Self::generic_help(help)).unwrap_or("");
                    let mut lines = vec![Line::from(first)];
                    lines.extend([choices, help.into(), hint.into()].into_iter().filter(|line| !line.is_empty()).flat_map(|text| text.lines().map(|line| Line::raw(line.to_owned())).collect::<Vec<_>>()));
                    let paragraph = Paragraph::new(lines).wrap(Wrap{trim:false});
                    // Wrapped help lines on narrow screens must not push the key hints out.
                    let mut popup = popup;
                    popup.height = (paragraph.line_count(popup.width.saturating_sub(2).max(1)) as u16 + 2).min(area.height);
                    popup.y = popup.y.min(area.bottom().saturating_sub(popup.height));
                    frame.render_widget(Clear, popup);
                    frame.render_widget(paragraph.block(Block::default().title(spec.as_ref().map(|s|s.label.as_str()).unwrap_or(key)).borders(Borders::ALL).style(Style::default().fg(Color::Cyan))),popup);
                }
                Mode::NewOutbound(selected) => {
                    let mut state = ListState::default().with_selected(Some(*selected));
                    frame.render_stateful_widget(List::new(OUTBOUND_KINDS.iter().map(|s|ListItem::new(*s))).highlight_symbol("› ").highlight_style(crate::theme::selected()).block(Block::default().title(t("新增出站节点 · ↑↓ 选择 · Enter 确认 · Esc 取消")).borders(Borders::ALL)),popup,&mut state);
                }
                Mode::Header(input) => frame.render_widget(Paragraph::new(if last_key(&self.form.path)==Some("subscriptions") {tr!("Subscription name: {}▏\nEnter confirm · Esc cancel", input.text)} else if last_key(&self.form.path)==Some("proxy_groups") {tr!("Proxy group name: {}▏\nEnter confirm · Esc cancel", input.text)} else if self.form.rename_rule_set.is_some() {tr!("Rename rule set: {}▏\nEnter confirm · Esc cancel",input.text)}else if last_key(&self.form.path)==Some("rule_sets") {tr!("规则集名称：{}▏\nEnter 创建 · Esc 取消",input.text)}else{tr!("请求头名称：{}▏\nEnter 继续设置值 · Esc 取消",input.text)}).block(Block::default().title(if last_key(&self.form.path)==Some("subscriptions") {t("Subscriptions")} else if last_key(&self.form.path)==Some("proxy_groups") {t("Proxy groups")}else if last_key(&self.form.path)==Some("rule_sets"){t("规则集")}else{t("新增请求头")}).borders(Borders::ALL)),popup),
                Mode::Delete{..} => frame.render_widget(Paragraph::new(t("删除所选条目/分组？\n只修改草稿，Ctrl-S 后写入配置。\ny 确认删除 · n / Esc 取消")).wrap(Wrap{trim:false}).block(Block::default().title(t("确认删除")).borders(Borders::ALL).style(Style::default().fg(Color::Yellow))),popup),
                Mode::Browse | Mode::Search(_) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests;
