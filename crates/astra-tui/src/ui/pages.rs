//! Rules, Logs and Profiles pages.
use super::dashboard::{card, Target, BLUE, MUTED};
use super::shell::duration;
use super::*;
use ratatui::{layout::Rect, style::Modifier};

impl Ui {
    pub(super) fn render_rules(&self, f: &mut Frame<'_>, area: Rect) {
        let Some(proxy) = self.active_proxy() else {
            f.render_widget(
                Paragraph::new(t("No profile yet · 6 then n to create one"))
                    .style(Style::default().fg(MUTED))
                    .block(card("Rules")),
                area,
            );
            return;
        };
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let mut lines = vec![];
        match self.active_rule() {
            Some(rule) => {
                let details = self.details.get(&rule.instance.name);
                let users: Vec<_> = self
                    .rows
                    .iter()
                    .filter(|r| {
                        r.instance.kind == Kind::Proxy
                            && self
                                .details
                                .get(&r.instance.name)
                                .is_some_and(|d| d.bindings.contains(&rule.instance.name))
                    })
                    .map(|r| r.instance.name.as_str())
                    .collect();
                let running = rule.pid.is_some();
                lines.push(Line::from(vec![
                    Span::styled(
                        if running { " ● " } else { " ○ " },
                        Style::default().fg(if running { Color::Green } else { MUTED }),
                    ),
                    Span::styled(rule.instance.name.clone(), bold),
                    Span::styled(
                        format!(
                            "  ·  {}{}",
                            t(&rule.state),
                            if running {
                                format!(" {}", duration(rule.uptime))
                            } else {
                                String::new()
                            }
                        ),
                        Style::default().fg(MUTED),
                    ),
                ]));
                if let Some(d) = details {
                    lines.push(Line::raw(format!(
                        "   {}  ·  {} {}",
                        tr!("{} rules", d.rules),
                        t("Log level"),
                        d.log_level.as_str().to_uppercase()
                    )));
                }
                if users.len() > 1 {
                    lines.push(Line::styled(
                        tr!(
                            "   Shared by {}; edits apply to all of them",
                            users.join(", ")
                        ),
                        Style::default().fg(Color::Yellow),
                    ));
                }
                if rule.changed {
                    lines.push(Line::styled(
                        t("   Changed on disk but not applied · reload it in Profiles"),
                        Style::default().fg(Color::Yellow),
                    ));
                }
                lines.push(Line::raw(""));
                lines.push(Line::from(vec![
                    Span::styled(" t ", Style::default().fg(Color::White).bg(BLUE)),
                    Span::raw(format!(
                        " {}",
                        t("Test a route: host[:port] [tcp|udp] [process]")
                    )),
                ]));
                lines.push(Line::styled(
                    t("   Shows the matching rule, the expanded entry and the final action. Uses the applied rules; sends no traffic."),
                    Style::default().fg(MUTED),
                ));
                if !running {
                    lines.push(Line::styled(
                        t("   Route tests need the rule core running · c connects the profile and its rule core"),
                        Style::default().fg(Color::Yellow),
                    ));
                }
                lines.push(Line::raw(""));
                lines.push(Line::from(vec![
                    Span::styled(" / ", Style::default().fg(Color::White).bg(BLUE)),
                    Span::raw(format!(
                        " {}",
                        t("Search rules: opens the rule editor with search; Enter jumps to the next match")
                    )),
                ]));
                lines.push(Line::from(vec![
                    Span::styled(" e ", Style::default().fg(Color::White).bg(BLUE)),
                    Span::raw(format!(
                        " {}",
                        t("Edit rules (Ctrl-T inside the editor tests a route)")
                    )),
                ]));
                lines.push(Line::from(vec![
                    Span::styled(" 5 ", Style::default().fg(Color::White).bg(BLUE)),
                    Span::raw(format!(" {}", t("Logs · ] shows this rule core; set capture level ROUTE for each decision"))),
                ]));
            }
            None => {
                let standalone = self
                    .details
                    .get(&proxy.instance.name)
                    .is_some_and(|d| d.modes.iter().any(|m| m == "Standalone"));
                lines.push(Line::raw(if standalone {
                    tr!("{} runs in standalone mode: every connection uses one outbound and no rule core.", proxy.instance.name)
                } else {
                    tr!("{} has no rule core bound.", proxy.instance.name)
                }));
                lines.push(Line::styled(
                    t("6 Profiles → select the profile → b bind a rule core, or n to create one (Tab lists rule cores)."),
                    Style::default().fg(MUTED),
                ));
            }
        }
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(card("Rules · first match wins")),
            area,
        );
    }

    pub(super) fn render_logs_page(&self, f: &mut Frame<'_>, area: Rect) {
        let rows = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(area);
        let proxy = self.active_proxy().map(|r| r.instance.name.clone());
        let rule = self.active_rule().map(|r| r.instance.name.clone());
        let current = self.viewer.as_ref().map(|v| v.name.clone());
        let mut spans = vec![Span::styled(
            format!(" {} ", t("Source")),
            Style::default().fg(MUTED),
        )];
        for (key, label, name) in [("[", "Proxy core", &proxy), ("]", "Rule core", &rule)] {
            let Some(name) = name else { continue };
            let on = current.as_ref() == Some(name);
            spans.push(Span::styled(
                format!(" {key} {} · {name} ", t(label)),
                if on {
                    Style::default().bg(BLUE).fg(Color::White)
                } else {
                    Style::default().fg(MUTED)
                },
            ));
            spans.push(Span::raw(" "));
        }
        if let Some(name) = current
            .as_ref()
            .filter(|n| Some(*n) != proxy.as_ref() && Some(*n) != rule.as_ref())
        {
            spans.push(Span::styled(
                format!(" {name} "),
                Style::default().bg(BLUE).fg(Color::White),
            ));
        }
        spans.push(Span::styled(
            format!("  F7 {}", t("TUI log")),
            Style::default().fg(MUTED),
        ));
        f.render_widget(Paragraph::new(Line::from(spans)), rows[0]);
        if let Some(viewer) = &self.viewer {
            viewer.render(f, rows[1]);
        } else {
            f.render_widget(
                Paragraph::new(if proxy.is_some() {
                    t("Log closed · Enter or [ opens the proxy log, ] the rule core log")
                } else {
                    t("No profile yet · 6 then n to create one")
                })
                .style(Style::default().fg(MUTED))
                .block(card("Logs")),
                rows[1],
            );
        }
    }

    pub(super) fn render_profiles(&self, f: &mut Frame<'_>, area: Rect) {
        let active = self.active_proxy().map(|r| r.instance.name.clone());
        let items: Vec<ListItem> = self
            .rows()
            .iter()
            .map(|row| {
                let color = crate::theme::state_color(&row.state);
                let details = self.details.get(&row.instance.name);
                let modes = details
                    .map(|details| {
                        details
                            .modes
                            .iter()
                            .map(|mode| t(mode))
                            .collect::<Vec<_>>()
                            .join(" + ")
                    })
                    .unwrap_or_default();
                let first = Line::from(vec![
                    Span::styled(
                        if row.pid.is_some() { "● " } else { "○ " },
                        Style::default().fg(color),
                    ),
                    Span::styled(
                        format!("{}  ", row.instance.name),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        if active.as_deref() == Some(&row.instance.name) {
                            format!("{}  ", t("active"))
                        } else {
                            String::new()
                        },
                        Style::default().fg(BLUE),
                    ),
                    Span::styled(t(&row.state).to_owned(), Style::default().fg(color)),
                    Span::raw(if area.width >= 48 {
                        format!("  {modes}")
                    } else {
                        String::new()
                    }),
                    Span::styled(
                        if row.changed {
                            t(" · reload pending")
                        } else {
                            ""
                        },
                        Style::default().fg(Color::Yellow),
                    ),
                ]);
                let summary = if let Some(details) = details {
                    let mut summary = if details.config_unavailable {
                        t("Configuration unavailable").to_owned()
                    } else {
                        details.listeners.join(" · ")
                    };
                    if !details.bindings.is_empty() {
                        summary.push_str(&format!("  → {}", details.bindings.join(", ")));
                    }
                    if summary.is_empty() {
                        summary = t("No listeners").into();
                    }
                    summary
                } else {
                    row.instance
                        .config
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned()
                };
                ListItem::new(Text::from(vec![
                    first,
                    Line::styled(format!("  {summary}"), Style::default().fg(MUTED)),
                ]))
            })
            .collect();
        let empty = items.is_empty();
        let mut state = ListState::default();
        if !empty {
            state.select(Some(self.selections[self.index()]));
        }
        let list = List::new(items)
            .block(card("").title(self.profiles_title()))
            .highlight_style(crate::theme::selected())
            .highlight_symbol("› ");
        f.render_stateful_widget(list, area, &mut state);
        self.list_hits(area, state.offset());
        if area.width >= 40 {
            self.hit(
                Rect::new(area.x + 1, area.y, area.width.saturating_sub(2), 1),
                Target::Core,
            );
        }
        if empty {
            f.render_widget(
                Paragraph::new(t("n to add an instance")),
                area.inner(ratatui::layout::Margin::new(2, 1)),
            );
        }
    }
}
