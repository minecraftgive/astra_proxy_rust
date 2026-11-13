use crate::{i18n::t, tr};
use astra_app::edit::*;
use astra_app::schema::{Spec, Type, OUTBOUND_KINDS};
pub use astra_client::editor::*;
use astra_model::document::PathPart as Part;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};
use toml_edit::{DocumentMut, Item};
pub trait Render {
    fn render(&self, frame: &mut Frame<'_>, area: Rect);
    fn render_rows(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        doc: &DocumentMut,
        rows: &[FlatRow],
        title: Line<'_>,
    ) -> (ListState, usize, bool);
    fn render_footer(&self, frame: &mut Frame<'_>, area: Rect, rows: &[FlatRow]);
    fn render_popup(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        list_area: Rect,
        doc: &DocumentMut,
        list: &(ListState, usize, bool),
    );
}
impl Render for Editor {
    fn render(&self, frame: &mut Frame<'_>, area: Rect) {
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
                        let item = ListItem::new(format!("[{}] {}", chosen.iter().position(|v| v == name).map(|i| (i+1).to_string()).unwrap_or_else(|| " ".into()), name));
                        // While searching, members that do not match are dimmed rather than hidden.
                        if query.as_ref().is_some_and(|q| !name.to_lowercase().contains(q)) { item.style(Style::default().fg(Color::DarkGray)) } else { item }
                    });
                    let title = match find {
                        Some(query) => format!("{} · /{}▏", t("Group members"), query),
                        None => t("Group members").to_owned(),
                    };
                    let hint = if find.is_some() { t("Type to find · ↑↓ next match · Enter / Esc done") } else { t("j/k move · Shift+↑/↓ order · Space toggle · Enter confirm · Esc cancel") };
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
#[path = "test/editor_test.rs"]
mod tests;
