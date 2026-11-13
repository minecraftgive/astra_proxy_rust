//! A small in-page query form backed by the existing Agent diagnostic lifecycle.
use super::{
    dashboard::{card, Target, MUTED},
    *,
};
use astra_client::nettools::{OutputTone, RECORDS};
use ratatui::layout::Rect;

impl Ui {
    pub(super) fn dns_query_key(&mut self, key: event::KeyEvent) -> Input {
        let field = self.dns_state.query_field;
        match key.code {
            KeyCode::Esc => self.dns_state.query_open = false,
            KeyCode::Tab => self.dns_state.query_field = (field + 1) % 3,
            KeyCode::BackTab => self.dns_state.query_field = (field + 2) % 3,
            KeyCode::Char('x') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cancel_dns_query()
            }
            KeyCode::Char('u') if field == 0 && key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.dns_state.target.clear()
            }
            KeyCode::Backspace if field == 0 => {
                self.dns_state.target.pop();
            }
            KeyCode::Char(c)
                if field == 0
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if self.dns_state.target.len() < 253 && !c.is_control() {
                    self.dns_state.target.push(c);
                }
            }
            KeyCode::Left if field == 1 => {
                self.dns_state.record = (self.dns_state.record + RECORDS.len() - 1) % RECORDS.len()
            }
            KeyCode::Right | KeyCode::Char(' ') if field == 1 => {
                self.dns_state.record = (self.dns_state.record + 1) % RECORDS.len()
            }
            KeyCode::Enter => {
                if self.dns_query_running() {
                    self.cancel_dns_query();
                } else {
                    self.request_dns_query();
                }
            }
            KeyCode::PageUp => {
                self.dns_state.query_scroll = self.dns_state.query_scroll.saturating_sub(5)
            }
            KeyCode::PageDown => {
                self.dns_state.query_scroll = self
                    .dns_state
                    .query_scroll
                    .saturating_add(5)
                    .min(self.dns_query_max.get())
            }
            KeyCode::Up if field == 2 => {
                self.dns_state.query_scroll = self.dns_state.query_scroll.saturating_sub(1)
            }
            KeyCode::Down if field == 2 => {
                self.dns_state.query_scroll = self
                    .dns_state
                    .query_scroll
                    .saturating_add(1)
                    .min(self.dns_query_max.get())
            }
            _ => {}
        }
        Input::Idle
    }
    pub(super) fn dns_status_lines(&self) -> Vec<String> {
        let Some(resolver) = self.dns_resolver() else {
            return vec![t("Current DNS settings unavailable").into()];
        };
        let servers = |servers: &[astra_model::runtime::DnsServer]| {
            servers
                .iter()
                .map(|server| format!("{} {}", server.transport, server.address))
                .collect::<Vec<_>>()
                .join(" · ")
        };
        let mut lines = vec![
            format!(
                "{}: {} · IPv6 {} · {}: {}",
                t("Mode"),
                if resolver.fake_ip {
                    t("Fake IP")
                } else {
                    t("Real IP")
                },
                if resolver.ipv6 { t("On") } else { t("Off") },
                t("DNS policies"),
                resolver.policy_count
            ),
            format!("{}: {}", t("Upstream"), servers(&resolver.nameservers)),
        ];
        if !resolver.enabled {
            lines = vec![t("DNS is disabled for this profile").into()];
        }
        if !resolver.proxy_server_nameserver.is_empty() {
            lines.push(format!(
                "{}: {}",
                t("Proxy DNS (always direct)"),
                servers(&resolver.proxy_server_nameserver)
            ));
        }
        if !resolver.fallback.is_empty() {
            lines.push(format!(
                "{}: {}",
                t("Fallback DNS"),
                servers(&resolver.fallback)
            ));
        }
        lines.push(format!(
            "{}: {} · {}: {}",
            t("DNS listener"),
            if resolver.listen.is_empty() {
                "—".into()
            } else {
                resolver
                    .listen
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            },
            t("Respect rules"),
            if resolver.respect_rules {
                t("On")
            } else {
                t("Off")
            }
        ));
        lines
    }
    /// Returns the remaining area for the query history.
    pub(super) fn render_dns_tools(&self, f: &mut Frame<'_>, area: Rect) -> Rect {
        let status = self.dns_status_lines();
        let status_text = Paragraph::new(status.join("\n")).wrap(Wrap { trim: false });
        if self.dns_state.status_open {
            let block = card("Current DNS · ↑↓ scroll · Esc close");
            let inner = block.inner(area);
            let max = status_text
                .line_count(inner.width.max(1))
                .saturating_sub(inner.height as usize)
                .min(u16::MAX as usize) as u16;
            self.dns_status_max.set(max);
            f.render_widget(block, area);
            f.render_widget(
                status_text.scroll((self.dns_state.status_scroll.min(max), 0)),
                inner,
            );
            return Rect::default();
        }
        let requested = status_text.line_count(area.width.saturating_sub(2).max(1)) as u16 + 2;
        let status_height = requested.min((area.height / 3).max(3)).min(area.height);
        let query_height = if self.dns_state.query_open {
            if self.dns_state.query.is_some() || self.dns_state.query_error.is_some() {
                12
            } else {
                6
            }
        } else {
            1
        };
        let query_height = query_height.min(area.height.saturating_sub(status_height + 3));
        let parts = Layout::vertical([
            Constraint::Length(status_height),
            Constraint::Length(query_height),
            Constraint::Min(0),
        ])
        .split(area);
        f.render_widget(status_text.block(card("Current DNS · s details")), parts[0]);
        self.hit(parts[0], Target::DnsStatus);
        if self.dns_state.query_open {
            self.render_dns_query_form(f, parts[1]);
        } else {
            f.render_widget(
                Paragraph::new(Line::from(crate::theme::key_hints(t(
                    "n Query DNS · enter a domain and record type",
                ))))
                .style(Style::default().fg(Color::Cyan)),
                parts[1],
            );
            self.hit(parts[1], Target::DnsQueryField(0));
        }
        parts[2]
    }
    fn render_dns_query_form(&self, f: &mut Frame<'_>, area: Rect) {
        let block = card("Simple DNS query · current profile");
        let inner = block.inner(area);
        f.render_widget(block, area);
        let parts = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner);
        let field_style = |field| {
            if self.dns_state.query_field == field {
                crate::theme::selected()
            } else {
                Style::default()
            }
        };
        f.render_widget(
            Paragraph::new(format!(
                "{}: {}{}",
                t("Domain"),
                self.dns_state.target,
                if self.dns_state.query_field == 0 {
                    "▏"
                } else {
                    ""
                }
            ))
            .style(field_style(0)),
            parts[0],
        );
        self.hit(parts[0], Target::DnsQueryField(0));
        let record = format!(
            "{}: ◀ {} ▶",
            t("Record"),
            RECORDS[self.dns_state.record % RECORDS.len()]
        );
        let row = Layout::horizontal([
            Constraint::Length(24.min(parts[1].width)),
            Constraint::Min(0),
        ])
        .split(parts[1]);
        f.render_widget(Paragraph::new(record).style(field_style(1)), row[0]);
        f.render_widget(
            Paragraph::new(if self.dns_query_running() {
                t("Cancel query")
            } else {
                t("Run query")
            })
            .style(field_style(2)),
            row[1],
        );
        self.hit(row[0], Target::DnsQueryField(1));
        self.hit(row[1], Target::DnsQueryRun);
        f.render_widget(
            Paragraph::new(t(
                "Tab field · ←→ type · Enter run/cancel · PgUp/PgDn results · Ctrl-D details · Esc close",
            ))
            .style(Style::default().fg(MUTED)),
            parts[2],
        );
        let mut lines = Vec::new();
        if let Some(error) = &self.dns_state.query_error {
            lines.push(Line::styled(error.clone(), Style::default().fg(Color::Red)));
        }
        if let Some(view) = &self.dns_state.query {
            lines.push(Line::styled(
                format!("{} {} → {}", view.record, view.target, view.server),
                Style::default().fg(Color::Cyan),
            ));
            lines.extend(self.dns_query_output().into_iter().map(|row| {
                Line::styled(
                    row.text,
                    Style::default().fg(match row.tone {
                        OutputTone::Success => Color::Green,
                        OutputTone::Warning => Color::Yellow,
                        OutputTone::Error => Color::Red,
                        OutputTone::Info => Color::Cyan,
                        OutputTone::Muted => MUTED,
                    }),
                )
            }));
        } else if self.dns_query_running() {
            lines.push(Line::raw(t("Testing…")));
        }
        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
        let max = paragraph
            .line_count(parts[3].width.max(1))
            .saturating_sub(parts[3].height as usize)
            .min(u16::MAX as usize) as u16;
        self.dns_query_max.set(max);
        f.render_widget(
            paragraph.scroll((self.dns_state.query_scroll.min(max), 0)),
            parts[3],
        );
    }
}
