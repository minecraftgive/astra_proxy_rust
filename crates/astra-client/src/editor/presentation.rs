//! Renderer-independent editor labels, values, and form metadata.
use super::*;
use crate::highlight::{Flavor, Token, TokenKind, TokenStyle};

#[derive(Clone)]
pub struct TreeRow {
    pub index: usize,
    pub path: Path,
    pub title: String,
    pub depth: usize,
    pub expanded: Option<bool>,
    pub marked: bool,
    pub value: Vec<Token>,
}
#[derive(Clone)]
pub struct FieldPresentation {
    pub path: Path,
    pub spec: Spec,
    pub value: String,
    pub suggestions: Vec<String>,
    pub members: bool,
    pub list: Vec<String>,
}
pub struct Presentation {
    pub tree: Vec<TreeRow>,
    pub fields: Vec<FieldPresentation>,
    pub dirty: bool,
    pub hint: String,
}
impl Editor {
    pub fn interface_names(&self) -> Result<Vec<String>> {
        let doc = self.document()?;
        let exclude = doc
            .get("tun")
            .and_then(Item::as_table_like)
            .and_then(|table| table.get("name"))
            .and_then(Item::as_str);
        crate::network_interfaces::names(exclude)
            .context(t("Could not list local network adapters"))
    }
    pub fn presentation(&self) -> Result<Presentation> {
        let doc = self.document()?;
        let rows = self.visible_rows(&doc);
        let selected = rows.get(self.form.selected);
        let path = selected
            .map(FlatRow::path)
            .unwrap_or_else(|| self.form.path.clone());
        let tree = rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let mut item = TreeRow {
                    index,
                    path: row.path(),
                    title: String::new(),
                    depth: Self::depth(row),
                    expanded: None,
                    marked: false,
                    value: vec![],
                };
                match row {
                    FlatRow::Field { path, spec } => {
                        item.title = spec.label.clone();
                        let table = table_at(doc.as_table(), path);
                        let value = table
                            .and_then(|table| table.get(&spec.key))
                            .map(scalar)
                            .unwrap_or_else(|| spec.default.clone());
                        item.value = if spec.secret() && !value.is_empty() {
                            vec![Token::raw("••••••••")]
                        } else {
                            crate::highlight::spans(
                                &value.replace('\n', " ↵ "),
                                Self::flavor(spec, table),
                            )
                        };
                    }
                    FlatRow::Node { index, label } => {
                        item.title = label.clone();
                        item.expanded = Some(self.form.expanded_nodes.contains(index));
                        item.marked = collection(&doc, &[Part::Key("outbounds".into())])
                            .and_then(|nodes| nodes.get(*index))
                            .and_then(|node| node.get("tag"))
                            .and_then(Item::as_str)
                            .is_some_and(|tag| {
                                self.form.marked_nodes.iter().any(|marked| marked == tag)
                            });
                    }
                    FlatRow::Rule { index, label } => {
                        item.title = label.clone();
                        item.marked = self.form.marked_rules.contains(index);
                    }
                    FlatRow::RuleGroup { start, label, .. } => {
                        item.title = label.clone();
                        item.expanded = Some(self.form.expanded_rule_groups.contains(start));
                    }
                    FlatRow::Collection { path, label } => {
                        item.title = label.clone();
                        if path.len() == 1
                            && matches!(
                                last_key(path),
                                Some("rules" | "proxy_groups" | "rule_sets")
                            )
                        {
                            item.expanded = Some(
                                self.form
                                    .expanded_collections
                                    .iter()
                                    .any(|value| Some(value.as_str()) == last_key(path)),
                            );
                        }
                    }
                    FlatRow::Section { path, label } => {
                        item.title = label.clone();
                        item.expanded = if let Some(name) = rule_set_name(path) {
                            Some(
                                self.form
                                    .expanded_rule_sets
                                    .iter()
                                    .any(|value| value == name),
                            )
                        } else {
                            proxy_group_name(path).map(|name| {
                                self.form
                                    .expanded_proxy_groups
                                    .iter()
                                    .any(|value| value == name)
                            })
                        };
                    }
                    FlatRow::Info { label, .. } => item.title = label.clone(),
                    FlatRow::UpdateSubscriptions => item.title = t("Update subscriptions").into(),
                    FlatRow::AddHeader { path } => {
                        item.title = t(if last_key(path) == Some("subscriptions") {
                            "Add subscription"
                        } else {
                            "Add header"
                        })
                        .into()
                    }
                }
                item
            })
            .collect();
        let fields = self
            .schema_rows(&doc, &path)
            .into_iter()
            .filter_map(|row| {
                let Row::Field(spec) = row else {
                    return None;
                };
                let item = table_at(doc.as_table(), &path).and_then(|table| table.get(&spec.key));
                let mut value = item.map(scalar).unwrap_or_else(|| spec.default.clone());
                if let Mode::Field { key, input, .. } = &self.form.mode {
                    if self.form.path == path && key == &spec.key {
                        value = input.text.clone();
                    }
                }
                let suggestions = self
                    .reference_values_at(&doc, &path, &spec.key)
                    .unwrap_or_else(|| match &spec.kind {
                        Type::Choice(values) => {
                            values.iter().map(|value| (*value).to_owned()).collect()
                        }
                        _ => vec![],
                    });
                let list = item
                    .and_then(Item::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| item.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default();
                let members = proxy_group_name(&path).is_some() && spec.key == "members";
                Some(FieldPresentation {
                    path: path.clone(),
                    spec,
                    value,
                    suggestions,
                    members,
                    list,
                })
            })
            .collect();
        Ok(Presentation {
            tree,
            fields,
            dirty: self.source_buffer.is_some() || self.text() != self.original,
            hint: self.hint(selected),
        })
    }
    pub fn set_list_values(&mut self, path: Path, key: String, values: Vec<String>) -> Result<()> {
        let values: toml_edit::Array = values.iter().map(String::as_str).collect();
        self.set_field(path, key, values.to_string())
    }
    /// Highlights text without validating it, so incomplete source stays editable.
    pub fn source_tokens(text: &str) -> Vec<Token> {
        text.split_inclusive('\n')
            .flat_map(|line| {
                let mut tokens = crate::highlight::spans(line, Flavor::Auto);
                if line.trim_start().starts_with('#') {
                    tokens = vec![Token::styled(
                        line,
                        TokenStyle::default().kind(TokenKind::Comment),
                    )];
                }
                tokens
            })
            .collect()
    }
}

