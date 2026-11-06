//! Overview page and shared mouse hit testing. Never synthesize telemetry.
use super::connections::{ConnectionRow, ConnectionRowKey};
use super::shell::{duration, CONNECTIONS, PAGES, PROXIES};
use super::*;
use astra_model::runtime::Mode;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::Rect,
    style::Modifier,
    widgets::{BorderType, Sparkline},
};

pub(super) use crate::theme::{BLUE, MUTED};

#[derive(Clone)]
pub(super) enum Target {
    TaskDetails,
    Page(usize),
    Row(usize),
    Menu(usize),
    Binding(usize),
    Back,
    Core,
    Runtime(astra_model::runtime::Command),
    Group(usize),
    Member(usize),
    Connection(u64),
    ConnectionRow(ConnectionRowKey),
    Connect,
    Palette(usize),
    NetTools,
    /// A settings row and, when every choice is shown, the clicked choice.
    Setting(usize, Option<usize>),
    Installation(super::installation::Click),
}

fn fit_cells(text: &str, width: usize) -> String {
    astra_tui_kit::text::truncate_cells(text, width, "…")
}

fn pad_cells(text: &str, width: usize) -> String {
    astra_tui_kit::text::pad_cells(text, width)
}

/// Current instance state as the uppercase colour code used by the layouts.
fn indicator_state(row: &Row) -> (&'static str, Color) {
    use astra_client::overview::InstanceState;
    match astra_client::overview::indicator_state(row) {
        InstanceState::Starting => ("●Y", Color::Yellow),
        InstanceState::Error => ("●R", Color::Red),
        InstanceState::Running => ("●G", Color::Green),
        InstanceState::Stopped => ("○", MUTED),
    }
}

pub(super) fn card(title: &str) -> Block<'_> {
    astra_tui_kit::Panel::new(format!(" {} ", t(title))).block()
}

impl Ui {
    pub(super) fn navigate(&mut self, page: usize) -> Input {
        self.focus = Default::default();
        let input = self.client.navigate(page);
        self.sync_overview_cursor(None);
        input
    }
    pub(super) fn navigate_back(&mut self) -> Input {
        self.focus = Default::default();
        let input = self.client.navigate_back();
        self.sync_overview_cursor(None);
        input
    }
    fn notices(&self) -> Vec<(Color, String)> {
        use astra_client::overview::Severity;
        self.client
            .notices()
            .into_iter()
            .map(|(severity, text)| {
                (
                    match severity {
                        Severity::Success => Color::Green,
                        Severity::Warning => Color::Yellow,
                        Severity::Error => Color::Red,
                    },
                    text,
                )
            })
            .collect()
    }

    pub(super) fn overview_input(&self) -> bool {
        self.live_input() && self.runtime_picker.is_none() && self.palette.is_none()
    }

