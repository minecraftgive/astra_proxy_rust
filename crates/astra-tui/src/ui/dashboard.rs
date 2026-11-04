//! Overview page and shared mouse hit testing. Never synthesize telemetry.
use super::connections::{ConnectionRow, ConnectionRowKey};
use super::shell::{duration, CONNECTIONS, LOGS, PAGES, PROXIES};
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
}

fn fit_cells(text: &str, width: usize) -> String {
    astra_tui_kit::text::truncate_cells(text, width, "…")
}

fn pad_cells(text: &str, width: usize) -> String {
    astra_tui_kit::text::pad_cells(text, width)
}

/// Current instance state as the uppercase colour code used by the layouts.
fn indicator_state(row: &Row) -> (&'static str, Color) {
    if row.state == "Starting" || row.state == "Stopping" {
        ("●Y", Color::Yellow)
    } else if row.state.starts_with("Unresponsive")
        || row.state.starts_with("Exited")
        || row.state.starts_with("Failed")
    {
        ("●R", Color::Red)
    } else if row.pid.is_some() && row.state == "Running" {
        ("●G", Color::Green)
    } else {
        ("○", MUTED)
    }
}

pub(super) fn card(title: &str) -> Block<'_> {
    astra_tui_kit::Panel::new(format!(" {} ", t(title))).block()
}

impl Ui {
    pub(super) fn overview_input(&self) -> bool {
        self.live_input() && self.runtime_picker.is_none() && self.palette.is_none()
    }

    pub(super) fn remember_page(&mut self, page: usize) {
        if self.page != page {
            self.navigation_stack.push(self.page);
            self.page = page;
        }
    }

    pub(super) fn navigate_back(&mut self) -> Input {
        let Some(page) = self.navigation_stack.pop() else {
            return Input::Idle;
        };
        // navigate performs normal page setup; returning must not push history.
        let history_len = self.navigation_stack.len();
        let input = self.navigate(page);
        self.navigation_stack.truncate(history_len);
        input
    }