#[derive(Clone, Debug)]
pub struct SourceError {
    pub message: String,
    pub span: Option<std::ops::Range<usize>>,
}
impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.message.fmt(f)
    }
}
impl std::error::Error for SourceError {}
impl Editor {
    pub fn set_source_buffer(&mut self, text: String) {
        self.source_buffer = (text != self.text()).then_some(text);
    }
    pub fn commit_source(&mut self) -> std::result::Result<(), SourceError> {
        let Some(text) = self.source_buffer.clone() else {
            return Ok(());
        };
        self.import(text).map_err(|error| SourceError {
            span: error
                .downcast_ref::<toml_edit::TomlError>()
                .and_then(|error| error.span()),
            message: format!("{error:#}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_source_survives_serialization_and_prevents_save_until_corrected() {
        let mut editor = Editor::new("rule".into(), "default_action = 'direct'\n".into());
        editor.set_source_buffer("default_action = [".into());
        assert!(editor.commit_source().is_err());
        assert!(!editor.can_close());
        assert!(editor.prepare_save().is_err());
        let mut restored: Editor =
            serde_json::from_str(&serde_json::to_string(&editor).unwrap()).unwrap();
        assert_eq!(
            restored.source_buffer.as_deref(),
            Some("default_action = [")
        );
        restored.set_source_buffer("default_action = 'block'\n".into());
        restored.commit_source().unwrap();
        assert!(restored.source_buffer.is_none());
        assert!(restored.text().contains("block"));
        restored.restore(false).unwrap();
        assert!(restored.text().contains("direct"));
    }
    #[test]
    fn source_highlighting_preserves_every_unicode_byte_and_line_ending() {
        let text = "# 配置\nlisten = '127.0.0.1:7890'\nnames = ['日本🚀', 'b']\n";
        let result = Editor::source_tokens(text)
            .into_iter()
            .map(|token| token.content)
            .collect::<String>();
        assert_eq!(result, text);
    }
    #[test]
    fn native_field_edit_validates_the_supplied_path_and_retains_invalid_input() {
        let mut editor = Editor::new("proxy".into(), "standalone=true\n[tun]\nmtu=1500\n".into())
            .with_kind(CoreKind::Proxy);
        let path = vec![Part::Key("tun".into())];
        assert!(editor
            .set_field(path.clone(), "mtu".into(), "invalid".into())
            .is_err());
        assert!(matches!(&editor.form.mode, Mode::Field { input, .. } if input.text == "invalid"));
        assert_eq!(
            editor.document().unwrap()["tun"]["mtu"].as_integer(),
            Some(1500)
        );
        editor.set_field(path, "mtu".into(), "1400".into()).unwrap();
        assert_eq!(
            editor.document().unwrap()["tun"]["mtu"].as_integer(),
            Some(1400)
        );
        editor.restore(false).unwrap();
        assert_eq!(
            editor.document().unwrap()["tun"]["mtu"].as_integer(),
            Some(1500)
        );
    }
    #[test]
    fn semantic_drag_preserves_fold_identity_and_is_undoable() {
        let source = "standalone=true\n[[outbounds]]\ntag='first'\nkind='socks5'\nserver='127.0.0.1:1080'\n[[outbounds]]\ntag='second'\nkind='http'\nserver='127.0.0.1:8080'\n[[outbounds]]\ntag='third'\nkind='http'\nserver='127.0.0.1:8081'\n";
        let mut editor = Editor::new("proxy".into(), source.into()).with_kind(CoreKind::Proxy);
        editor.form.expanded_nodes.push(2);
        let rows = editor.visible_rows(&editor.document().unwrap());
        let from = rows
            .iter()
            .position(|row| matches!(row, FlatRow::Node { index: 2, .. }))
            .unwrap();
        let to = rows
            .iter()
            .position(|row| matches!(row, FlatRow::Node { index: 0, .. }))
            .unwrap();
        editor.select_row(from).unwrap();
        editor.move_to_row(to).unwrap();
        assert_eq!(
            editor.document().unwrap()["outbounds"][0]["tag"].as_str(),
            Some("third")
        );
        assert_eq!(editor.form.expanded_nodes, vec![0]);
        editor.restore(false).unwrap();
        assert_eq!(editor.text(), source);
    }
}
