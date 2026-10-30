//! Overview page and shared mouse hit testing. Never synthesize telemetry.
use super::shell::{duration, CONNECTIONS, LOGS, PAGES, PROXIES};
use super::*;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::Rect,
    style::Modifier,
    widgets::{BorderType, Sparkline},
};

pub(super) const BLUE: Color = Color::Rgb(79, 91, 213);
pub(super) const MUTED: Color = Color::Rgb(140, 147, 161);

#[derive(Clone)]
pub(super) enum Target {
    TaskDetails,
    Page(usize),
    Row(usize),
    Menu(usize),
    Binding(usize),
    Back,
    Core,
    Runtime(astra_ipc::runtime::Command),
    Group(usize),
    Member(usize),
    Connection(usize),
    Connect,
    Palette(usize),
    /// Make the n-th proxy instance the active profile.
    Activate(usize),
}

/// Panels use the terminal's own background so light and dark themes both read well.
pub(super) fn card(title: &str) -> Block<'_> {
    Block::default()
        .title(format!(" {} ", t(title)))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title_style(Style::default().fg(MUTED))
        .border_style(Style::default().fg(Color::DarkGray))
}

impl Ui {
    pub(super) fn overview_input(&self) -> bool {
        self.live_input() && self.runtime_picker.is_none() && self.palette.is_none()
    }

    pub(super) fn navigate(&mut self, page: usize) -> Input {
        let page = page.min(PAGES.len() - 1);
        if self.page == LOGS && page != LOGS {
            self.close_viewer();
        }
        self.page = page;
        self.connection_filtering = false;
        self.runtime_picker = None;
        if page == LOGS && self.viewer.is_none() {
            if let Some(name) = self.log_source(true) {
                return self.open_logs(name);
            }
        }
        Input::Idle
    }

    pub(super) fn hit(&self, rect: Rect, target: Target) {
        if rect.width > 0 && rect.height > 0 {
            self.hits.borrow_mut().push((rect, target));
        }
    }

    pub(super) fn button(&self, f: &mut Frame<'_>, area: Rect, label: &str, target: Target) {
        f.render_widget(
            Paragraph::new(format!(" {} ", t(label)))
                .style(Style::default().bg(BLUE).fg(Color::White)),
            area,
        );
        self.hit(area, target);
    }

    /// Problems that need the user, each naming the key that fixes it.
    fn notices(&self) -> Vec<(Color, String)> {
        let mut notices = Vec::new();
        let Some(row) = self.active_proxy() else {
            return notices;
        };
        if row.state.starts_with("Exited") || row.state == "Unresponsive" {
            notices.push((
                Color::Red,
                tr!(
                    "Core stopped unexpectedly ({}) · L logs · e edit · c retry",
                    t(&row.state)
                ),
            ));
        }
        if row.changed {
            notices.push((
                Color::Yellow,
                t("Configuration changed but not applied · r apply").into(),
            ));
        }
        if let Some(rule) = self.active_rule() {
            if rule.changed {
                notices.push((
                    Color::Yellow,
                    tr!(
                        "Rule core {} has unapplied changes · reload it in Profiles",
                        rule.instance.name
                    ),
                ));
            }
            if row.pid.is_some() && rule.pid.is_none() {
                notices.push((
                    Color::Yellow,
                    tr!(
                        "Rule core {} is not running; routed connections will fail",
                        rule.instance.name
                    ),
                ));
            }
        }
        if row.pid.is_some() && self.live().is_none() && !self.runtime_error.is_empty() {
            notices.push((Color::Yellow, self.runtime_error.clone()));
        }
        if self
            .details
            .get(&row.instance.name)
            .is_some_and(|d| d.config_unavailable)
        {
            notices.push((Color::Red, t("Configuration unavailable · e edit").into()));
        }
        notices
    }

