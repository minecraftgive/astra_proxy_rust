use crate::{
    edit::*,
    history,
    schema::{self, Row, Type},
};
use anyhow::{ensure, Context, Result};
use astra_model::{
    document::{EditCommand, Field, FieldType, PathPart as Part, Platform, Schema, SchemaRow},
    CoreKind,
};
use toml_edit::{value, DocumentMut, Item, Table};

pub const DOCUMENT_LIMIT: usize = 1024 * 1024;
const HISTORY_LIMIT: usize = 200;

/// A pure transactional draft. Parsing and failed commands never rewrite source.
/// Host adapters validate runtime requirements; incomplete drafts remain editable.
pub struct Document {
    kind: CoreKind,
    text: String,
    history: Vec<String>,
    future: Vec<String>,
}
impl Document {
    pub fn parse(kind: CoreKind, text: &str) -> Result<Self> {
        parse(text)?;
        Ok(Self {
            kind,
            text: text.into(),
            history: vec![],
            future: vec![],
        })
    }
    pub fn text(&self) -> &str {
        &self.text
    }
    pub fn kind(&self) -> CoreKind {
        self.kind
    }
    /// Phase two after parsing: pure schema/reference validation. Certificates,
    /// downloaded rule data, listeners, and platform capabilities are prepared by cores.
    pub fn validate_structure(&self) -> Result<()> {
        match self.kind {
            CoreKind::Proxy => {
                let config: astra_model::config::proxy::ProxyConfig = toml::from_str(&self.text)?;
                config.validate_structure()?;
            }
            CoreKind::Rule => {
                let config: astra_model::config::rule::RuleConfig = toml::from_str(&self.text)?;
                config.validate_structure()?;
            }
            CoreKind::Server => {
                let config: astra_model::config::server::Config = toml::from_str(&self.text)?;
                config.validate_structure()?;
            }
        }
        Ok(())
    }
    pub fn template(kind: CoreKind) -> String {
        match kind {
            CoreKind::Proxy => "standalone = true\nstandalone_outbound = \"node\"\nsocks5_listen = \"127.0.0.1:1080\"\n\n[[outbounds]]\ntag = \"node\"\nkind = \"socks5\"\nserver = \"127.0.0.1:7890\"\n",
            CoreKind::Rule => "socket = \"/tmp/astra-rule.sock\"\ndefault_action = \"direct\"\nrules = []\n",
            CoreKind::Server => "max_connections = 1024\nmax_udp_flows = 512\nallow_private_targets = false\n\n[[listeners]]\ntag = \"socks\"\nprotocol = \"socks5\"\nlisten = \"127.0.0.1:1080\"\n[[listeners.users]]\nname = \"user\"\npassword = \"change-me\"\n",
        }.into()
    }
    pub fn undo(&mut self) -> bool {
        self.restore(false)
    }
    pub fn redo(&mut self) -> bool {
        self.restore(true)
    }
    fn restore(&mut self, redo: bool) -> bool {
        let Some(text) =
            history::restore(&mut self.history, &mut self.future, self.text.clone(), redo)
        else {
            return false;
        };
        self.text = text;
        true
    }
    pub fn schema(&self, path: &[Part], platform: Platform) -> Result<Schema> {
        let doc = self.editable()?;
        let rows = schema::rows_for(&doc, path, self.kind, platform, |s| s)
            .into_iter()
            .map(|row| match row {
                Row::Field(spec) => {
                    let secret = spec.secret();
                    SchemaRow::Field {
                        field: Field {
                            value: table_at(doc.as_table(), path)
                                .and_then(|table| table.get(&spec.key))
                                .map(scalar),
                            key: spec.key,
                            label: spec.label,
                            default: spec.default,
                            required: spec.required,
                            help: spec.help,
                            secret,
                            field_type: match spec.kind {
                                Type::Text => FieldType::Text,
                                Type::Number(min, max) => FieldType::Number { min, max },
                                Type::Choice(values) => FieldType::Choice {
                                    values: values.iter().map(|s| (*s).into()).collect(),
                                },
                                Type::Boolean => FieldType::Boolean,
                                Type::StringList => FieldType::StringList,
                            },
                        },
                    }
                }
                Row::Group { key, label } => SchemaRow::Group { key, label },
                Row::Entry { index } => SchemaRow::Entry { index },
            })
            .collect();
        Ok(Schema {
            core: self.kind,
            path: path.to_vec(),
            collection: collection(&doc, path).is_some()
                || match (self.kind, path) {
                    (CoreKind::Proxy, [Part::Key(k)]) => k == "outbounds",
                    (CoreKind::Rule, [Part::Key(k)]) => k == "rules",
                    (CoreKind::Server, [Part::Key(k)]) => k == "listeners",
                    (CoreKind::Server, [Part::Key(k), Part::Index(_), Part::Key(child)]) => {
                        k == "listeners" && child == "users"
                    }
                    _ => false,
                },
            rows,
        })
    }
    fn editable(&self) -> Result<DocumentMut> {
        let mut doc = parse(&self.text)?;
        normalize_arrays(doc.as_table_mut())?;
        Ok(doc)
    }
    /// Apply one command atomically. Returns whether the text changed.
    pub fn apply(&mut self, command: EditCommand) -> Result<bool> {
        if let EditCommand::ReplaceText { text } = command {
            parse(&text)?;
            return Ok(self.commit(text));
        }
        let mut doc = parse(&self.text)?;
        // Normalize only the collection being edited. Unrelated inline arrays keep
        // their source representation, comments and unknown extension values.
        match &command {
            EditCommand::SetField { path, .. } | EditCommand::RemoveField { path, .. } => {
                normalize_path(doc.as_table_mut(), path, false)?
            }
            EditCommand::AppendEntry { path }
            | EditCommand::RemoveEntry { path, .. }
            | EditCommand::MoveEntry { path, .. } => {
                normalize_path(doc.as_table_mut(), path, true)?
            }
            EditCommand::AppendOutbound { .. } | EditCommand::RenameProxy { .. } => {
                normalize_path(doc.as_table_mut(), &[Part::Key("outbounds".into())], true)?
            }
            EditCommand::AppendRule { .. } | EditCommand::RenameRuleSet { .. } => {
                normalize_path(doc.as_table_mut(), &[Part::Key("rules".into())], true)?
            }
            EditCommand::ReplaceText { .. } => {}
        }
        match command {
            EditCommand::SetField { path, key, input } => {
                ensure!(!key.is_empty(), "field key is empty");
                let spec = schema::rows_for(&doc, &path, self.kind, Platform::current(), |s| s)
                    .into_iter()
                    .find_map(|r| match r {
                        Row::Field(s) if s.key == key => Some(s),
                        _ => None,
                    });
                let parsed = match spec.as_ref().map(|s| &s.kind) {
                    Some(Type::Number(min, max)) => {
                        let v: i64 = input.parse().context("expected an integer")?;
                        ensure!(
                            (*min..=*max).contains(&v),
                            "integer is outside allowed range"
                        );
                        value(v)
                    }
                    Some(Type::Boolean) => {
                        value(input.parse::<bool>().context("expected true or false")?)
                    }
                    Some(Type::Choice(choices)) => {
                        ensure!(choices.contains(&input.as_str()), "unsupported field value");
                        value(input)
                    }
                    Some(Type::StringList) => {
                        let parsed: DocumentMut = format!("value = {input}")
                            .parse()
                            .context("expected a TOML string array")?;
                        let array = parsed["value"].as_array().context("expected an array")?;
                        ensure!(
                            array.iter().all(|v| v.as_str().is_some()),
                            "expected only strings"
                        );
                        value(array.clone())
                    }
                    _ => {
                        ensure!(
                            !spec.as_ref().is_some_and(|s| s.required) || !input.is_empty(),
                            "required field is empty"
                        );
                        value(input)
                    }
                };
                let table = table_at_mut(doc.as_table_mut(), &path)?;
                ensure!(
                    !table
                        .get(&key)
                        .is_some_and(|i| i.is_table() || i.is_array_of_tables()),
                    "cannot replace a group with a field"
                );
                replace_field(table, &key, parsed);
            }
            EditCommand::RemoveField { path, key } => {
                ensure!(
                    !schema::rows_for(&doc, &path, self.kind, Platform::current(), |s| s)
                        .iter()
                        .any(|r| matches!(r, Row::Field(s) if s.key == key && s.required)),
                    "required field cannot be removed"
                );
                table_at_mut(doc.as_table_mut(), &path)?.remove(&key);
            }
            EditCommand::RenameProxy { previous, next } => {
                ensure!(
                    self.kind == CoreKind::Proxy,
                    "proxy rename requires a proxy document"
                );
                ensure!(
                    !astra_model::groups::reserved(&previous)
                        && !astra_model::groups::reserved(&next),
                    "reserved proxy name"
                );
                ensure!(
                    !next.trim().is_empty() && !proxy_targets(&doc).contains(&next),
                    "proxy name is empty or already exists"
                );
                let mut renamed = false;
                if let Some(nodes) = doc
                    .get_mut("outbounds")
                    .and_then(Item::as_array_of_tables_mut)
                {
                    for node in nodes.iter_mut() {
                        if node.get("tag").and_then(Item::as_str) == Some(previous.as_str()) {
                            replace_field(node, "tag", value(&next));
                            renamed = true;
                        }
                    }
                }
                if let Some(groups) = doc
                    .get_mut("proxy_groups")
                    .and_then(Item::as_table_like_mut)
                {
                    if let Some(group) = groups.remove(&previous) {
                        groups.insert(&next, group);
                        renamed = true;
                    }
                }
                ensure!(renamed, "proxy name does not exist");
                rename_proxy_references(&mut doc, &previous, &next);
            }
            EditCommand::RenameRuleSet { previous, next } => {
                ensure!(
                    self.kind == CoreKind::Rule,
                    "rule set rename requires a rule document"
                );
                ensure!(!next.trim().is_empty(), "rule set name is empty");
                let sets = doc
                    .get_mut("rule_sets")
                    .and_then(Item::as_table_like_mut)
                    .context("no rule sets")?;
                ensure!(!sets.contains_key(&next), "rule set already exists");
                let set = sets.remove(&previous).context("rule set does not exist")?;
                sets.insert(&next, set);
                if let Some(rules) = doc.get_mut("rules").and_then(Item::as_array_of_tables_mut) {
                    for rule in rules.iter_mut() {
                        if rule.get("kind").and_then(Item::as_str) == Some("rule_set")
                            && rule.get("value").and_then(Item::as_str) == Some(previous.as_str())
                        {
                            replace_field(rule, "value", value(&next));
                        }
                    }
                }
            }
            EditCommand::AppendOutbound { kind, tag } => {
                ensure!(
                    self.kind == CoreKind::Proxy,
                    "outbounds require a proxy document"
                );
                ensure!(
                    !tag.trim().is_empty() && !proxy_targets(&doc).contains(&tag),
                    "duplicate or empty outbound tag"
                );
                let normalized = match kind.as_str() {
                    "SOCKS5" => "socks5",
                    "HTTP CONNECT" => "http",
                    "VLESS" => "native:vless",
                    "VMess" => "native:vmess",
                    "Shadowsocks" => "native:shadowsocks",
                    "Trojan" => "native:trojan",
                    "Hysteria2" => "native:hysteria2",
                    "TUIC" => "native:tuic",
                    "Trojan TLS (legacy)" => "trojan_tls",
                    "VLESS TLS (legacy)" => "vless_tls",
                    v => v,
                };
                ensure!(
                    matches!(
                        normalized,
                        "socks5"
                            | "http"
                            | "native:vless"
                            | "native:vmess"
                            | "native:shadowsocks"
                            | "native:trojan"
                            | "native:hysteria2"
                            | "native:tuic"
                            | "trojan_tls"
                            | "vless_tls"
                    ),
                    "unsupported outbound kind"
                );
                collection_mut(&mut doc, &[Part::Key("outbounds".into())])?
                    .push(outbound_template(normalized, &tag));
            }
            EditCommand::AppendEntry { path } => {
                let mut table = Table::new();
                let index = collection(&doc, &path).map_or(0, |a| a.len());
                match (self.kind, last_key(&path)) {
                    (CoreKind::Server, Some("listeners")) => {
                        table["tag"] = value(format!("listener-{}", index + 1));
                        table["protocol"] = value("socks5");
                        table["listen"] = value("127.0.0.1:1080");
                    }
                    (CoreKind::Server, Some("users")) => {
                        table["name"] = value(format!("user-{}", index + 1));
                        table["password"] = value("change-me");
                    }
                    (CoreKind::Proxy, Some("outbounds")) => {
                        table = outbound_template("socks5", &format!("node-{}", index + 1));
                    }
                    (CoreKind::Rule, Some("rules")) => {
                        table["kind"] = value("domain_suffix");
                        table["value"] = value("example.com");
                        table["action"] = value("direct");
                    }
                    _ => anyhow::bail!("unsupported entry collection"),
                }
                collection_mut(&mut doc, &path)?.push(table);
            }
            EditCommand::AppendRule {
                kind,
                value: input,
                action,
            } => {
                ensure!(self.kind == CoreKind::Rule, "rules require a rule document");
                ensure!(
                    schema::RULE_KINDS.contains(&kind.as_str()),
                    "unsupported rule kind"
                );
                let mut rule = Table::new();
                rule["kind"] = value(kind);
                rule["value"] = value(input);
                rule["action"] = value(action);
                collection_mut(&mut doc, &[Part::Key("rules".into())])?.push(rule);
            }
            EditCommand::RemoveEntry { path, index } => {
                if last_key(&path) == Some("outbounds") {
                    if let Some(tag) = collection(&doc, &path)
                        .and_then(|a| a.get(index))
                        .and_then(|n| n.get("tag"))
                        .and_then(Item::as_str)
                    {
                        ensure!(!proxy_references(&doc, tag), "outbound is still referenced");
                    }
                }
                let items = collection_mut(&mut doc, &path)?;
                ensure!(index < items.len(), "entry index out of range");
                items.remove(index);
            }
            EditCommand::MoveEntry { path, from, to } => {
                let items = collection_mut(&mut doc, &path)?;
                ensure!(
                    from < items.len() && to < items.len(),
                    "entry index out of range"
                );
                let mut ordered: Vec<Table> = items.iter().cloned().collect();
                let item = ordered.remove(from);
                ordered.insert(to, item);
                items.clear();
                for item in ordered {
                    items.push(item);
                }
                position_tables(doc.as_table_mut(), &mut 0);
            }
            EditCommand::ReplaceText { .. } => unreachable!(),
        }
        let text = doc.to_string();
        parse(&text)?;
        Ok(self.commit(text))
    }
    fn commit(&mut self, text: String) -> bool {
        if text == self.text {
            return false;
        }
        let previous = std::mem::replace(&mut self.text, text);
        history::remember(&mut self.history, &mut self.future, previous, HISTORY_LIMIT);
        true
    }
}
fn parse(text: &str) -> Result<DocumentMut> {
    ensure!(text.len() <= DOCUMENT_LIMIT, "configuration exceeds 1 MiB");
    text.parse().context("invalid TOML configuration")
}
fn normalize_arrays(table: &mut Table) -> Result<()> {
    for (key, item) in table.iter_mut() {
        if let Some(array) = item.as_array().filter(|a| {
            (!a.is_empty() && a.iter().all(|v| v.as_inline_table().is_some()))
                || (a.is_empty()
                    && matches!(key.get(), "rules" | "outbounds" | "listeners" | "users"))
        }) {
            let mut tables = toml_edit::ArrayOfTables::new();
            for value in array.iter() {
                tables.push(value.as_inline_table().unwrap().clone().into_table());
            }
            *item = Item::ArrayOfTables(tables);
        }
        if let Some(child) = item.as_table_mut() {
            normalize_arrays(child)?;
        }
        if let Some(children) = item.as_array_of_tables_mut() {
            for child in children.iter_mut() {
                normalize_arrays(child)?;
            }
        }
    }
    Ok(())
}

fn normalize_path(
    table: &mut dyn toml_edit::TableLike,
    path: &[Part],
    leaf_collection: bool,
) -> Result<()> {
    let Some(Part::Key(key)) = path.first() else {
        return Ok(());
    };
    let Some(item) = table.get_mut(key) else {
        return Ok(());
    };
    let indexed = matches!(path.get(1), Some(Part::Index(_)));
    if indexed || (path.len() == 1 && leaf_collection) {
        if let Some(array) = item.as_array() {
            let mut tables = toml_edit::ArrayOfTables::new();
            for value in array {
                tables.push(
                    value
                        .as_inline_table()
                        .context("collection requires table entries")?
                        .clone()
                        .into_table(),
                );
            }
            *item = Item::ArrayOfTables(tables);
        }
    }
    if let Some(Part::Index(index)) = path.get(1) {
        if let Some(child) = item
            .as_array_of_tables_mut()
            .and_then(|a| a.get_mut(*index))
        {
            normalize_path(child, &path[2..], leaf_collection)?;
        }
    } else if let Some(child) = item.as_table_like_mut() {
        normalize_path(child, &path[1..], leaf_collection)?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "test/document_test.rs"]
mod tests;
