use crate::{i18n::t, tr};
pub use astra_client::nettools::*;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};
pub trait Render {
    fn render(&self, frame: &mut Frame<'_>, area: Rect);
}
fn result_style(tone: OutputTone) -> Style {
    Style::default().fg(match tone {
        OutputTone::Success => Color::Green,
        OutputTone::Warning => Color::Yellow,
        OutputTone::Error => Color::Red,
        OutputTone::Info => Color::Cyan,
        OutputTone::Muted => crate::theme::MUTED,
    })
}
fn chain_line(chain: &LiveChain) -> Line<'static> {
    let mut spans = vec![Span::styled(
        "链路 ",
        Style::default().fg(crate::theme::MUTED),
    )];
    for (index, (label, tone)) in chain.steps().into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(
                " › ",
                Style::default().fg(crate::theme::MUTED),
            ));
        }
        spans.push(Span::styled(
            clean(&label),
            match tone {
                "warning" => Style::default().fg(Color::Yellow),
                "muted" => Style::default().fg(crate::theme::MUTED),
                _ => Style::default().add_modifier(Modifier::BOLD),
            },
        ));
    }
    Line::from(spans)
}
impl Render for View {
    fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let mut fields = self.fields();
        // Preserve result space in short terminals. Field navigation still uses
        // the full shared form, showing the newly focused field on each key.
        if !fields.is_empty() && area.height < fields.len() as u16 + 10 {
            fields = vec![fields
                .iter()
                .copied()
                .find(|field| *field == self.field)
                .unwrap_or(fields[0])];
        }
        let parts = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(fields.len() as u16 + 1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(area);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(tr!("Network tools · {}", clean(&self.name))),
                chain_line(&self.live_chain()),
            ])
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::BOTTOM)),
            parts[0],
        );
        let lines: Vec<Line> = fields
            .iter()
            .map(|field| {
                let mut value = self.field_value(*field);
                if value.is_empty() {
                    value = self.field_hint(*field).into();
                }
                if *field == Field::Egress && !self.egress_query.is_empty() {
                    value.push_str(&format!("  /{}", self.egress_query));
                }
                if !field.editable() {
                    value = format!("‹ {value} ›");
                }
                let selected = *field == self.field;
                Line::styled(
                    format!(
                        "{} {:<12} {}",
                        if selected { ">" } else { " " },
                        t(field.label()),
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

        let block = Block::default().borders(Borders::ALL).title(t("Result"));
        let inner = block.inner(parts[2]);
        let width = inner.width.max(1) as usize;
        let mut rows: Vec<Line> = vec![];
        for strip in self.path_strips() {
            for row in &strip.rows {
                for text in wrap_lines(vec![row.text()], width) {
                    rows.push(Line::styled(text, Style::default().fg(Color::Cyan)));
                }
            }
            for attachment in &strip.attachments {
                rows.push(Line::styled(
                    format!("旁注：{}", clean(attachment)),
                    Style::default().fg(crate::theme::MUTED),
                ));
            }
        }
        for row in self.result_rows() {
            let style = result_style(row.tone);
            for text in wrap_lines(vec![row.text], width) {
                rows.push(Line::styled(text, style));
            }
        }
        let height = inner.height as usize;
        let skip = rows
            .len()
            .saturating_sub(height)
            .saturating_sub(self.scroll);
        frame.render_widget(block, parts[2]);
        frame.render_widget(
            Paragraph::new(rows.into_iter().skip(skip).take(height).collect::<Vec<_>>()),
            inner,
        );
        frame.render_widget(
            Paragraph::new(t(
                "Enter 运行/取消 · ←→ 切换 · Ctrl+Y 复制 · Ctrl+E JSON · Ctrl+↑/↓ 滚动 · Esc 返回",
            ))
            .style(Style::default().fg(crate::theme::MUTED)),
            parts[3],
        );
    }
}

#[cfg(test)]
#[path = "test/nettools_test.rs"]
mod tests;