    pub(super) fn render_overview(&self, f: &mut Frame<'_>, area: Rect) {
        let Some(row) = self.active_proxy() else {
            f.render_widget(
                Paragraph::new(t(
                    "No profile yet. Press 6 for Profiles, then n to create a proxy instance; : lists every command.",
                ))
                .wrap(Wrap { trim: false })
                .block(card("Overview")),
                area,
            );
            return;
        };
        let notices = self.notices();
        let bands = Layout::vertical([
            Constraint::Length(5),
            Constraint::Length(if notices.is_empty() {
                0
            } else {
                notices.len() as u16 + 2
            }),
            Constraint::Length(if self.live().is_some() { 3 } else { 0 }),
            Constraint::Min(0),
        ])
        .split(area);
        self.render_hero(f, bands[0], row);
        if !notices.is_empty() {
            let lines: Vec<_> = notices
                .into_iter()
                .map(|(color, text)| {
                    Line::from(vec![
                        Span::styled(" ▌ ", Style::default().fg(color)),
                        Span::raw(text),
                    ])
                })
                .collect();
            f.render_widget(
                Paragraph::new(lines)
                    .wrap(Wrap { trim: false })
                    .block(card("Attention")),
                bands[1],
            );
        }
        self.render_runtime_modes(f, bands[2]);
        self.render_overview_cards(f, bands[3], row);
    }