    pub(super) fn navigate(&mut self, page: usize) -> Input {
        self.suspend_editor();
        self.focus = Default::default();
        let page = page.min(PAGES.len() - 1);
        if self.page == LOGS && page != LOGS {
            self.close_viewer();
        }
        self.remember_page(page);
        if page == shell::SETTINGS {
            self.refresh_program_info();
        }
        if page == CONNECTIONS {
            self.reveal_selected_connection();
        }
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

    /// Recovery feedback and problems that need the user's attention.
    fn notices(&self) -> Vec<(Color, String)> {
        self.notice_expiry.set(None);
        let mut notices = Vec::new();
        let Some(row) = self.active_proxy() else {
            return notices;
        };
        if let Some(notice) = self
            .details
            .get(&row.instance.name)
            .and_then(|d| d.network_recovery.as_ref())
        {
            use astra_model::recovery::RecoveryState;
            let (label, color) = match notice.state {
                RecoveryState::Recovered => ("Network settings recovered", Color::Green),
                RecoveryState::Pending => ("Network recovery pending", Color::Red),
                RecoveryState::Conflict => ("Network recovery conflict", Color::Red),
            };
            let time = i64::try_from(notice.timestamp_ms)
                .ok()
                .and_then(chrono::DateTime::from_timestamp_millis)
                .map(|date| {
                    date.with_timezone(&chrono::Local)
                        .format("%Y-%m-%d %H:%M:%S %:z")
                        .to_string()
                })
                .unwrap_or_else(|| "?".into());
            notices.push((color, format!("{} · {time}", t(label))));
            notices.push((color, notice.message.clone()));
            notices.push((
                color,
                format!("{}: {}", t("Recovery record"), notice.journal.display()),
            ));
        }
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
        if row.pid.is_some() && !self.runtime_error.is_empty() {
            notices.push((Color::Red, self.runtime_error.clone()));
        }
        if self
            .details
            .get(&row.instance.name)
            .is_some_and(|d| d.config_unavailable)
        {
            notices.push((Color::Red, t("Configuration unavailable · e edit").into()));
        }
        // Successes and warnings recede after 15 seconds; errors stay until fixed.
        let now = Instant::now();
        let mut seen = self.notice_since.borrow_mut();
        seen.retain(|message, _| notices.iter().any(|(_, text)| text == message));
        notices.retain(|(color, message)| {
            if !matches!(color, Color::Yellow | Color::Green) {
                return true;
            }
            let deadline = *seen.entry(message.clone()).or_insert(now) + Duration::from_secs(15);
            if deadline <= now {
                return false;
            }
            self.notice_expiry.set(Some(
                self.notice_expiry
                    .get()
                    .map_or(deadline, |next| next.min(deadline)),
            ));
            true
        });
        notices
    }

    pub(super) fn next_notice_expiry(&self) -> Option<Instant> {
        if self.page == shell::OVERVIEW && self.kind != Kind::Server {
            self.notice_expiry.get()
        } else {
            None
        }
    }

    pub(super) fn expire_notices(&self) -> bool {
        if self
            .next_notice_expiry()
            .is_some_and(|deadline| deadline <= Instant::now())
        {
            // Keep first-seen times so a later redraw cannot revive expired notices.
            self.notice_expiry.set(None);
            true
        } else {
            false
        }
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
        if self.monitor.is_some() {
            return Input::Idle;
        }
        if matches!(
            mouse.kind,
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
        ) {
            if self.page == shell::SETTINGS
                && self.overview_input()
                && !self.tasks.showing_details()
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
mod tests {
    use super::super::shell::{OVERVIEW, PROFILES};
    use super::*;
    use ratatui::backend::TestBackend;

    #[test]
    fn escape_returns_through_page_history_after_closing_dialogs() {
        let mut ui = Ui::new(Session::default());
        ui.navigate(PROFILES);
        ui.navigate(PROFILES); // Reselecting a page must not add a frame.
        ui.navigate(PROXIES);
        ui.help_scroll = Some(0);
        let escape = event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        ui.key(escape);
        assert!(ui.help_scroll.is_none());
        assert_eq!(ui.page, PROXIES);
        ui.key(escape);
        assert_eq!(ui.page, PROFILES);
        ui.key(escape);
        assert_eq!(ui.page, OVERVIEW);
        ui.key(escape);
        assert_eq!(ui.page, OVERVIEW);
        assert!(ui.navigation_stack.is_empty());
    }

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
    fn live_sample() -> Ui {
        use astra_model::runtime::Connection;
        use astra_model::runtime::Group;
        use astra_model::runtime::Member;
        use astra_model::runtime::Snapshot;
        let mut ui = sample();
        ui.accept_runtime(
            "proxy-00".into(),
            Snapshot {
                pid: 100,
                mode: Mode::Rule,
                system_proxy: true,
                active: 3,
                total: 9,
                upload: 50_000,
                download: 900_000,
                // Deliberately unsorted: the Overview orders by total traffic.
                connections: vec![
                    Connection {
                        id: 3,
                        source: None,
                        host: "data.bilibili.com".into(),
                        port: 443,
                        network: "Udp".into(),
                        inbound: "Tun".into(),
                        outbound: "DIRECT".into(),
                        upload: 353_500,
                        download: 87_600,
                        elapsed_ms: 3000,
                    },
                    Connection {
                        id: 2,
                        source: None,
                        host: "claude.ai".into(),
                        port: 443,
                        network: "Tcp".into(),
                        inbound: "Mixed".into(),
                        outbound: "AJP-80-Direct-Vision".into(),
                        upload: 1_400_000,
                        download: 44_900,
                        elapsed_ms: 3000,
                    },
                    Connection {
                        id: 1,
                        source: None,
                        host: "j-cn-live-comet.chat.bilibili.com".into(),
                        port: 2245,
                        network: "Tcp".into(),
                        inbound: "Mixed".into(),
                        outbound: "DIRECT".into(),
                        upload: 18_800,
                        download: 19_800_000,
                        elapsed_ms: 3000,
                    },
                ],
                groups: vec![
                    Group {
                        name: "PROXY".into(),
                        mode: "select".into(),
                        selected: Some("AJP-80-Direct-Vision".into()),
                        testing: false,
                        members: vec![Member {
                            name: "AJP-80-Direct-Vision".into(),
                            delay_ms: Some(38),
                            alive: Some(true),
                        }],
                    },
                    Group {
                        name: "DIRECT".into(),
                        mode: "select".into(),
                        selected: Some("DIRECT".into()),
                        testing: false,
                        members: vec![],
                    },
                ],
                ..Snapshot::default()
            },
        );
        ui.accept_runtime("proxy-00".into(), ui.live().unwrap().clone());
        ui.rates = [2_300, 762_900];
        for value in 0..64u64 {
            ui.history[0].push_back(if value % 3 == 0 { 2_300 } else { 0 });
            ui.history[1].push_back(762_900 - value * 8_000);
        }
        ui
    }
    #[test]
    fn retained_pid_does_not_make_an_unresponsive_core_look_healthy() {
        let mut ui = sample();
        ui.rows[0].state = "Unresponsive".into();
        assert_eq!(indicator_state(&ui.rows[0]), ("●R", Color::Red));
        ui.rows[0].state = "Starting".into();
        assert_eq!(indicator_state(&ui.rows[0]), ("●Y", Color::Yellow));
    }
    #[test]
    fn network_recovery_notice_keeps_conflict_evidence_visible() {
        crate::i18n::with_language(crate::i18n::Language::English, || {
            let mut ui = sample();
            ui.details.get_mut("proxy-00").unwrap().network_recovery =
                Some(astra_model::recovery::Notice {
                    state: astra_model::recovery::RecoveryState::Conflict,
                    timestamp_ms: 1700000000000,
                    message: "Route changed externally; retained".into(),
                    journal: "C:/Astra/recovery.json".into(),
                    boot_id: "boot".into(),
                });
            let notices = ui.notices();
            assert_eq!(notices.len(), 3);
            assert!(notices.iter().all(|(color, _)| *color == Color::Red));
            assert!(
                notices[0].1.contains("Network recovery conflict")
                    && notices[0].1.contains("2023-")
            );
            assert!(notices[1].1.contains("retained"));
            assert!(notices[2].1.contains("C:/Astra/recovery.json"));
            assert!(ui.next_notice_expiry().is_none());
            let old = Instant::now().checked_sub(Duration::from_secs(16)).unwrap();
            for (_, message) in &notices {
                ui.notice_since.borrow_mut().insert(message.clone(), old);
            }
            assert_eq!(ui.notices(), notices, "conflicts never time out");
            ui.details
                .get_mut("proxy-00")
                .unwrap()
                .network_recovery
                .as_mut()
                .unwrap()
                .state = astra_model::recovery::RecoveryState::Pending;
            let pending = ui.notices();
            assert_eq!(pending.len(), 3);
            assert!(pending.iter().all(|(color, _)| *color == Color::Red));
            assert!(ui.next_notice_expiry().is_none());
        });
    }
    #[test]
    fn network_recovery_success_expires_without_reviving_on_redraw() {
        crate::i18n::with_language(crate::i18n::Language::English, || {
            let mut ui = sample();
            ui.details.get_mut("proxy-00").unwrap().network_recovery =
                Some(astra_model::recovery::Notice {
                    state: astra_model::recovery::RecoveryState::Recovered,
                    timestamp_ms: 1700000000000,
                    message: "Owned network settings restored".into(),
                    journal: "C:/Astra/recovery.json".into(),
                    boot_id: "boot".into(),
                });
            let text = screen(&render(&ui, 140, 40));
            assert!(text.contains("Attention"));
            assert!(text.contains("Network settings recovered"));
            assert!(text.contains("Owned network settings restored"));
            assert!(text.contains("C:/Astra/recovery.json"));
            assert_eq!(ui.notices().len(), 3);
            assert!(ui.next_notice_expiry().is_some());
            assert!(!ui.expire_notices(), "not due yet");

            let old = Instant::now().checked_sub(Duration::from_secs(16)).unwrap();
            for shown in ui.notice_since.borrow_mut().values_mut() {
                *shown = old;
            }
            ui.notice_expiry.set(Some(old + Duration::from_secs(15)));
            assert!(ui.expire_notices(), "wake even without input or telemetry");
            assert!(ui.next_notice_expiry().is_none());
            assert!(!ui.expire_notices(), "consume the idle wakeup only once");
            for _ in 0..2 {
                let text = screen(&render(&ui, 140, 40));
                assert!(!text.contains("Attention"));
                assert!(!text.contains("Network settings recovered"));
                assert!(!text.contains("Owned network settings restored"));
                assert!(!text.contains("C:/Astra/recovery.json"));
                assert!(ui.next_notice_expiry().is_none());
            }
            assert!(ui.details["proxy-00"].network_recovery.is_some());
        });
    }
    #[test]
    fn overview_notice_expiry_waits_until_overview_is_visible() {
        let mut ui = sample();
        ui.rows[0].changed = true;
        assert_eq!(ui.notices().len(), 1);
        let deadline = ui.next_notice_expiry().unwrap();
        ui.page = PROFILES;
        assert!(ui.next_notice_expiry().is_none());
        assert!(!ui.expire_notices());
        ui.page = OVERVIEW;
        assert_eq!(ui.next_notice_expiry(), Some(deadline));
        ui.rows[0].changed = false;
        assert!(ui.notices().is_empty());
        assert!(ui.next_notice_expiry().is_none());
    }
    #[test]
    fn overview_attention_hides_warnings_but_keeps_errors() {
        crate::i18n::with_language(crate::i18n::Language::English, || {
            let mut ui = sample();
            ui.rows[0].state = "Exited(1)".into();
            ui.rows[0].pid = None;
            ui.rows[0].changed = true;
            assert_eq!(ui.notices().len(), 2, "error and warning both shown");
            let old = Instant::now().checked_sub(Duration::from_secs(16)).unwrap();
            for shown in ui.notice_since.borrow_mut().values_mut() {
                *shown = old;
            }
            let notices = ui.notices();
            assert_eq!(notices.len(), 1, "warning timed out");
            assert_eq!(notices[0].0, Color::Red, "error persists");
            assert!(notices[0].1.contains("Core stopped unexpectedly"));
            assert!(ui.next_notice_expiry().is_none());
        });
    }
    #[test]
    fn overview_layout_prioritizes_connections_and_adapts_to_width() {
        crate::i18n::with_language(crate::i18n::Language::English, || {
            let ui = live_sample();
            let text = screen(&render(&ui, 140, 40));
            assert_eq!(text.matches("Astra").count(), 1);
            assert!(text.contains("├─ 1 Overview"));
            assert!(text.contains("LIVE CONNECTIONS · 3/3"));
            assert!(text.contains("SNAPSHOT"));
            assert!(text.contains("●G proxy-00 · P"));
            assert!(text.contains("CONTROLS"));
            assert!(text.contains("S System proxy ●"));
            assert!(text.contains("N Network tools"));
            assert!(
                "▁▂▃▄▅▆▇█".chars().any(|glyph| text.contains(glyph)),
                "embedded traffic chart missing"
            );
            let order = ["j-cn-live-comet", "claude.ai", "data.bilibili.com"]
                .map(|host| text.find(host).unwrap());
            assert!(
                order.windows(2).all(|pair| pair[0] < pair[1]),
                "connections are not sorted by traffic: {order:?}"
            );

            let text = screen(&render(&ui, 100, 30));
            assert!(text.contains("PROXY GROUPS · 2"));
            assert!(!text.contains("SNAPSHOT"));
            assert!(text.contains("m Rule"));
            assert!(text.contains("S System proxy ●"));

            let text = screen(&render(&ui, 70, 30));
            assert!(text.contains("Astra · proxy-00 · ● Connected"));
            assert_eq!(text.matches("Connected").count(), 1);
            assert!(text.contains("├─ 1 Overview"));
            assert!(text.contains("DIRECT · Tcp"));
            assert!(text.contains("S proxy ●"));
            assert!(text.contains("A LAN ○"));
            assert!(!text.contains("SNAPSHOT"));
        });
    }
    #[test]
    fn header_rates_keep_columns_and_actions_stay_in_controls() {
        crate::i18n::with_language(crate::i18n::Language::English, || {
            for width in [70, 100, 140] {
                let mut ui = live_sample();
                let mut columns = None;
                for value in [0, 1_023, 1_048_575, 1 << 30, u64::MAX] {
                    ui.rates = [value, value / 2];
                    ui.direct_rates = [value / 3, value / 4];
                    let terminal = render(&ui, width, 30);
                    let buffer = terminal.backend().buffer();
                    let up: Vec<_> = (0..width)
                        .filter(|&x| buffer[(x, 1)].symbol() == "↑")
                        .collect();
                    let down: Vec<_> = (0..width)
                        .filter(|&x| buffer[(x, 2)].symbol() == "↓")
                        .collect();
                    assert_eq!(up.len(), 2, "both rate columns at width {width}");
                    assert_eq!(up, down, "upload and download columns align");
                    assert_eq!(columns.get_or_insert(up.clone()), &up);
                    let footer: String = (0..width).map(|x| buffer[(x, 28)].symbol()).collect();
                    assert!(footer.contains("F2") && footer.contains("F1"));

                    let controls_y = (0..30)
                        .find(|&y| {
                            (0..width)
                                .map(|x| buffer[(x, y)].symbol())
                                .collect::<String>()
                                .contains("CONTROLS")
                        })
                        .unwrap();
                    let hits = ui.hits.borrow();
                    let tools = hits
                        .iter()
                        .find(|(_, target)| matches!(target, Target::NetTools))
                        .unwrap()
                        .0;
                    let connect = hits
                        .iter()
                        .find(|(_, target)| matches!(target, Target::Connect))
                        .unwrap()
                        .0;
                    assert!(tools.y > controls_y);
                    assert!(tools.y < controls_y + ui.runtime_controls_height(width) - 1);
                    assert_eq!(tools.y, connect.y);
                    assert!(tools.right() < connect.x);
                    assert!(connect.right() >= width - 3 && connect.right() < width);
                }
                // Missing telemetry keeps the same columns as live values.
                let terminal = render(&sample(), width, 30);
                let buffer = terminal.backend().buffer();
                let up: Vec<_> = (0..width)
                    .filter(|&x| buffer[(x, 1)].symbol() == "↑")
                    .collect();
                assert_eq!(columns.unwrap(), up);
            }
        });
    }

    #[test]
    fn overview_expands_traffic_and_stays_stable_in_small_windows() {
        crate::i18n::with_language(crate::i18n::Language::English, || {
            let mut ui = live_sample();
            ui.key(event::KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE));
            let text = screen(&render(&ui, 140, 40));
            assert!(text.contains("Proxy traffic · z collapse"));
            assert!(text.contains("z collapse"));
            ui.key(event::KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE));
            assert!(!screen(&render(&ui, 140, 40)).contains("Traffic"));
            for (width, height) in [(1, 1), (8, 4), (20, 6), (60, 20), (90, 24)] {
                let _ = render(&ui, width, height);
            }
        });
    }
    #[test]
    fn narrow_overview_scrolls_from_connections_into_proxy_groups() {
        crate::i18n::with_language(crate::i18n::Language::English, || {
            use astra_model::runtime::Group;
            use astra_model::runtime::Member;
            let mut ui = live_sample();
            let mut snapshot = ui.live().unwrap().clone();
            for index in 0..10 {
                snapshot.groups.push(Group {
                    name: format!("GROUP-{index}"),
                    mode: "select".into(),
                    selected: Some(format!("node-{index}")),
                    testing: false,
                    members: vec![Member {
                        name: format!("node-{index}"),
                        delay_ms: Some(20 + index),
                        alive: Some(true),
                    }],
                });
            }
            ui.accept_runtime("proxy-00".into(), snapshot);
            let _ = render(&ui, 70, 30);
            let limit = ui.overview_item_count.get();
            assert_eq!(limit, 15);
            for _ in 0..limit {
                ui.key(event::KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
            }
            let text = screen(&render(&ui, 70, 30));
            assert!(
                text.contains("GROUP-9"),
                "proxy groups did not scroll: {text}"
            );
        });
    }
    #[test]
    fn overview_folds_shared_targets_and_renders_aggregate_traffic() {
        crate::i18n::with_language(crate::i18n::Language::English, || {
            for width in [140, 70] {
                let mut ui = live_sample();
                let mut snapshot = ui.live().unwrap().clone();
                let mut first = snapshot.connections[0].clone();
                first.id = 10;
                first.host = "chatgpt.com".into();
                first.port = 443;
                first.upload = 1024;
                first.download = 2048;
                let mut second = first.clone();
                second.id = 11;
                second.upload = 2048;
                second.download = 4096;
                snapshot.connections = vec![first, second];
                snapshot.active = 2;
                ui.accept_runtime("proxy-00".into(), snapshot);
                let text = screen(&render(&ui, width, 30));
                assert!(text.contains("▸ chatgpt.com:443 (2)"), "{text}");
                assert!(text.contains("3.0 KiB") && text.contains("6.0 KiB"));
                assert!(!ui
                    .hits
                    .borrow()
                    .iter()
                    .any(|(_, target)| matches!(target, Target::Connection(_))));
                let header = ui
                    .hits
                    .borrow()
                    .iter()
                    .find_map(|(area, target)| {
                        matches!(target, Target::ConnectionRow(_)).then_some(*area)
                    })
                    .unwrap();
                ui.mouse(click(header));
                assert_eq!(ui.page, OVERVIEW);
                let text = screen(&render(&ui, width, 30));
                assert!(text.contains("▾ chatgpt.com:443 (2)"), "{text}");
                for id in [10, 11] {
                    assert!(ui.hits.borrow().iter().any(
                        |(_, target)| matches!(target, Target::Connection(value) if *value == id)
                    ));
                }
                let header = ui
                    .hits
                    .borrow()
                    .iter()
                    .find_map(|(area, target)| {
                        matches!(target, Target::ConnectionRow(_)).then_some(*area)
                    })
                    .unwrap();
                ui.mouse(click(header));
                render(&ui, width, 30);
                assert!(!ui
                    .hits
                    .borrow()
                    .iter()
                    .any(|(_, target)| matches!(target, Target::Connection(_))));
            }
        });
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
    #[tokio::test]
    async fn overview_opens_network_tools_and_esc_returns_to_overview() {
        let mut ui = sample();
        let press = |ui: &mut Ui, code| ui.key(event::KeyEvent::new(code, KeyModifiers::NONE));
        let Input::Run(action, _) = press(&mut ui, KeyCode::Char('N')) else {
            panic!("N should open network tools")
        };
        assert!(matches!(&action, Action::Diagnostics(name) if name == "proxy-00"));
        ui.execute(std::path::Path::new("unused-overview.sock"), action, None)
            .await;
        let view = ui.diagnostics.as_ref().expect("network tools open");
        assert!(view.handles_escape(), "opens straight into the tools page");
        assert!(!ui.open_nettools, "the request flag is consumed");
        let text = crate::i18n::with_language(crate::i18n::Language::English, || {
            screen(&render(&ui, 120, 30))
        });
        assert!(
            text.contains("Network tools") && text.contains("Egress"),
            "{text}"
        );
        press(&mut ui, KeyCode::Esc);
        // The Overview card offers the same entry as a button.
        render(&ui, 140, 40);
        let button = ui
            .hits
            .borrow()
            .iter()
            .find(|(_, hit)| matches!(hit, Target::NetTools))
            .map(|(rect, _)| *rect)
            .expect("network tools button on Overview");
        let click = ui.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: button.x,
            row: button.y,
            modifiers: KeyModifiers::NONE,
        });
        assert!(matches!(click, Input::Run(Action::Diagnostics(_), _)) && ui.open_nettools);
        ui.open_nettools = false;
        assert!(ui.diagnostics.is_none());
        assert_eq!(ui.page, OVERVIEW);
        for live in [false, true] {
            let mut controls = if live { live_sample() } else { sample() };
            controls.focus.region = focus::Region::Controls;
            controls.focus.cursor = if live { 6 } else { 0 };
            assert!(matches!(
                press(&mut controls, KeyCode::Enter),
                Input::Run(Action::Diagnostics(_), _)
            ));
            assert!(controls.open_nettools);
            press(&mut controls, KeyCode::Right);
            assert!(matches!(
                press(&mut controls, KeyCode::Enter),
                Input::Run(Action::Stop(_), _)
            ));
        }
        // D still opens the node list, where n reaches the same tools.
        let Input::Run(action, _) = press(&mut ui, KeyCode::Char('D')) else {
            panic!()
        };
        ui.execute(std::path::Path::new("unused-overview.sock"), action, None)
            .await;
        assert!(!ui.diagnostics.as_ref().unwrap().handles_escape());
    }
}
