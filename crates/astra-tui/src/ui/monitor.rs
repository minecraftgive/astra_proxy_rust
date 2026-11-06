//! Terminal monitor view over the shared sampling model.
use super::*;
#[cfg(test)]
use astra_client::monitor::rates;
pub use astra_client::monitor::View;
use astra_client::monitor::{bytes, mean_duration_ms, number, trend, LIMIT};
#[cfg(test)]
use astra_management::v2::MonitorSnapshot;
use ratatui::layout::Rect;
pub(super) trait Render {
    fn render(&self, f: &mut Frame<'_>, area: Rect) -> u16;
}
impl Render for View {
    fn render(&self, f: &mut Frame<'_>, area: Rect) -> u16 {
        let muted = Style::default().fg(dashboard::MUTED);
        let mut lines = vec![
            Line::styled(
                t("Selected PID only; children excluded. CPU 100% = one logical core."),
                muted,
            ),
            Line::styled(
                t("Network counters cover this core's forwarded traffic, not the host network."),
                muted,
            ),
            Line::raw(""),
        ];
        if let Some(sample) = self.samples.back() {
            let s = &sample.snapshot;
            lines.push(Line::from(format!(
                "PID {}   {} {}   {} {}",
                s.pid
                    .map(|pid| pid.to_string())
                    .unwrap_or_else(|| "—".into()),
                t("Uptime"),
                number(&s.uptime_secs)
                    .map(shell::duration)
                    .unwrap_or_else(|| "—".into()),
                t("Sample time (ms)"),
                s.sampled_at_ms,
            )));
            lines.push(Line::from(format!(
                "CPU {}   RSS {}   {} {}",
                s.cpu_percent
                    .filter(|v| v.is_finite())
                    .map(|v| format!("{v:.1}%"))
                    .unwrap_or_else(|| "—".into()),
                bytes(s.memory_bytes.as_deref().and_then(number).map(|n| n as f64)),
                t("Virtual memory"),
                bytes(
                    s.virtual_memory_bytes
                        .as_deref()
                        .and_then(number)
                        .map(|n| n as f64)
                ),
            )));
            lines.push(Line::from(format!(
                "↑ {}/s   ↓ {}/s   {} {} / {} {}",
                bytes(sample.rates[0]),
                bytes(sample.rates[1]),
                t("Active"),
                s.active_connections
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "—".into()),
                s.total_connections.as_deref().unwrap_or("—"),
                t("Total"),
            )));
            lines.push(Line::from(format!(
                "{} ↑ {}   ↓ {}",
                t("Cumulative traffic"),
                bytes(s.upload_bytes.as_deref().and_then(number).map(|n| n as f64)),
                bytes(
                    s.download_bytes
                        .as_deref()
                        .and_then(number)
                        .map(|n| n as f64)
                ),
            )));
        } else {
            lines.push(Line::from(t("Waiting for monitor sample")));
        }
        lines.push(Line::raw(""));
        if let Some(error) = &self.error {
            lines.push(Line::styled(
                format!(
                    "{}: {error}",
                    t("Sample failed; displayed values may be stale")
                ),
                Style::default().fg(Color::Yellow),
            ));
        }
        let width = area.width.saturating_sub(13) as usize;
        lines.push(Line::styled(
            format!(
                "{} · {}/{} · {}",
                t("Recent samples"),
                self.samples.len(),
                LIMIT,
                t("Each row uses its own scale; · = unavailable")
            ),
            muted,
        ));
        for (label, values) in [
            (
                "CPU",
                self.samples
                    .iter()
                    .map(|s| s.snapshot.cpu_percent)
                    .collect::<Vec<_>>(),
            ),
            (
                "RSS",
                self.samples
                    .iter()
                    .map(|s| {
                        s.snapshot
                            .memory_bytes
                            .as_deref()
                            .and_then(number)
                            .map(|n| n as f64)
                    })
                    .collect(),
            ),
            ("↑ B/s", self.samples.iter().map(|s| s.rates[0]).collect()),
            ("↓ B/s", self.samples.iter().map(|s| s.rates[1]).collect()),
        ] {
            lines.push(Line::from(format!("{label:7} {}", trend(&values, width))));
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            t("— = unavailable or waiting for a second sample"),
            muted,
        ));
        lines.push(Line::raw(""));
        if let Some(internal) = self
            .samples
            .back()
            .and_then(|s| s.snapshot.internal.as_ref())
        {
            lines.push(Line::styled(
                format!(
                    "{} · {}",
                    t("Core internal metrics · epoch"),
                    internal.epoch
                ),
                Style::default().fg(Color::Cyan),
            ));
            for metric in &internal.metrics {
                let kind = match metric.kind {
                    astra_model::monitor::MetricKind::Counter => t("Counter"),
                    astra_model::monitor::MetricKind::Gauge => t("Gauge"),
                };
                let mean = if metric.name.ends_with(".duration_us") && metric.unit == "us" {
                    format!(
                        " · {} {}",
                        t("Mean since epoch"),
                        mean_duration_ms(metric, &internal.metrics).unwrap_or_else(|| "—".into())
                    )
                } else {
                    String::new()
                };
                lines.push(Line::from(format!(
                    "{} = {} {} · {kind} · {}{mean}",
                    metric.name, metric.value, metric.unit, metric.description,
                )));
            }
        } else {
            lines.push(Line::styled(t("Core internal metrics unavailable"), muted));
        }
        let state = if self.paused {
            t("Paused")
        } else {
            t("Sampling every second")
        };
        let content = Paragraph::new(lines).wrap(Wrap { trim: false });
        let max_scroll = content
            .line_count(area.width.saturating_sub(2).max(1))
            .saturating_sub(area.height.saturating_sub(2) as usize)
            .min(u16::MAX as usize) as u16;
        f.render_widget(Clear, area);
        f.render_widget(
            content.scroll((self.scroll.min(max_scroll), 0)).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(
                        "{} · {} · {state}",
                        t("Performance / memory / network monitor"),
                        self.name
                    ))
                    .title_bottom(t(
                        "Space pause/resume · c clear · ↑↓ / PgUp/Dn scroll · Esc / F9 close",
                    )),
            ),
            area,
        );
        max_scroll
    }
}

#[cfg(test)]
#[path = "../test/ui_monitor_test.rs"]
mod tests;
