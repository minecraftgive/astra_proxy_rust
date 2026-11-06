use crate::{i18n::t, tr};
pub use astra_client::nettools::*;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders, Paragraph},
    Frame,
};
pub trait Render {
    fn render(&self, frame: &mut Frame<'_>, area: Rect);
}
impl Render for View {
    fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let fields = self.fields();
        let parts = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Length(fields.len() as u16 + 1),
                Constraint::Min(1),
                Constraint::Length(2),
            ])
            .split(area);
        frame.render_widget(
            Paragraph::new(tr!("Network tools · {}", clean(&self.name)))
                .block(Block::default().borders(Borders::BOTTOM)),
            parts[0],
        );
        let lines: Vec<Line> = fields
            .iter()
            .map(|field| {
                let (label, value) = match field {
                    Field::Egress => (
                        t("Egress"),
                        format!(
                            "‹ {} ›  {}/{}{}",
                            egress_label(&self.egresses[self.egress]),
                            self.egress + 1,
                            self.egresses.len(),
                            if self.egress_query.is_empty() {
                                String::new()
                            } else {
                                format!("  /{}", self.egress_query)
                            }
                        ),
                    ),
                    Field::Kind => (t("Test"), format!("‹ {} ›", t(self.kind.label()))),
                    Field::Target => (
                        t("Target"),
                        format!(
                            "{}{}",
                            self.target,
                            if self.target.is_empty() {
                                match self.kind {
                                    Kind::Tcp => "host:port",
                                    Kind::Dns => "example.com",
                                    Kind::Icmp => "host / IP",
                                }
                            } else {
                                ""
                            }
                        ),
                    ),
                    Field::Record => (t("Record"), format!("‹ {} ›", self.record)),
                    Field::Server => (
                        t("DNS server"),
                        if self.server.is_empty() {
                            "1.1.1.1 · udp:// tcp:// tls://".into()
                        } else {
                            self.server.clone()
                        },
                    ),
                    Field::Count => (t("Count"), format!("‹ {} ›", self.count)),
                };
                let selected = *field == self.field;
                Line::styled(
                    format!(
                        "{} {:<12} {}",
                        if selected { ">" } else { " " },
                        label,
                        clean(&value)
                    ),
                    if selected {
                        crate::theme::selected()
                    } else {
                        Style::default()
                    },
                )
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), parts[1]);

        let mut output: Vec<Line> = Vec::new();
        if let Some(note) = &self.note {
            output.push(Line::styled(
                clean(note),
                Style::default().fg(Color::DarkGray),
            ));
        }
        for attempt in &self.attempts {
            output.push(match (&attempt.error, attempt.rtt_ms) {
                (None, Some(ms)) => Line::styled(
                    format!(
                        "#{:<3} {:>9.2} ms  {}",
                        attempt.seq,
                        ms,
                        clean(&attempt.detail)
                    ),
                    Style::default().fg(Color::Green),
                ),
                (error, _) => Line::styled(
                    format!(
                        "#{:<3} {:>12}  {}",
                        attempt.seq,
                        t("failed"),
                        clean(error.as_deref().unwrap_or_default())
                    ),
                    Style::default().fg(Color::Red),
                ),
            });
        }
        if self.running() {
            output.push(Line::raw(t("Testing…")));
        }
        if let Some(summary) = &self.summary {
            let stats = match (summary.min_ms, summary.avg_ms, summary.max_ms) {
                (Some(min), Some(avg), Some(max)) => {
                    format!(" · min/avg/max {min:.2}/{avg:.2}/{max:.2} ms")
                }
                _ => String::new(),
            };
            output.push(Line::styled(
                format!(
                    "{}{stats}",
                    tr!(
                        "{} sent · {} received · {}% loss",
                        summary.sent,
                        summary.received,
                        summary.loss_percent()
                    )
                ),
                Style::default().fg(Color::Yellow),
            ));
        }
        if let Some(error) = &self.error {
            output.push(Line::styled(clean(error), Style::default().fg(Color::Red)));
        }
        if output.is_empty() {
            output.push(Line::raw(t(
                "Pick an egress (system route, adapter or outbound), then press Enter",
            )));
        }
        // Follow the newest lines.
        let height = parts[2].height.saturating_sub(1) as usize;
        let skip = output.len().saturating_sub(height);
        frame.render_widget(
            Paragraph::new(output.into_iter().skip(skip).collect::<Vec<_>>())
                .block(Block::default().borders(Borders::TOP).title(t("Results"))),
            parts[2],
        );
        frame.render_widget(
            Paragraph::new(t(
                "↑↓ field · ←→ choose · type to edit / find egress · Ctrl-U clear\nEnter run / stop · Esc back to node tests",
            )),
            parts[3],
        );
    }
}

#[cfg(test)]
#[path = "test/nettools_test.rs"]
mod tests;
