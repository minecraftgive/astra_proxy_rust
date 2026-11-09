//! Recent resolver activity, with the same live-list affordances as connections.
use super::{
    dashboard::{card, Target, MUTED},
    *,
};
use astra_model::runtime::DnsQuery;
use ratatui::layout::Rect;

fn time(ms: u64) -> String {
    chrono::DateTime::from_timestamp_millis(ms.min(i64::MAX as u64) as i64)
        .map(|at| {
            at.with_timezone(&chrono::Local)
                .format("%H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "—".into())
}
fn source(row: &DnsQuery) -> &str {
    t(match row.source.as_str() {
        "cache" => "Cache",
        "fake_ip" => "Fake IP",
        "ipv6_disabled" => "IPv6 disabled",
        _ => "Upstream",
    })
}
fn color(row: &DnsQuery) -> Color {
    match row.status.as_str() {
        "NoError" => Color::Green,
        "NXDomain" => Color::Yellow,
        _ => Color::Red,
    }
}
fn fit(value: &str, width: usize) -> String {
    astra_tui_kit::text::truncate_cells(value, width, "…")
}
fn pad(value: &str, width: usize) -> String {
    astra_tui_kit::text::pad_cells(&fit(value, width), width)
}

impl Ui {
    pub(super) fn dns_key(&mut self, key: event::KeyEvent) -> Option<Input> {
        if self.dns_state.status_open {
            match key.code {
                KeyCode::Esc | KeyCode::Char('s') => self.dns_state.status_open = false,
                KeyCode::Up => {
                    self.dns_state.status_scroll = self.dns_state.status_scroll.saturating_sub(1)
                }
                KeyCode::Down => {
                    self.dns_state.status_scroll = self
                        .dns_state
                        .status_scroll
                        .saturating_add(1)
                        .min(self.dns_status_max.get())
                }
                KeyCode::PageUp => {
                    self.dns_state.status_scroll = self.dns_state.status_scroll.saturating_sub(8)
                }
                KeyCode::PageDown => {
                    self.dns_state.status_scroll = self
                        .dns_state
                        .status_scroll
                        .saturating_add(8)
                        .min(self.dns_status_max.get())
                }
                _ => {}
            }
            return Some(Input::Idle);
        }
        if key.code == KeyCode::Char('s')
            && key.modifiers.is_empty()
            && !self.dns_state.filtering
            && !self.dns_state.query_open
        {
            self.dns_state.status_open = true;
            return Some(Input::Idle);
        }
        if self.dns_state.query_open {
            return Some(self.dns_query_key(key));
        }
        if key.code == KeyCode::Char('n') && key.modifiers.is_empty() && !self.dns_state.filtering {
            self.open_dns_query();
            return Some(Input::Idle);
        }
        if self.dns_state.filtering {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => self.dns_state.filtering = false,
                KeyCode::Backspace => {
                    self.dns_state.filter.pop();
                    self.dns_state.selected = None;
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.dns_state.filter.clear();
                    self.dns_state.selected = None;
                }
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    self.dns_state.filter.push(c);
                    self.dns_state.selected = None;
                }
                _ => {}
            }
            return Some(Input::Idle);
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('f') {
                self.dns_state.filtering = true;
                self.dns_state.detail = false;
                return Some(Input::Idle);
            }
            return None;
        }
        let code = if self.vim_keys {
            match key.code {
                KeyCode::Char('j') => KeyCode::Down,
                KeyCode::Char('k') => KeyCode::Up,
                KeyCode::Char('g') => KeyCode::Home,
                KeyCode::Char('G') => KeyCode::End,
                code => code,
            }
        } else {
            key.code
        };
        if self.dns_state.detail {
            match code {
                KeyCode::Esc | KeyCode::Enter => self.dns_state.detail = false,
                KeyCode::Char(' ') => self.toggle_dns_pause(),
                KeyCode::Char('/') => {
                    self.dns_state.filtering = true;
                    self.dns_state.detail = false;
                }
                KeyCode::Up => {
                    self.dns_state.detail_scroll = self.dns_state.detail_scroll.saturating_sub(1)
                }
                KeyCode::Down => {
                    self.dns_state.detail_scroll = self
                        .dns_state
                        .detail_scroll
                        .saturating_add(1)
                        .min(self.dns_detail_max.get())
                }
                KeyCode::PageDown => {
                    self.dns_state.detail_scroll = self
                        .dns_state
                        .detail_scroll
                        .saturating_add(8)
                        .min(self.dns_detail_max.get())
                }
                KeyCode::PageUp => {
                    self.dns_state.detail_scroll = self.dns_state.detail_scroll.saturating_sub(8)
                }
                KeyCode::Home => self.dns_state.detail_scroll = 0,
                KeyCode::End => self.dns_state.detail_scroll = self.dns_detail_max.get(),
                _ => return None,
            }
            return Some(Input::Idle);
        }
        if self.focus.region != focus::Region::Main {
            return None;
        }
        let index = self.dns_selected_index();
        let last = self.dns_rows().len().saturating_sub(1);
        let step = self.dns_viewport.get().max(1);
        match code {
            KeyCode::Char('/') => self.dns_state.filtering = true,
            KeyCode::Char(' ') => self.toggle_dns_pause(),
            KeyCode::Up => self.select_dns(index.saturating_sub(1)),
            KeyCode::Down => self.select_dns((index + 1).min(last)),
            KeyCode::PageUp => self.select_dns(index.saturating_sub(step)),
            KeyCode::PageDown => self.select_dns(index.saturating_add(step).min(last)),
            KeyCode::Home => self.select_dns(0),
            KeyCode::End => self.select_dns(last),
            KeyCode::Enter if !self.dns_rows().is_empty() => {
                self.select_dns(index);
                self.open_dns_detail();
            }
            KeyCode::Esc if !self.dns_state.filter.is_empty() => {
                self.dns_state.filter.clear();
                self.dns_state.selected = None;
            }
            _ => return None,
        }
        Some(Input::Idle)
    }
    fn dns_empty(&self) -> &'static str {
        if self.live().is_none() {
            t("DNS queries appear after connecting.")
        } else if self.dns_log().is_none() {
            t("DNS history unavailable · update the proxy core")
        } else if !self.dns_state.filter.is_empty() {
            t("No matching DNS queries")
        } else {
            t("No DNS queries yet")
        }
    }
    pub(super) fn render_dns_preview(&self, f: &mut Frame<'_>, area: Rect) {
        let log = self.dns_live();
        let title = format!(
            "{} · {} · 4",
            t("DNS queries"),
            log.map_or(0, |log| log.records.len())
        );
        let block = card(&title);
        let inner = block.inner(area);
        f.render_widget(block, area);
        self.hit(area, Target::Page(shell::DNS));
        let Some(log) = log else {
            f.render_widget(
                Paragraph::new(if self.live().is_none() {
                    t("DNS queries appear after connecting.")
                } else {
                    t("DNS history unavailable · update the proxy core")
                })
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(MUTED)),
                inner,
            );
            return;
        };
        if log.records.is_empty() {
            f.render_widget(
                Paragraph::new(t("No DNS queries yet")).style(Style::default().fg(MUTED)),
                inner,
            );
            return;
        }
        let count = inner.height.saturating_sub(1) as usize;
        let width = inner.width as usize;
        for (index, row) in log.records.iter().take(count).enumerate() {
            let rect = Rect::new(inner.x, inner.y + index as u16, inner.width, 1);
            let domain_width = width.saturating_sub(20).max(1);
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::raw(format!("{} ", pad(&row.domain, domain_width))),
                    Span::styled(
                        format!("{} {}", pad(&row.query_type, 5), fit(&row.status, 12)),
                        Style::default().fg(color(row)),
                    ),
                ])),
                rect,
            );
            self.hit(rect, Target::DnsQuery(row.resolver.clone(), row.id));
        }
        if inner.height > 0 {
            let health = self.runtime_health();
            let label = if health.is_empty() {
                t("4 / click to open · latest queries")
            } else {
                &health
            };
            f.render_widget(
                Paragraph::new(fit(label, width)).style(Style::default().fg(MUTED)),
                Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
            );
        }
    }
    pub(super) fn render_dns(&self, f: &mut Frame<'_>, area: Rect) {
        let rows = self.dns_rows();
        let title = format!(
            "{} · {}/{}{}{}",
            t("DNS queries"),
            rows.len(),
            self.dns_log().map_or(0, |log| log.records.len()),
            if self.dns_state.frozen.is_some() {
                format!(" · {}", t("Paused"))
            } else {
                String::new()
            },
            if self.dns_log().is_some_and(|log| log.truncated) {
                format!(" · {}", t("Latest 128 queries"))
            } else {
                String::new()
            }
        );
        let block = self.focused_card(&title, focus::Region::Main);
        let inner = block.inner(area);
        f.render_widget(block, area);
        let inner = self.render_dns_tools(f, inner);
        if self.dns_state.status_open {
            return;
        }
        let bands = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(inner);
        f.render_widget(
            Paragraph::new(format!(
                "{}{} {}  {}",
                t("Filter"),
                if self.dns_state.filtering { ">" } else { ":" },
                self.dns_state.filter,
                self.runtime_health()
            ))
            .style(Style::default().fg(MUTED)),
            bands[0],
        );
        f.render_widget(
            Paragraph::new(Line::from(crate::theme::key_hints(t(
                "↑↓ select · Enter details · / filter · Space pause · Esc back",
            )))),
            bands[2],
        );
        if rows.is_empty() && !self.dns_state.detail {
            f.render_widget(
                Paragraph::new(self.dns_empty())
                    .wrap(Wrap { trim: false })
                    .style(Style::default().fg(MUTED)),
                bands[1],
            );
            return;
        }
        let index = self.dns_selected_index().min(rows.len().saturating_sub(1));
        let (list, detail) = if self.dns_state.detail {
            if bands[1].height < 12 || bands[1].width < 70 {
                (Rect::default(), Some(bands[1]))
            } else {
                let parts =
                    Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)])
                        .split(bands[1]);
                (parts[0], Some(parts[1]))
            }
        } else {
            (bands[1], None)
        };
        let wide = list.width >= 90;
        let row_height = if wide { 1 } else { 2 };
        let capacity = (list.height as usize / row_height).max(1);
        self.dns_viewport.set(capacity);
        let start = index.saturating_sub(capacity - 1);
        for (offset, row) in rows.iter().skip(start).take(capacity).enumerate() {
            if list.height == 0 {
                break;
            }
            let width = list.width as usize;
            let summary = format!(
                "{} {} · {} · {:.2} ms",
                row.query_type,
                row.status,
                source(row),
                row.duration_us as f64 / 1000.0
            );
            let lines = if wide {
                vec![Line::from(vec![
                    Span::styled(
                        format!("{} ", time(row.timestamp_ms)),
                        Style::default().fg(MUTED),
                    ),
                    Span::raw(pad(&row.domain, width.saturating_sub(58))),
                    Span::styled(format!(" {summary}"), Style::default().fg(color(row))),
                ])]
            } else {
                vec![
                    Line::raw(fit(
                        &format!("{} {}", time(row.timestamp_ms), row.domain),
                        width,
                    )),
                    Line::styled(fit(&summary, width), Style::default().fg(color(row))),
                ]
            };
            let rect = Rect::new(
                list.x,
                list.y + (offset * row_height) as u16,
                list.width,
                row_height as u16,
            )
            .intersection(list);
            f.render_widget(
                Paragraph::new(lines).style(if start + offset == index {
                    crate::theme::selected()
                } else {
                    Style::default()
                }),
                rect,
            );
            self.hit(rect, Target::DnsQuery(row.resolver.clone(), row.id));
        }
        if let Some((area, row)) = detail.zip(
            self.dns_state
                .detail_record
                .as_ref()
                .or_else(|| rows.get(index).copied()),
        ) {
            let block = card("DNS query details · Esc close");
            let inner = block.inner(area);
            let mut lines = vec![
                Line::raw(format!(
                    "{} · {} · {}",
                    row.domain,
                    row.query_type,
                    time(row.timestamp_ms)
                )),
                Line::styled(
                    format!(
                        "{} · {} · {:.2} ms",
                        row.status,
                        source(row),
                        row.duration_us as f64 / 1000.0
                    ),
                    Style::default().fg(color(row)),
                ),
            ];
            if let Some(upstream) = &row.upstream {
                lines.push(Line::raw(format!("{}: {upstream}", t("Upstream"))));
            }
            if let Some(error) = &row.error {
                lines.push(Line::styled(error.clone(), Style::default().fg(Color::Red)));
            }
            lines.push(Line::styled(t("Answers"), Style::default().fg(MUTED)));
            if row.answers.is_empty() {
                lines.push(Line::raw("—"));
            }
            lines.extend(row.answers.iter().cloned().map(Line::raw));
            if row.answers_truncated {
                lines.push(Line::styled(
                    t("Answer preview truncated"),
                    Style::default().fg(Color::Yellow),
                ));
            }
            let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
            let max = paragraph
                .line_count(inner.width.max(1))
                .saturating_sub(inner.height as usize)
                .min(u16::MAX as usize) as u16;
            self.dns_detail_max.set(max);
            f.render_widget(block, area);
            f.render_widget(
                paragraph.scroll((self.dns_state.detail_scroll.min(max), 0)),
                inner,
            );
        }
    }
}