    /// Compact traffic and group summary on top; busiest connections and profiles below.
    /// Wide terminals use two columns, narrow ones stack the most useful cards.
    fn render_overview_cards(&self, f: &mut Frame<'_>, area: Rect, row: &Row) {
        if area.height < 3 {
            return;
        }
        let top_height = 7.min(area.height);
        let rows =
            Layout::vertical([Constraint::Length(top_height), Constraint::Min(0)]).split(area);
        if area.width >= 100 {
            let top = Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
                .split(rows[0]);
            self.render_traffic(f, top[0], row);
            self.render_group_summary(f, top[1]);
            if rows[1].height >= 4 {
                let bottom =
                    Layout::horizontal([Constraint::Percentage(62), Constraint::Percentage(38)])
                        .split(rows[1]);
                self.render_top_connections(f, bottom[0]);
                self.render_profile_switcher(f, bottom[1]);
            }
        } else {
            self.render_traffic(f, rows[0], row);
            if rows[1].height >= 4 {
                let bottom =
                    Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)])
                        .split(rows[1]);
                self.render_group_summary(f, bottom[0]);
                self.render_top_connections(f, bottom[1]);
            }
        }
    }

    fn render_group_summary(&self, f: &mut Frame<'_>, area: Rect) {
        let block = card("Proxy groups · 2");
        let inner = block.inner(area);
        f.render_widget(block, area);
        let Some(snapshot) = self.live() else {
            let outbounds = self
                .active_proxy()
                .and_then(|r| self.details.get(&r.instance.name))
                .map(|d| d.outbounds.join("  ·  "))
                .unwrap_or_default();
            f.render_widget(
                Paragraph::new(if outbounds.is_empty() {
                    t("Groups appear after connecting.").to_owned()
                } else {
                    outbounds
                })
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(MUTED)),
                inner.inner(ratatui::layout::Margin::new(1, 0)),
            );
            return;
        };
        let name_width = snapshot
            .groups
            .iter()
            .map(|g| Line::from(g.name.as_str()).width())
            .max()
            .unwrap_or(0)
            .min(18);
        for (index, group) in snapshot
            .groups
            .iter()
            .take(inner.height as usize)
            .enumerate()
        {
            let selected = group.selected.as_deref().unwrap_or("—");
            let member = group.members.iter().find(|m| m.name == selected);
            let rect = Rect::new(inner.x, inner.y + index as u16, inner.width, 1);
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(
                        format!(" {:<name_width$} ", group.name),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        if group.mode == "select" { "  " } else { "⟳ " },
                        Style::default().fg(MUTED),
                    ),
                    Span::raw(format!("→ {selected}  ")),
                    if group.testing {
                        Span::styled(t("Testing…").to_owned(), Style::default().fg(MUTED))
                    } else {
                        live::delay_span(
                            member.and_then(|m| m.delay_ms),
                            member.and_then(|m| m.alive),
                        )
                    },
                ])),
                rect,
            );
            self.hit(rect, Target::Group(index));
        }
        if snapshot.groups.len() > inner.height as usize && inner.height > 0 {
            let more = format!(" +{} ", snapshot.groups.len() - inner.height as usize + 1);
            let width = more.len() as u16;
            f.render_widget(
                Paragraph::new(more).style(Style::default().fg(MUTED)),
                Rect::new(
                    inner.right().saturating_sub(width),
                    inner.bottom() - 1,
                    width,
                    1,
                ),
            );
        }
    }

    fn render_top_connections(&self, f: &mut Frame<'_>, area: Rect) {
        let block = card("Busiest connections · 3");
        let inner = block.inner(area);
        f.render_widget(block, area);
        self.hit(area, Target::Page(CONNECTIONS));
        let Some(snapshot) = self.live() else {
            f.render_widget(
                Paragraph::new(t("Connections appear after connecting."))
                    .style(Style::default().fg(MUTED)),
                inner.inner(ratatui::layout::Margin::new(1, 0)),
            );
            return;
        };
        let mut connections: Vec<_> = snapshot.connections.iter().collect();
        connections.sort_by_key(|c| std::cmp::Reverse(c.upload + c.download));
        if connections.is_empty() {
            f.render_widget(
                Paragraph::new(t("No active connections")).style(Style::default().fg(MUTED)),
                inner.inner(ratatui::layout::Margin::new(1, 0)),
            );
            return;
        }
        // Fixed columns: outbound (15) and both byte counters (25) plus padding.
        let host_width = (inner.width as usize).saturating_sub(43).clamp(12, 48);
        let lines: Vec<_> = connections
            .iter()
            .take(inner.height as usize)
            .map(|c| {
                let mut target = format!("{}:{}", c.host, c.port);
                if Line::from(target.as_str()).width() > host_width {
                    target = target
                        .chars()
                        .take(host_width.saturating_sub(1))
                        .collect::<String>()
                        + "…";
                }
                Line::from(vec![
                    Span::raw(format!(" {target:<host_width$} ")),
                    Span::styled(format!("{:<14} ", c.outbound), Style::default().fg(BLUE)),
                    Span::styled(
                        format!(
                            "↓ {:>9}  ↑ {:>9}",
                            live::bytes(c.download),
                            live::bytes(c.upload)
                        ),
                        Style::default().fg(MUTED),
                    ),
                ])
            })
            .collect();
        f.render_widget(Paragraph::new(lines), inner);
    }

    fn render_profile_switcher(&self, f: &mut Frame<'_>, area: Rect) {
        let block = card("Profiles · p switch");
        let inner = block.inner(area);
        f.render_widget(block, area);
        let active = self.active_proxy().map(|r| r.instance.name.as_str());
        let proxies = self.rows.iter().filter(|r| r.instance.kind == Kind::Proxy);
        for (index, row) in proxies.take(inner.height as usize).enumerate() {
            let running = row.pid.is_some();
            let failed = row.state.starts_with("Exited") || row.state == "Unresponsive";
            let is_active = active == Some(row.instance.name.as_str());
            let rect = Rect::new(inner.x, inner.y + index as u16, inner.width, 1);
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(
                        if running || failed { " ● " } else { " ○ " },
                        Style::default().fg(if running {
                            Color::Green
                        } else if failed {
                            Color::Red
                        } else {
                            MUTED
                        }),
                    ),
                    Span::styled(
                        row.instance.name.clone(),
                        if is_active {
                            Style::default().fg(BLUE).add_modifier(Modifier::BOLD)
                        } else {
                            Style::default()
                        },
                    ),
                    Span::styled(
                        format!(
                            "  {}{}",
                            t(&row.state),
                            if running {
                                format!(" {}", duration(row.uptime))
                            } else {
                                String::new()
                            }
                        ),
                        Style::default().fg(MUTED),
                    ),
                ])),
                rect,
            );
            self.hit(rect, Target::Activate(index));
        }
    }

    fn render_hero(&self, f: &mut Frame<'_>, area: Rect, row: &Row) {
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let failed = row.state.starts_with("Exited") || row.state == "Unresponsive";
        let (dot, color, state) = if row.pid.is_some() {
            ("●", Color::Green, t("Profile connected"))
        } else if failed {
            ("●", Color::Red, t("Failed"))
        } else {
            ("○", MUTED, t("Not connected"))
        };
        let mut first = vec![
            Span::styled(format!(" {dot} "), Style::default().fg(color)),
            Span::styled(state, bold.fg(color)),
            Span::raw("  "),
            Span::styled(row.instance.name.clone(), bold),
        ];
        if row.pid.is_some() {
            first.push(Span::styled(
                format!("  ·  {} {}", t("Up"), duration(row.uptime)),
                Style::default().fg(MUTED),
            ));
        }
        let mut lines = vec![Line::from(first)];
        if let Some(snapshot) = self.live() {
            let (chain, delay, alive) = live::exit_chain(snapshot);
            let mut spans = vec![Span::styled(
                format!(" {}  ", t("Exit")),
                Style::default().fg(MUTED),
            )];
            for (index, hop) in chain.iter().enumerate() {
                if index > 0 {
                    spans.push(Span::styled(" ▸ ", Style::default().fg(Color::DarkGray)));
                }
                spans.push(if index + 1 == chain.len() {
                    Span::styled(hop.clone(), bold.fg(BLUE))
                } else {
                    Span::raw(hop.clone())
                });
            }
            spans.push(Span::raw("  "));
            spans.push(live::delay_span(delay, alive));
            lines.push(Line::from(spans));
        } else if row.pid.is_some() {
            let waiting = if self.runtime_error.is_empty() {
                t("Waiting for runtime data…")
            } else {
                &self.runtime_error
            };
            lines.push(Line::styled(
                format!(" {waiting}"),
                Style::default().fg(MUTED),
            ));
        } else if failed {
            lines.push(Line::styled(
                format!(" {}", t("L logs · e edit · c retry")),
                Style::default().fg(Color::Red),
            ));
        } else {
            lines.push(Line::styled(
                format!(
                    " {}",
                    t("c connect · the mode and takeover switches apply once connected")
                ),
                Style::default().fg(MUTED),
            ));
        }
        let mut meta = vec![];
        if let Some(d) = self.details.get(&row.instance.name) {
            if !d.listeners.is_empty() {
                meta.push(d.listeners.join(" · "));
            }
            if d.outbound_count > 0 {
                meta.push(tr!("{} outbounds", d.outbound_count));
            }
        }
        meta.push(match self.active_rule() {
            Some(rule) => tr!("Rule core {}", rule.instance.name),
            None => t("No rule core").into(),
        });
        lines.push(Line::styled(
            format!(" {}", meta.join("  ·  ")),
            Style::default().fg(MUTED),
        ));
        f.render_widget(Paragraph::new(lines).block(card("Profile")), area);
        if area.width >= 60 {
            let label = if row.pid.is_some() {
                "c Disconnect"
            } else {
                "c Connect"
            };
            let width = Line::from(t(label)).width() as u16 + 2;
            self.button(
                f,
                Rect::new(area.right() - width - 2, area.y + 1, width, 1),
                label,
                Target::Connect,
            );
        }
    }

    fn render_traffic(&self, f: &mut Frame<'_>, area: Rect, row: &Row) {
        let block = card("Traffic");
        let inner = block.inner(area);
        f.render_widget(block, area);
        let Some(snapshot) = self.live() else {
            let text = if row.pid.is_some() {
                t("Waiting for runtime data…")
            } else {
                t("Traffic appears after connecting.")
            };
            f.render_widget(
                Paragraph::new(format!(" {text}")).style(Style::default().fg(MUTED)),
                inner,
            );
            return;
        };
        let rows = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(inner);
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw(format!(" {} {}", t("Active"), snapshot.active)),
                Span::styled(
                    format!(
                        "   {} ↓ {} ↑ {}",
                        t("Total"),
                        live::bytes(snapshot.download),
                        live::bytes(snapshot.upload)
                    ),
                    Style::default().fg(MUTED),
                ),
            ])),
            rows[0],
        );
        if rows[1].height == 0 {
            return;
        }
        let lanes =
            Layout::vertical([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)]).split(rows[1]);
        for (index, lane, arrow, color) in
            [(1, lanes[0], "↓", BLUE), (0, lanes[1], "↑", Color::Magenta)]
        {
            if lane.height == 0 {
                continue;
            }
            let label = format!(" {arrow} {:>10}/s ", live::bytes(self.rates[index]));
            let label_width = Line::from(label.as_str()).width() as u16;
            f.render_widget(
                Paragraph::new(label)
                    .style(Style::default().fg(color).add_modifier(Modifier::BOLD)),
                Rect::new(
                    lane.x,
                    lane.y + lane.height - 1,
                    label_width.min(lane.width),
                    1,
                ),
            );
            // Newest samples sit at the right edge, like the GUI chart.
            let room = lane.width.saturating_sub(label_width + 1) as usize;
            let data: Vec<_> = self.history[index].iter().copied().collect();
            let data = &data[data.len().saturating_sub(room)..];
            let width = data.len() as u16;
            f.render_widget(
                Sparkline::default()
                    .data(data)
                    .style(Style::default().fg(color)),
                Rect::new(
                    lane.right().saturating_sub(width + 1),
                    lane.y,
                    width,
                    lane.height,
                ),
            );
        }
    }

    pub(super) fn list_hits(&self, area: Rect, offset: usize) {
        for visible in 0..area.height.saturating_sub(2) as usize / 2 {
            if offset + visible >= self.rows().len() {
                break;
            }
            self.hit(
                Rect::new(
                    area.x + 1,
                    area.y + 1 + visible as u16 * 2,
                    area.width.saturating_sub(2),
                    2,
                ),
                Target::Row(offset + visible),
            );
        }
    }

    pub(super) fn menu_hits(&self, area: Rect, offset: usize) {
        let count = self
            .binding_picker
            .as_ref()
            .map_or_else(|| self.operations().len(), |p| p.choices.len());
        for visible in 0..area.height.saturating_sub(2) as usize {
            let index = visible + offset;
            if index >= count {
                break;
            }
            self.hit(
                Rect::new(
                    area.x + 1,
                    area.y + 1 + visible as u16,
                    area.width.saturating_sub(2),
                    1,
                ),
                if self.binding_picker.is_some() {
                    Target::Binding(index)
                } else {
                    Target::Menu(index)
                },
            );
        }
    }

    pub(super) fn mouse(&mut self, mouse: MouseEvent) -> Input {
        if matches!(
            mouse.kind,
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
        ) {
            return self.key(event::KeyEvent::new(
                if mouse.kind == MouseEventKind::ScrollUp {
                    KeyCode::Up
                } else {
                    KeyCode::Down
                },
                KeyModifiers::NONE,
            ));
        }
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return Input::Idle;
        }
        if self.help_scroll.is_some()
            || self.prompt.is_some()
            || self.recovery.is_some()
            || self.diagnostics.is_some()
            || self.explanation.is_some()
        {
            return Input::Idle;
        }
        let target = self
            .hits
            .borrow()
            .iter()
            .rev()
            .find(|(rect, _)| rect.contains((mouse.column, mouse.row).into()))
            .map(|(_, target)| target.clone());
        if self.palette.is_some() && !matches!(target, Some(Target::Palette(_))) {
            return Input::Idle;
        }
        match target {
            Some(Target::TaskDetails) => {
                self.key(event::KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE))
            }
            Some(Target::Palette(index)) => self.palette_run(index),
            Some(Target::Connect) if self.can_switch_page() => self.toggle_active(),
            Some(Target::Runtime(command)) => self.runtime_action(command),
            Some(Target::Group(index)) => {
                if self.page != PROXIES {
                    self.navigate(PROXIES);
                }
                self.group_selection = index;
                self.open_runtime_group();
                Input::Idle
            }
            Some(Target::Activate(index)) if self.can_switch_page() => {
                self.selections[0] = index;
                self.kind = Kind::Proxy;
                Input::Idle
            }
            Some(Target::Member(index)) => self.choose_runtime_member(index),
            Some(Target::Connection(index)) => {
                self.connection_selection = index;
                Input::Idle
            }
            Some(Target::Page(page)) if self.can_switch_page() => self.navigate(page),
            Some(Target::Row(index)) => {
                let slot = self.index();
                self.selections[slot] = index;
                self.menu = Some(0);
                Input::Idle
            }
            Some(Target::Menu(index)) => {
                self.menu = None;
                if let Some(op) = self.operations().get(index).copied() {
                    self.operation(op)
                } else {
                    Input::Idle
                }
            }
            Some(Target::Binding(index)) => {
                if let Some(picker) = &mut self.binding_picker {
                    picker.selected = index;
                }
                self.key(event::KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE))
            }
            Some(Target::Back) => self.key(event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Some(Target::Core) => self.key(event::KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            _ => Input::Idle,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::shell::{OVERVIEW, PROFILES};
    use super::*;
    use ratatui::backend::TestBackend;

    fn sample() -> Ui {
        let mut ui = Ui::new(Session::default());
        ui.rows = (0..24)
            .map(|index| Row {
                instance: Instance {
                    name: format!("proxy-{index:02}"),
                    kind: Kind::Proxy,
                    config: format!("/profiles/proxy-{index:02}.toml").into(),
                },
                pid: Some(100 + index),
                state: "Running".into(),
                uptime: 3723,
                changed: false,
            })
            .collect();
        ui.details.insert(
            "proxy-00".into(),
            InstanceDetails {
                modes: vec!["Rules".into(), "System proxy".into()],
                listeners: vec!["SOCKS5 127.0.0.1:7890".into()],
                outbounds: vec!["Tokyo [select]".into(), "Singapore [url-test]".into()],
                outbound_count: 12,
                rules: 114,
                diagnostics_supported: true,
                ..InstanceDetails::default()
            },
        );
        ui
    }
    fn click(rect: Rect) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        }
    }
    fn render(ui: &Ui, width: u16, height: u16) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| ui.render(f)).unwrap();
        terminal
    }
    fn screen(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }
    #[test]
    fn mouse_navigation_actions_scrolling_and_modal_capture() {
        let mut ui = sample();
        render(&ui, 120, 36);
        let rect = ui
            .hits
            .borrow()
            .iter()
            .find(|(_, hit)| matches!(hit, Target::Page(PROFILES)))
            .unwrap()
            .0;
        ui.mouse(click(rect));
        assert_eq!(ui.page, PROFILES);
        render(&ui, 120, 36);
        let rect = ui
            .hits
            .borrow()
            .iter()
            .find(|(_, hit)| matches!(hit, Target::Row(1)))
            .unwrap()
            .0;
        ui.mouse(click(rect));
        assert_eq!(ui.selections[0], 1);
        assert_eq!(ui.menu, Some(0));
        render(&ui, 120, 36);
        assert!(ui
            .hits
            .borrow()
            .iter()
            .all(|(_, hit)| matches!(hit, Target::Menu(_))));
        let rect = ui.hits.borrow()[0].0;
        assert!(
            matches!(ui.mouse(click(rect)), Input::Run(Action::Stop(name), _) if name == "proxy-01")
        );
        ui.menu = None;
        for _ in 0..20 {
            ui.mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 50,
                row: 15,
                modifiers: KeyModifiers::NONE,
            });
        }
        assert_eq!(ui.selections[0], 21);
        render(&ui, 120, 36);
        assert!(ui
            .hits
            .borrow()
            .iter()
            .any(|(_, hit)| matches!(hit, Target::Row(21))));
        ui.prompt(Prompt::Shutdown);
        let before = ui.page;
        ui.mouse(click(Rect::new(1, 1, 1, 1)));
        assert_eq!(ui.page, before);
        assert!(ui.prompt.is_some());
    }
    #[test]
    fn overview_resizes_follows_active_profile_and_palette_runs_commands() {
        crate::i18n::with_language(crate::i18n::Language::English, || {
            let mut ui = sample();
            let text = screen(&render(&ui, 140, 40));
            for value in [
                "1 Overview",
                "6 Profiles",
                "Connected",
                "proxy-00",
                "SOCKS5 127.0.0.1:7890",
                "1h02m",
            ] {
                assert!(text.contains(value), "{value}");
            }
            for (width, height) in [(1, 1), (20, 6), (60, 20), (90, 24)] {
                render(&ui, width, height);
            }
            // c toggles the active profile from any page except Profiles.
            assert!(matches!(
                ui.key(event::KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)),
                Input::Run(Action::Stop(name), _) if name == "proxy-00"
            ));
            ui.key(event::KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE));
            for c in "node".chars() {
                ui.key(event::KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
            }
            render(&ui, 100, 30);
            assert!(matches!(
                ui.key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                Input::Run(Action::Diagnostics(name), _) if name == "proxy-00"
            ));
            assert!(ui.palette.is_none());
            // p lists profiles; choosing one makes it active everywhere.
            ui.key(event::KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
            ui.key(event::KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
            ui.key(event::KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
            ui.key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            assert_eq!(ui.active_proxy().unwrap().instance.name, "proxy-02");
            assert_eq!(ui.page, OVERVIEW);
            if std::env::var_os("ASTRA_TUI_PREVIEW").is_some() {
                ui.page = OVERVIEW;
                let terminal = render(&ui, 140, 40);
                let cells: Vec<_> = terminal.backend().buffer().content.iter().map(|cell| serde_json::json!({"text":cell.symbol(),"fg":format!("{:?}",cell.fg),"bg":format!("{:?}",cell.bg)})).collect();
                std::fs::write(
                    concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/tui-preview.json"),
                    serde_json::to_vec(&cells).unwrap(),
                )
                .unwrap();
            }
        });
    }
}