    pub(super) fn hit(&self, rect: Rect, target: Target) {
        if rect.width > 0 && rect.height > 0 {
            self.hits.borrow_mut().insert(rect, target);
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

    pub(super) fn overview_header_height(&self, _width: u16) -> u16 {
        5
    }

    /// One bordered panel for the active instance, its profile and the page tabs.
    pub(super) fn render_overview_header(&self, f: &mut Frame<'_>, area: Rect) {
        if self.kind == Kind::Server {
            self.render_server_header(f, area);
            return;
        }
        let Some(row) = self.active_proxy() else {
            return;
        };
        if area.width < 4 || area.height < 3 {
            return;
        }
        let narrow = area.width < 80;
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let muted = Style::default().fg(MUTED);
        let dark = Style::default().fg(Color::DarkGray);
        let failed = row.state.starts_with("Exited")
            || row.state.starts_with("Unresponsive")
            || row.state.starts_with("Failed");
        let (dot, color, state) = if failed {
            ("●", Color::Red, t("Failed"))
        } else if row.pid.is_some() && row.state == "Running" {
            ("●", Color::Green, t("Connected"))
        } else {
            ("○", MUTED, t("Not connected"))
        };
        let color = self.indicator_color(color);
        let mut title = vec![Span::styled(" Astra", bold.fg(BLUE))];
        if narrow {
            title.push(Span::styled(" · ", dark));
            title.push(Span::styled(row.instance.name.clone(), bold));
            title.push(Span::styled(" · ", dark));
            title.push(Span::styled(
                format!("{dot} {state}"),
                Style::default().fg(color),
            ));
            if row.pid.is_some() {
                title.push(Span::styled(
                    format!(" · {} {}", t("Up"), duration(row.uptime)),
                    muted,
                ));
            }
        }
        title.push(Span::raw(" "));
        let block = Block::default()
            .title(Line::from(title))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(dark);
        let inner = block.inner(area);
        f.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let mode_style = |mode: Mode| match mode {
            Mode::Rule => Style::default().fg(Color::Cyan),
            Mode::Global => Style::default().fg(BLUE),
            Mode::Direct => Style::default().fg(Color::Green),
        };
        let mut status = Vec::new();
        if narrow {
            // The title already carries the profile name, state and uptime.
            status.push(Span::raw(" "));
        } else {
            status.push(Span::styled(format!(" {dot} "), Style::default().fg(color)));
            status.push(Span::styled(row.instance.name.clone(), bold));
            status.push(Span::styled(
                format!(" {state}"),
                Style::default().fg(color),
            ));
            if row.pid.is_some() {
                status.push(Span::styled(format!(" {}", duration(row.uptime)), muted));
            }
        }
        if let Some(snapshot) = self.live() {
            if !narrow {
                status.push(Span::styled("  │  ", dark));
            }
            status.push(Span::styled(
                t(match snapshot.mode {
                    Mode::Rule => "Rule",
                    Mode::Global => "Global",
                    Mode::Direct => "Direct",
                }),
                bold.patch(mode_style(snapshot.mode)),
            ));
            let (chain, delay, alive) = live::exit_chain(snapshot);
            if !chain.is_empty() {
                status.push(Span::styled(format!("  ·  {} ", t("Exit")), muted));
                for (index, hop) in chain.iter().enumerate() {
                    if index > 0 {
                        status.push(Span::styled(" ▸ ", dark));
                    }
                    status.push(if index + 1 == chain.len() {
                        Span::styled(hop.clone(), bold.fg(BLUE))
                    } else {
                        Span::raw(hop.clone())
                    });
                }
                status.push(Span::raw("  "));
                status.push(live::delay_span(delay, alive));
            }
            status.push(Span::styled(format!(" · {}", self.runtime_health()), muted));
        } else if narrow {
            status.push(Span::styled(t("Waiting for runtime data…"), muted));
        } else {
            status.push(Span::styled(
                format!("  ·  {}", t("Waiting for runtime data…")),
                muted,
            ));
        }
        let compact = area.width < 110;
        let rates = self.rate_spans(0, compact);
        Self::render_right_aligned(
            f,
            Rect::new(inner.x, inner.y, inner.width, 1),
            status,
            rates,
        );

        let mut meta = vec![Span::raw(" ")];
        if let Some(details) = self.details.get(&row.instance.name) {
            if !details.listeners.is_empty() {
                meta.push(Span::raw(details.listeners.join(" · ")));
            }
            if details.outbound_count > 0 {
                if !details.listeners.is_empty() {
                    meta.push(Span::styled("  ·  ", muted));
                }
                meta.push(Span::styled(
                    tr!("{} outbounds", details.outbound_count),
                    muted,
                ));
            }
        }
        if let Some(rule) = self.active_rule() {
            if meta.len() > 1 {
                meta.push(Span::styled("  ·  ", muted));
            }
            meta.push(Span::styled(tr!("Rule core {}", rule.instance.name), muted));
        }
        if meta.len() == 1 {
            meta.push(Span::styled(t("No listener details yet."), muted));
        }
        let meta_row = inner.y + 1;
        if meta_row < inner.bottom() {
            Self::render_right_aligned(
                f,
                Rect::new(inner.x, meta_row, inner.width, 1),
                meta,
                self.rate_spans(1, compact),
            );
        }
        // Span the whole panel so the separator replaces the side borders.
        self.render_embedded_page_bar(f, Rect::new(area.x, inner.bottom() - 1, area.width, 1));
    }

    pub(super) fn rate_spans(&self, index: usize, compact: bool) -> Vec<Span<'static>> {
        let available = self.runtime_rates_available();
        let arrow = if index == 0 { "↑" } else { "↓" };
        let color = if index == 0 {
            Color::Magenta
        } else {
            Color::Cyan
        };
        let style = |value: u64, color: Color| {
            if !available || value == 0 {
                Style::default().fg(MUTED)
            } else {
                Style::default().fg(color)
            }
        };
        let value = |bytes| live::rate_value(available.then_some(bytes), compact);
        vec![
            Span::styled("Proxy ", Style::default().fg(MUTED)),
            Span::styled(
                format!("{arrow} {}/s", value(self.rates[index])),
                style(self.rates[index], color),
            ),
            Span::styled(" · Direct ", Style::default().fg(MUTED)),
            Span::styled(
                format!("{arrow} {}/s", value(self.direct_rates[index])),
                style(self.direct_rates[index], color),
            ),
            Span::raw(" "),
        ]
    }

    pub(super) fn render_right_aligned(
        f: &mut Frame<'_>,
        area: Rect,
        mut left: Vec<Span<'static>>,
        right: Vec<Span<'static>>,
    ) {
        let left_width: usize = left.iter().map(Span::width).sum();
        let right_width: usize = right.iter().map(Span::width).sum();
        if right_width < area.width as usize && left_width + right_width >= area.width as usize {
            let mut remaining = (area.width as usize).saturating_sub(right_width + 1);
            left = left
                .into_iter()
                .filter_map(|span| {
                    if remaining == 0 {
                        return None;
                    }
                    let text = fit_cells(&span.content, remaining);
                    remaining = remaining.saturating_sub(Line::from(text.as_str()).width());
                    Some(Span::styled(text, span.style))
                })
                .collect();
        }
        let left_width: usize = left.iter().map(Span::width).sum();
        if !right.is_empty() && left_width + right_width < area.width as usize {
            left.push(Span::raw(
                " ".repeat(area.width as usize - left_width - right_width),
            ));
            left.extend(right);
        }
        f.render_widget(Paragraph::new(Line::from(left)), area);
    }

    /// Page tabs drawn on a separator row inside the merged Astra panel.
    fn render_embedded_page_bar(&self, f: &mut Frame<'_>, area: Rect) {
        if area.width < 3 || area.height == 0 {
            return;
        }
        let border = Style::default().fg(Color::DarkGray);
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("├", border),
                Span::styled("─".repeat(area.width.saturating_sub(2) as usize), border),
                Span::styled("┤", border),
            ])),
            area,
        );
        let compact = area.width < 100;
        let mut x = area.x + 2;
        for (index, page) in PAGES.iter().enumerate() {
            let label = if compact && index != self.page {
                format!(" {} ", index + 1)
            } else {
                format!(" {} {} ", index + 1, t(page))
            };
            let width = Line::from(label.as_str()).width() as u16;
            if x + width + 1 >= area.right() {
                break;
            }
            let rect = Rect::new(x, area.y, width, 1);
            f.render_widget(
                Paragraph::new(label).style(if index == self.page {
                    Style::default()
                        .bg(BLUE)
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(MUTED)
                }),
                rect,
            );
            self.hit(rect, Target::Page(index));
            x = x.saturating_add(width + 1);
        }
    }

    pub(super) fn render_overview(&self, f: &mut Frame<'_>, area: Rect) {
        self.record_live_viewport(live::Viewport::Overview, 0);
        self.record_live_viewport(live::Viewport::Groups, 0);
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
        let notice_lines: usize = notices
            .iter()
            .map(|(_, text)| {
                (astra_tui_kit::text::width(text) + 3)
                    .div_ceil(area.width.saturating_sub(2).max(1) as usize)
                    .max(1)
            })
            .sum();
        let bands = Layout::vertical([
            Constraint::Length(if notices.is_empty() {
                0
            } else {
                (notice_lines.saturating_add(2).min(u16::MAX as usize) as u16)
                    .min(area.height.saturating_sub(3))
            }),
            Constraint::Length(self.runtime_controls_height(area.width)),
            Constraint::Min(0),
        ])
        .split(area);
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
                bands[0],
            );
        }
        self.render_runtime_modes(f, bands[1]);

        self.render_overview_cards(f, bands[2], row);
    }

    /// Connections are the main area; errors sit above, groups below or beside.
    fn render_overview_cards(&self, f: &mut Frame<'_>, area: Rect, row: &Row) {
        if area.height < 3 {
            return;
        }
        if self.traffic_expanded && area.width >= 80 && area.height >= 10 {
            let requested = ((area.height as u32 * 40 / 100) as u16).clamp(6, 14);
            let traffic_height = requested.min(area.height.saturating_sub(4));
            let rows = Layout::vertical([Constraint::Length(traffic_height), Constraint::Min(0)])
                .split(area);
            self.render_traffic(f, rows[0], row);
            self.render_overview_main(f, rows[1], row);
        } else {
            self.render_overview_main(f, area, row);
        }
    }

    fn render_overview_main(&self, f: &mut Frame<'_>, area: Rect, row: &Row) {
        let connection_count = self.connection_rows().len();
        if area.width >= 120 {
            let columns =
                Layout::horizontal([Constraint::Percentage(71), Constraint::Percentage(29)])
                    .split(area);
            self.overview_item_count.set(connection_count);
            self.render_overview_connections(f, columns[0], false);
            self.render_snapshot(f, columns[1]);
            return;
        }
        let narrow = area.width < 80;
        let groups_height = 6.min(area.height.saturating_sub(4));
        let groups_visible = groups_height >= 4;
        let (limit, groups_start) = match self.live() {
            Some(snapshot) if narrow && groups_visible => (
                connection_count + snapshot.groups.len(),
                self.overview_cursor.saturating_sub(connection_count),
            ),
            Some(_) => (connection_count, 0),
            None => (0, 0),
        };
        self.overview_item_count.set(limit);
        if groups_visible {
            let rows = Layout::vertical([Constraint::Min(0), Constraint::Length(groups_height)])
                .split(area);
            self.render_overview_connections(f, rows[0], narrow);
            let title = self
                .live()
                .map(|snapshot| tr!("PROXY GROUPS · {}", snapshot.groups.len()))
                .unwrap_or_else(|| t("PROXY GROUPS").to_owned());
            self.render_groups_panel(f, rows[1], &title, true, groups_start);
        } else {
            self.render_overview_connections(f, area, narrow);
        }
        let _ = row;
    }

    fn render_snapshot(&self, f: &mut Frame<'_>, area: Rect) {
        let block = self.focused_card("SNAPSHOT", focus::Region::Secondary);
        let inner = block.inner(area);
        f.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        let muted = Style::default().fg(MUTED);
        f.render_widget(
            Paragraph::new(self.instance_indicator()),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );
        let footer = inner.height >= 6;
        let bottom = if footer {
            inner.bottom() - 1
        } else {
            inner.bottom()
        };
        let mut y = inner.y + 1;
        if bottom.saturating_sub(y) >= 4 {
            y += 1;
            let label = self
                .live()
                .map(|snapshot| tr!("PROXY GROUPS · {}", snapshot.groups.len()))
                .unwrap_or_else(|| t("PROXY GROUPS").to_owned());
            f.render_widget(
                Paragraph::new(Span::styled(label, muted)),
                Rect::new(inner.x, y, inner.width, 1),
            );
            y += 1;
        }
        self.render_group_rows(
            f,
            Rect::new(inner.x, y, inner.width, bottom.saturating_sub(y)),
            0,
        );
        if footer {
            f.render_widget(
                Paragraph::new(Span::styled(format!(" {}", t("p switch profiles")), muted)),
                Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
            );
        }
    }

    fn render_groups_panel(
        &self,
        f: &mut Frame<'_>,
        area: Rect,
        title: &str,
        indicator: bool,
        start: usize,
    ) {
        let block = self.focused_card(title, focus::Region::Secondary);
        let inner = block.inner(area);
        f.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        let mut y = inner.y;
        if indicator {
            f.render_widget(
                Paragraph::new(self.instance_indicator()),
                Rect::new(inner.x, y, inner.width, 1),
            );
            y += 1;
        }
        self.render_group_rows(
            f,
            Rect::new(inner.x, y, inner.width, inner.bottom().saturating_sub(y)),
            start,
        );
    }

    fn instance_indicator(&self) -> Line<'static> {
        let mut spans = vec![Span::raw(" ")];
        if let Some(proxy) = self.active_proxy() {
            let (flag, color) = indicator_state(proxy);
            spans.push(Span::styled(flag.to_owned(), Style::default().fg(color)));
            spans.push(Span::raw(format!(" {} · P", proxy.instance.name)));
        }
        spans.push(Span::raw("   "));
        if let Some(rule) = self.active_rule() {
            let (flag, color) = indicator_state(rule);
            spans.push(Span::styled(flag.to_owned(), Style::default().fg(color)));
            spans.push(Span::raw(format!(" {} · R", rule.instance.name)));
        } else {
            spans.push(Span::styled("○ — · R", Style::default().fg(MUTED)));
        }
        Line::from(spans)
    }

    fn render_group_rows(&self, f: &mut Frame<'_>, area: Rect, start: usize) {
        self.record_live_viewport(
            live::Viewport::OverviewGroups,
            if area.width > 0 {
                area.height as usize
            } else {
                0
            },
        );
        if area.width == 0 || area.height == 0 {
            return;
        }
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
                area.inner(ratatui::layout::Margin::new(1, 0)),
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
        let visible = area.height as usize;
        let total = snapshot.groups.len();
        let selected = if self.focus.region == focus::Region::Secondary {
            Some(self.group_selection)
        } else if self.overview_cursor >= self.connection_rows().len()
            && self.overview_cursor < self.overview_item_count.get()
        {
            Some(start)
        } else {
            None
        };
        let start = self.live_list_offset(live::Viewport::OverviewGroups, selected, total, visible);
        for (offset, group) in snapshot.groups.iter().skip(start).take(visible).enumerate() {
            let selected = group.selected.as_deref().unwrap_or("—");
            let member = group.members.iter().find(|m| m.name == selected);
            let rect = Rect::new(area.x, area.y + offset as u16, area.width, 1);
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
                ]))
                .style(
                    if self.focus.region == focus::Region::Secondary
                        && self.group_selection == start + offset
                    {
                        crate::theme::selected_keep_colors()
                    } else {
                        Style::default()
                    },
                ),
                rect,
            );
            self.hit(rect, Target::Group(start + offset));
        }
        let hidden = total.saturating_sub(start + visible);
        if hidden > 0 && area.height > 0 {
            let more = format!(" +{hidden} ");
            let width = more.len() as u16;
            f.render_widget(
                Paragraph::new(more).style(Style::default().fg(MUTED)),
                Rect::new(
                    area.right().saturating_sub(width),
                    area.bottom() - 1,
                    width,
                    1,
                ),
            );
        }
    }

    fn render_overview_connections(&self, f: &mut Frame<'_>, area: Rect, narrow: bool) {
        if area.width < 4 || area.height < 3 {
            return;
        }
        let prefix = self.connection_badges();
        let heading = if narrow {
            t("CONNECTIONS")
        } else {
            t("LIVE CONNECTIONS")
        };
        let title = self.connection_snapshot().map_or_else(
            || format!("{prefix}{heading}"),
            |s| {
                format!(
                    "{prefix}{heading} · {}/{}{}",
                    s.connections.len(),
                    s.active,
                    if s.truncated { " · partial" } else { "" }
                )
            },
        );
        let block = self.focused_card(&title, focus::Region::Main);
        let inner = block.inner(area);
        f.render_widget(block, area);
        self.hit(area, Target::Page(CONNECTIONS));
        self.render_border_sparkline(f, area, &title);

        let Some(_) = self.connection_snapshot() else {
            f.render_widget(
                Paragraph::new(t("Connections appear after connecting."))
                    .style(Style::default().fg(MUTED)),
                inner.inner(ratatui::layout::Margin::new(1, 0)),
            );
            return;
        };
        let connections = self.connection_rows();
        if connections.is_empty() {
            f.render_widget(
                Paragraph::new(t("No active connections")).style(Style::default().fg(MUTED)),
                inner.inner(ratatui::layout::Margin::new(1, 0)),
            );
            return;
        }
        let footer = if inner.height >= 4 { 1 } else { 0 };
        let list = Rect::new(
            inner.x,
            inner.y,
            inner.width,
            inner.height.saturating_sub(footer),
        );
        if narrow {
            self.render_connection_rows_narrow(f, list, &connections);
        } else {
            self.render_connection_rows(f, list, &connections);
        }
        if footer > 0 {
            let text = if let Some(hint) = self.connection_pause_hint() {
                hint
            } else if narrow {
                tr!("↑↓ select · Enter expand/details · Space pause")
            } else {
                tr!(
                    "Enter expand/open · ←→ fold · Space pause · {}",
                    if self.traffic_expanded {
                        t("z collapse")
                    } else {
                        t("z chart")
                    }
                )
            };
            f.render_widget(
                Paragraph::new(Line::from(crate::theme::key_hints(&format!(" {text}")))),
                Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
            );
        }
    }

    fn render_connection_rows(&self, f: &mut Frame<'_>, area: Rect, connections: &[ConnectionRow]) {
        let visible = area.height as usize;
        let selected_key = self.selected_connection_row_key();
        let start = self.live_list_offset(
            live::Viewport::Overview,
            connections
                .iter()
                .position(|row| selected_key.as_ref() == Some(&row.key)),
            connections.len(),
            visible,
        );
        let fit = |text: String, width: usize| fit_cells(&text, width);
        let outbound_width = connections
            .iter()
            .map(|row| Line::from(row.connection.outbound.as_str()).width())
            .max()
            .unwrap_or(0)
            .clamp(6, 24);
        let host_width = (area.width as usize)
            .saturating_sub(outbound_width + 27)
            .clamp(12, 48);
        let lines: Vec<_> = connections
            .iter()
            .skip(start)
            .take(visible)
            .map(|row| {
                let c = &row.connection;
                let target = fit(row.label(), host_width);
                let outbound = fit(c.outbound.clone(), outbound_width);
                Line::from(vec![
                    Span::raw(format!(" {} ", pad_cells(&target, host_width))),
                    Span::styled(
                        format!("{} ", pad_cells(&outbound, outbound_width)),
                        Style::default().fg(crate::theme::exit_color(&c.outbound)),
                    ),
                    Span::styled("↓ ", Style::default().fg(MUTED)),
                    Span::styled(
                        format!("{:>9}", live::bytes(c.download)),
                        crate::theme::volume_style(c.download),
                    ),
                    Span::styled("  ↑ ", Style::default().fg(MUTED)),
                    Span::styled(
                        format!("{:>9}", live::bytes(c.upload)),
                        crate::theme::volume_style(c.upload),
                    ),
                ])
                .style(crate::theme::connection_row_style(
                    selected_key.as_ref() == Some(&row.key),
                    row.fresh,
                ))
            })
            .collect();
        f.render_widget(Paragraph::new(lines), area);
        for index in 0..connections.len().saturating_sub(start).min(visible) {
            self.hit(
                Rect::new(area.x + 1, area.y + index as u16, area.width - 2, 1),
                match &connections[start + index].key {
                    ConnectionRowKey::Connection(id) => Target::Connection(*id),
                    key => Target::ConnectionRow(key.clone()),
                },
            );
        }
    }

    fn render_connection_rows_narrow(
        &self,
        f: &mut Frame<'_>,
        area: Rect,
        connections: &[ConnectionRow],
    ) {
        let visible = area.height as usize / 2;
        let selected_key = self.selected_connection_row_key();
        let start = self.live_list_offset(
            live::Viewport::Overview,
            connections
                .iter()
                .position(|row| selected_key.as_ref() == Some(&row.key)),
            connections.len(),
            visible,
        );
        let fit = |text: String, width: usize| fit_cells(&text, width);
        for (offset, row) in connections.iter().skip(start).take(visible).enumerate() {
            let c = &row.connection;
            let row_style = crate::theme::connection_row_style(
                selected_key.as_ref() == Some(&row.key),
                row.fresh,
            );
            let y = area.y + (offset * 2) as u16;
            if y + 1 >= area.bottom() {
                break;
            }
            f.render_widget(
                Paragraph::new(Line::from(Span::raw(format!(
                    " {}",
                    fit(row.label(), area.width as usize - 2)
                ))))
                .style(row_style),
                Rect::new(area.x, y, area.width, 1),
            );
            let counters = format!("↓{}  ↑{}", live::bytes(c.download), live::bytes(c.upload));
            let counter_width = Line::from(counters.as_str()).width();
            let route_width = (area.width as usize)
                .saturating_sub(counter_width + 3)
                .max(6);
            let route = fit(format!("{} · {}", c.outbound, c.network), route_width);
            let route_spans = vec![
                Span::raw(format!(" {} ", pad_cells(&route, route_width))),
                Span::styled(
                    format!(
                        "{:>width$}",
                        format!("↓{}", live::bytes(c.download)),
                        width = counter_width.saturating_sub(
                            Line::from(format!("↑{}", live::bytes(c.upload))).width() + 2
                        )
                    ),
                    crate::theme::volume_style(c.download),
                ),
                Span::raw("  "),
                Span::styled(
                    format!("↑{}", live::bytes(c.upload)),
                    crate::theme::volume_style(c.upload),
                ),
            ];
            f.render_widget(
                Paragraph::new(Line::from(route_spans)).style(row_style),
                Rect::new(area.x, y + 1, area.width, 1),
            );
            self.hit(
                Rect::new(area.x + 1, y, area.width.saturating_sub(2), 2),
                match &row.key {
                    ConnectionRowKey::Connection(id) => Target::Connection(*id),
                    key => Target::ConnectionRow(key.clone()),
                },
            );
        }
    }

    fn render_border_sparkline(&self, f: &mut Frame<'_>, area: Rect, title: &str) {
        if !self.runtime_rates_available() {
            return;
        }
        let len = self.history[0].len().min(self.history[1].len());
        let data: Vec<u64> = (0..len)
            .map(|index| self.history[0][index].saturating_add(self.history[1][index]))
            .collect();
        if data.len() < 2 || data.iter().all(|value| *value == 0) {
            return;
        }
        let title_width = Line::from(title).width() as u16;
        let room = area.width.saturating_sub(title_width + 8).min(24);
        if room < 8 {
            return;
        }
        let width = room as usize;
        let tail = &data[data.len().saturating_sub(width)..];
        let chart = Rect::new(
            area.right() - tail.len() as u16 - 2,
            area.y,
            tail.len() as u16,
            1,
        );
        f.render_widget(
            Sparkline::default()
                .data(tail)
                .style(Style::default().fg(Color::Cyan)),
            chart,
        );
    }

    fn render_traffic(&self, f: &mut Frame<'_>, area: Rect, row: &Row) {
        let title = tr!("Proxy traffic · z collapse");
        let block = card(&title);
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
                        live::bytes(snapshot.proxy_download),
                        live::bytes(snapshot.proxy_upload)
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
            let label = if self.runtime_rates_available() {
                format!(" {arrow} {:>10}/s ", live::bytes(self.rates[index]))
            } else {
                format!(" {arrow} —/s ")
            };
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
            let chart = Rect::new(
                lane.right().saturating_sub(width + 1),
                lane.y,
                width,
                lane.height,
            );
            f.render_widget(
                Sparkline::default()
                    .data(data)
                    .style(Style::default().fg(color)),
                chart,
            );
        }
    }

    pub(super) fn menu_hits(&self, area: Rect, offset: usize) {
        let count = self
            .binding_picker
            .as_ref()
            .map_or_else(|| self.operations().len(), |p| p.choices.len());
        let rows = astra_tui_kit::RenderedRows::fixed(card("").inner(area), offset, count, 1);
        self.hits.borrow_mut().bind_rows(&rows, |index| {
            if self.binding_picker.is_some() {
                Target::Binding(index)
            } else {
                Target::Menu(index)
            }
        });
    }

    pub(super) fn mouse(&mut self, mouse: MouseEvent) -> Input {
        if self.client.installation.visible() {
            return self.installation_mouse(mouse);
        }
        if self.monitor.is_some() {
            return Input::Idle;
        }
        if matches!(
            mouse.kind,
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
        ) {
            if self.page == shell::SETTINGS
                && self.overview_input()
                && !self.client.tasks.showing_details()
                && self.tui_viewer.is_none()
            {
                self.scroll_settings(mouse.kind == MouseEventKind::ScrollDown);
                return Input::Idle;
            }
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
        let target = self.hits.borrow().hit(mouse.column, mouse.row).cloned();
        if self.palette.is_some() && !matches!(target, Some(Target::Palette(_))) {
            return Input::Idle;
        }
        match target {
            Some(Target::TaskDetails) => {
                self.key(event::KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE))
            }
            Some(Target::Palette(index)) => self.palette_run(index),
            Some(Target::Connect) if self.can_switch_page() => self.toggle_active(),
            Some(Target::NetTools) if self.can_switch_page() => {
                self.active_operation(super::Operation::NetTools)
            }
            Some(Target::Runtime(command)) => self.runtime_action(command),
            Some(Target::Group(index)) => {
                if self.page != PROXIES {
                    self.navigate(PROXIES);
                }
                self.group_selection = index;
                self.open_runtime_group();
                Input::Idle
            }
            Some(Target::Member(index)) => self.choose_runtime_member(index),
            Some(Target::ConnectionRow(key)) => {
                self.focus.region = focus::Region::Main;
                self.select_connection_row_key(key);
                self.toggle_connection_group();
                Input::Idle
            }
            Some(Target::Connection(id)) => {
                self.focus.region = focus::Region::Main;
                self.select_connection_id(id);
                if self.page == super::shell::OVERVIEW {
                    return self.navigate(CONNECTIONS);
                }
                Input::Idle
            }
            Some(Target::Page(page)) if self.can_switch_page() => self.navigate(page),
            Some(Target::Setting(row, choice)) => self.click_setting(row, choice),
            Some(Target::Installation(_)) => Input::Idle,
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
            Some(Target::Core) => {
                let kind = next_kind(self.kind);
                self.switch_core(kind);
                self.focus.region = focus::Region::Controls;
                self.focus.cursor = kind_index(kind);
                Input::Idle
            }
            _ => Input::Idle,
        }
    }
}

#[cfg(test)]
#[path = "../test/ui_dashboard_test.rs"]
mod tests;
