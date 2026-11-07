//! Semantic edits shared by keyboard and pointer-driven frontends.
use super::*;
impl Editor {
    pub fn rename_selected(&mut self) -> Result<()> {
        let doc = self.document()?;
        let selected = self.visible_rows(&doc).get(self.form.selected).cloned();

        if let Some(FlatRow::Node { index, .. }) = selected.as_ref() {
            return self.open_field(
                vec![Part::Key("outbounds".into()), Part::Index(*index)],
                "tag",
            );
        }
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

        Ok(())
    }
    pub fn toggle_rule_set(&mut self) -> Result<()> {
        let doc = self.document()?;
        let selected = self.visible_rows(&doc).get(self.form.selected).cloned();

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

        Ok(())
    }
    pub fn new_entry(&mut self) -> Result<()> {
        let doc = self.document()?;
        let selected = self.visible_rows(&doc).get(self.form.selected).cloned();

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

        Ok(())
    }
    pub fn delete_selected(&mut self) -> Result<()> {
        let doc = self.document()?;
        let selected = self.visible_rows(&doc).get(self.form.selected).cloned();

        match selected {
            Some(FlatRow::Field { path, spec, .. }) => {
                anyhow::ensure!(!spec.required, t("必填项不能删除，请按 Enter 修改"));
                // DIRECT/REJECT have no dynamic members to fall back on.
                anyhow::ensure!(
                    !(spec.key == "members"
                        && proxy_group_name(&path).is_some_and(astra_model::groups::builtin)),
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
            Some(FlatRow::Section { mut path, .. } | FlatRow::Collection { mut path, .. }) => {
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

        Ok(())
    }
    pub fn mark_selected(&mut self, all: bool) -> Result<()> {
        let doc = self.document()?;
        let selected = self.visible_rows(&doc).get(self.form.selected).cloned();
        let path = selected.as_ref().map(FlatRow::path).unwrap_or_default();
        if matches!(selected, Some(FlatRow::Node { .. })) {
            self.form.marked_rules.clear();
            if all {
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
            if all {
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
        Ok(())
    }
    pub fn move_selected(&mut self, down: bool) -> Result<()> {
        let doc = self.document()?;
        let selected = self.visible_rows(&doc).get(self.form.selected).cloned();
        let path = selected.as_ref().map(FlatRow::path).unwrap_or_default();
        let (kind, index) = item_index(&path).context(t("请先选择要调整顺序的规则或节点"))?;
        if kind == "rules" && !self.form.marked_rules.is_empty() {
            self.move_marked_rules(None, down)?;
            return Ok(());
        }
        let array_path = vec![Part::Key(kind.into())];
        let mut doc = self.document()?;
        let array = collection_mut(&mut doc, &array_path)?;
        let next = if !down {
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
        Ok(())
    }
    pub fn commit_header(&mut self) -> Result<()> {
        let Mode::Header(input) = &self.form.mode else {
            return Ok(());
        };
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
                previous.as_deref() == Some(&key) || !proxy_targets(&doc).contains(&key),
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
                !key.is_empty() && key.len() <= 128 && !key.chars().any(char::is_control),
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
                    .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)),
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
        Ok(())
    }
    pub fn create_outbound(&mut self, selected: usize) -> Result<()> {
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
        ][selected.min(OUTBOUND_KINDS.len() - 1)];
        self.append(outbound_template(kind, "edge"))?;
        Ok(())
    }
    pub fn set_members(&mut self, chosen: Vec<String>) -> Result<()> {
        let dynamic = table_at(self.document()?.as_table(), &self.form.path).is_some_and(|group| {
            ["include_all", "include_all_groups"]
                .iter()
                .any(|key| group.get(key).and_then(Item::as_bool) == Some(true))
        });
        anyhow::ensure!(
            !chosen.is_empty() || dynamic,
            t("Choose at least one group member")
        );

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
            if effective
                .selected
                .as_ref()
                .is_some_and(|s| astra_model::groups::member_path(&groups, name, s).is_none())
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
        Ok(())
    }
}

impl Editor {
    pub fn select_row(&mut self, index: usize) -> Result<()> {
        let doc = self.document()?;
        let rows = self.visible_rows(&doc);
        self.form.selected = index.min(rows.len().saturating_sub(1));
        if let Some(row) = rows.get(self.form.selected) {
            self.form.path = row.path();
        }
        Ok(())
    }
    pub fn collapse_all(&mut self) {
        self.form.expanded_collections.clear();
        self.form.expanded_rule_groups.clear();
        self.form.expanded_rule_sets.clear();
        self.form.expanded_proxy_groups.clear();
        self.form.expanded_nodes.clear();
        self.form.selected = 0;
    }
    pub fn collapse_current(&mut self) -> Result<()> {
        let doc = self.document()?;
        if let Some(row) = self.visible_rows(&doc).get(self.form.selected) {
            self.collapse_selection(&doc, row);
        }
        Ok(())
    }
    pub fn open_field(&mut self, path: Path, key: &str) -> Result<()> {
        let doc = self.document()?;
        self.form.path = path;
        self.focus_field(&doc, &self.form.path.clone(), key);
        self.enter(false)
    }
    pub fn edit_field(&mut self, path: Path, key: String, text: String) {
        self.form.path = path;
        self.form.mode = Mode::Field {
            key,
            input: TextInput::new(text),
            reveal: false,
        };
    }
    pub fn set_field(&mut self, path: Path, key: String, text: String) -> Result<()> {
        self.form.path = path;
        self.form.mode = Mode::Field {
            key,
            input: TextInput::new(text),
            reveal: false,
        };
        self.commit_field()
    }
    /// Preserve incomplete input in the shared draft without committing a field.
    pub fn replace_input(&mut self, text: String) {
        match &mut self.form.mode {
            Mode::Field { input, .. } | Mode::Header(input) | Mode::Search(input) => {
                *input = TextInput::new(text)
            }
            _ => {}
        }
    }
    pub fn commit_input(&mut self) -> Result<()> {
        match self.form.mode.clone() {
            Mode::Field { .. } => {
                self.commit_field()?;
                self.guide_next()
            }
            Mode::Header(_) => self.commit_header(),
            Mode::Search(input) => {
                self.cancel_input();
                self.form.last_search = input.text.clone();
                self.search(&input.text)
            }
            Mode::Members { chosen, .. } => self.set_members(chosen),
            Mode::NewOutbound(index) => self.create_outbound(index),
            Mode::Delete { .. } => self.confirm_delete(),
            Mode::Browse => Ok(()),
        }
    }
    pub fn toggle_member(&mut self, index: usize) {
        if let Mode::Members {
            options,
            chosen,
            cursor,
            ..
        } = &mut self.form.mode
        {
            if let Some(name) = options.get(index) {
                *cursor = index;
                if chosen.contains(name) {
                    chosen.retain(|value| value != name);
                } else {
                    chosen.push(name.clone());
                }
            }
        }
    }
    pub fn choose_outbound(&mut self, index: usize) {
        if let Mode::NewOutbound(selected) = &mut self.form.mode {
            *selected = index.min(OUTBOUND_KINDS.len() - 1);
        }
    }
    /// Reorder a dragged node/rule before another visible row of the same collection.
    pub fn move_to_row(&mut self, target: usize) -> Result<()> {
        let mut doc = self.document()?;
        let rows = self.visible_rows(&doc);
        let source = rows
            .get(self.form.selected)
            .context(t("Select a node or rule to duplicate"))?
            .path();
        let destination = rows
            .get(target)
            .context(t("Select a node or rule to duplicate"))?
            .path();
        let (kind, index) = item_index(&source).context(t("请先选择要调整顺序的规则或节点"))?;
        let (target_kind, target) =
            item_index(&destination).context(t("请先选择要调整顺序的规则或节点"))?;
        anyhow::ensure!(kind == target_kind, t("Move within the same collection"));
        if kind == "rules" && !self.form.marked_rules.is_empty() {
            return self.move_marked_rules(Some(target), false);
        }
        let array_path = vec![Part::Key(kind.into())];
        let array = collection_mut(&mut doc, &array_path)?;
        let mut order: Vec<usize> = (0..array.len()).collect();
        let moved = order.remove(index);
        let target = if index < target { target - 1 } else { target };
        order.insert(target, moved);
        let tables: Vec<Table> = array.iter().cloned().collect();
        *array = order.iter().map(|index| tables[*index].clone()).collect();
        if kind == "outbounds" {
            for expanded in &mut self.form.expanded_nodes {
                if let Some(index) = order.iter().position(|old| old == expanded) {
                    *expanded = index;
                }
            }
        }
        self.update(doc);
        self.focus_item(&self.document()?, kind, target);
        Ok(())
    }
    pub fn key_with_vim(&mut self, key: KeyEvent, vim: bool) -> Result<()> {
        if !vim
            && !key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
            && !key.modifiers.contains(KeyModifiers::SUPER)
            && !self.accepts_text_input()
            && matches!(
                key.code,
                KeyCode::Char('h' | 'j' | 'k' | 'l' | 'g' | 'G' | 'z' | 'J' | 'K')
            )
        {
            self.form.vim_g = false;
            self.form.vim_z = false;
            return Ok(());
        }
        self.key(key)
    }
}
