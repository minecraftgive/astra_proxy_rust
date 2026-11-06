use crate::nettools::Render as _;
use crate::{i18n::t, tr};
pub use astra_client::diagnostics::*;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
    Frame,
};
use std::time::Instant;
pub trait Render {
    fn render(&self, frame: &mut Frame<'_>, area: Rect);
}
impl Render for View {
    fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        if let Some(tools) = &self.tools {
            frame.render_widget(ratatui::widgets::Clear, area);
            tools.render(frame, area);
            return;
        }
        let parts = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(1),
                Constraint::Length(3),
                Constraint::Length(2),
            ])
            .split(area);
        frame.render_widget(
            Paragraph::new(tr!("Connectivity / Exit IP · {}", clean(&self.name))).block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .title(t("Node tests")),
            ),
            parts[0],
        );
        let visible = parts[1].height.saturating_sub(1).max(1) as usize;
        let offset = self.cursor.saturating_sub(visible.saturating_sub(1));
        let rows = self
            .targets
            .iter()
            .enumerate()
            .skip(offset)
            .take(visible)
            .map(|(index, tag)| {
                let probe = self
                    .report
                    .as_ref()
                    .and_then(|r| r.probes.iter().find(|p| &p.outbound == tag));
                let name = format!(
                    "{} {} {}",
                    if index == self.cursor { ">" } else { " " },
                    if self.checked.contains(tag) {
                        "[x]"
                    } else {
                        "[ ]"
                    },
                    clean(tag)
                );
                let latency = probe
                    .and_then(|p| p.connect_ms)
                    .or_else(|| self.tcp.get(tag).copied())
                    .map(|ms| format!("{ms} ms"))
                    .unwrap_or_else(|| "—".into());
                let state = if let Some(p) = probe {
                    t(if p.connect_ms.is_some() {
                        "Connected"
                    } else {
                        "Connection failed"
                    })
                } else if self.receiver.is_some() && !self.loading && self.submitted.contains(tag) {
                    if self.tcp.contains_key(tag) {
                        t("Querying exit IP…")
                    } else if self.active.contains(tag) {
                        t("Testing…")
                    } else {
                        t("Queued")
                    }
                } else {
                    "—"
                };
                let ip = probe
                    .and_then(|p| p.ip)
                    .map(|ip| ip.to_string())
                    .unwrap_or_else(|| "—".into());
                Row::new([
                    Cell::from(name),
                    Cell::from(latency),
                    Cell::from(state),
                    Cell::from(ip),
                ])
                .height(1)
                .style(if index == self.cursor {
                    crate::theme::selected()
                } else {
                    Style::default()
                })
            });
        let table = Table::new(
            rows,
            [
                Constraint::Percentage(42),
                Constraint::Length(12),
                Constraint::Length(16),
                Constraint::Min(10),
            ],
        )
        .header(
            Row::new([t("Node"), t("TCP latency"), t("Connectivity"), t("Exit IP")])
                .style(Style::default().fg(Color::Yellow)),
        );
        frame.render_widget(table, parts[1]);
        let completed = self.report.as_ref().map_or(0, |r| r.probes.len());
        let progress = self.started.map(|start| {
            tr!(
                "Progress {}/{} · Running {} · Queued {} · {}s",
                completed,
                self.submitted.len(),
                self.active.len(),
                self.submitted
                    .len()
                    .saturating_sub(completed + self.active.len()),
                self.finished
                    .unwrap_or_else(Instant::now)
                    .duration_since(start)
                    .as_secs()
            )
        });
        let status = if let Some(error) = &self.error {
            format!("{}\n{}", progress.as_deref().unwrap_or(""), clean(error))
        } else if self.loading {
            t("Loading nodes…").into()
        } else {
            let note = self
                .report
                .as_ref()
                .map(|r| {
                    t(if r.running_config {
                        "Using running configuration"
                    } else {
                        "Using saved configuration · instance stopped"
                    })
                })
                .unwrap_or(t("Select nodes, then press Enter to test"));
            let error = self
                .targets
                .get(self.cursor)
                .and_then(|tag| {
                    self.report
                        .as_ref()?
                        .probes
                        .iter()
                        .find(|p| &p.outbound == tag)
                })
                .and_then(|p| p.error.as_deref())
                .map(clean)
                .unwrap_or_default();
            format!(
                "{}\n{error}",
                progress.unwrap_or_else(|| format!(
                    "{note}  ({}/{})",
                    self.checked.len(),
                    self.targets.len()
                ))
            )
        };
        frame.render_widget(
            Paragraph::new(status).block(Block::default().borders(Borders::TOP)),
            parts[2],
        );
        frame.render_widget(
            Paragraph::new(t(
                "↑↓ select · Space toggle · a all/none · c clear\nEnter/t test selected · x stop · n network tools · Esc back",
            )),
            parts[3],
        );
    }
}

#[cfg(test)]
#[path = "test/diagnostics_test.rs"]
mod tests;
