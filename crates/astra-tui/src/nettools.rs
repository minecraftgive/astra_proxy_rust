use crate::{i18n::t, tr};
pub use astra_client::nettools::*;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::Line,
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
fn render_column(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    rows: Vec<OutputLine>,
    scroll: usize,
) {
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    // Wrap and clamp each column separately: a long report cannot push a
    // conclusion out of its column or change its scrollable row count.
    let lines = rows
        .into_iter()
        .flat_map(|row| {
            wrap_lines(vec![row.text], inner.width.max(1) as usize)
                .into_iter()
                .map(move |text| Line::styled(text, result_style(row.tone)))
        })
        .collect::<Vec<_>>();
    let height = inner.height as usize;
    let skip = lines.len().saturating_sub(height).saturating_sub(scroll);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(
            lines
                .into_iter()
                .skip(skip)
                .take(height)
                .collect::<Vec<_>>(),
        )
        .wrap(Wrap { trim: false }),
        inner,
    );
}
impl Render for View {
    fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let mut fields = self.fields();
        // Preserve result space in short terminals. Field navigation still uses
        // the full shared form, showing the newly focused field on each key.
        if !fields.is_empty() && area.height < fields.len() as u16 + 9 {
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
                Constraint::Length(2),
            ])
            .split(area);
        frame.render_widget(
            Paragraph::new(format!(
                "{}\n{}",
                tr!("Network tools · {}", clean(&self.name)),
                Section::ALL
                    .into_iter()
                    .map(|section| format!(
                        "{}{}",
                        if section == self.section { "●" } else { " " },
                        section.label()
                    ))
                    .collect::<Vec<_>>()
                    .join("  ")
            ))
            .block(Block::default().borders(Borders::BOTTOM)),
            parts[0],
        );
        let lines: Vec<Line> = fields
            .iter()
            .map(|field| {
                let label = t(field.label());
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

        if self.show_advanced {
            let columns =
                Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)])
                    .split(parts[2]);
            render_column(
                frame,
                columns[0],
                "结论",
                self.conclusion_rows(),
                self.scroll,
            );
            render_column(
                frame,
                columns[1],
                "原始报告 · Ctrl+D 收起",
                self.report_rows(),
                self.scroll,
            );
        } else {
            render_column(
                frame,
                parts[2],
                "结论 · Ctrl+D 原始报告",
                self.conclusion_rows(),
                self.scroll,
            );
        }
        frame.render_widget(
            Paragraph::new(t(
                "Ctrl+L 分区 · Ctrl+D 详情 · Ctrl+P 加入计划 · Ctrl+R 预设 · Ctrl+Y 复制 · Ctrl+E JSON\nEnter 运行/取消 · Ctrl+O 停止观察 · Ctrl+↑/↓ 滚动 · Esc 返回 · Ctrl+U 清空",
            )),
            parts[3],
        );
    }
}

#[cfg(test)]
#[path = "test/nettools_test.rs"]
mod tests;
