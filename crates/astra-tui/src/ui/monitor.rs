//! On-demand monitor. Dropping the view cancels its only in-flight request.
use super::*;
use astra_management::v2::MonitorSnapshot;
use ratatui::layout::Rect;
use std::collections::VecDeque;
use tokio::task::JoinHandle;

const LIMIT: usize = 120;
const INTERVAL: Duration = Duration::from_secs(1);

struct Sample {
    snapshot: MonitorSnapshot,
    rates: [Option<f64>; 2],
}

pub(super) struct View {
    name: String,
    samples: VecDeque<Sample>,
    baseline: Option<MonitorSnapshot>,
    pending: Option<JoinHandle<Result<MonitorSnapshot>>>,
    next: Instant,
    paused: bool,
    error: Option<String>,
    scroll: u16,
    max_scroll: std::cell::Cell<u16>,
}

impl Drop for View {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl View {
    pub(super) fn new(name: String) -> Self {
        Self {
            name,
            samples: VecDeque::new(),
            baseline: None,
            pending: None,
            next: Instant::now(),
            paused: false,
            error: None,
            scroll: 0,
            max_scroll: std::cell::Cell::new(0),
        }
    }

    fn cancel(&mut self) {
        if let Some(pending) = self.pending.take() {
            pending.abort();
        }
    }

    pub(super) fn key(&mut self, key: event::KeyEvent) {
        match key.code {
            KeyCode::Char(' ') => {
                self.paused = !self.paused;
                self.cancel();
                self.baseline = None;
                self.next = Instant::now();
            }
            KeyCode::Char('c') => {
                self.cancel();
                self.samples.clear();
                self.baseline = None;
                self.error = None;
                self.scroll = 0;
                self.next = Instant::now();
            }
            KeyCode::Down => self.scroll = self.scroll.saturating_add(1).min(self.max_scroll.get()),
            KeyCode::Up => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(10).min(self.max_scroll.get())
            }
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::Home => self.scroll = 0,
            KeyCode::End => self.scroll = self.max_scroll.get(),
            _ => {}
        }
    }

    pub(super) async fn poll(&mut self, socket: &std::path::Path) -> bool {
        let mut changed = false;
        if self.pending.as_ref().is_some_and(|task| task.is_finished()) {
            let result = self.pending.take().unwrap().await;
            match result {
                Ok(Ok(snapshot)) => self.accept(snapshot),
                result => {
                    self.error = Some(match result {
                        Ok(Err(error)) => format!("{error:#}"),
                        Err(error) => error.to_string(),
                        _ => unreachable!(),
                    });
                    self.baseline = None;
                }
            }
            self.next = Instant::now() + INTERVAL;
            changed = true;
        }
        if !self.paused && self.pending.is_none() && Instant::now() >= self.next {
            let socket = socket.to_owned();
            let name = self.name.clone();
            self.pending = Some(tokio::spawn(async move {
                let result = async {
                    let reply = tokio::time::timeout(
                        Duration::from_secs(5),
                        crate::protocol::call(&socket, Action::Monitor { name }),
                    )
                    .await
                    .context("Monitor request timed out")??;
                    if let Some(error) = reply.error {
                        anyhow::bail!(error);
                    }
                    serde_json::from_str(&reply.text).context("Invalid monitor response")
                }
                .await;
                wake();
                result
            }));
        }
        changed
    }

    fn accept(&mut self, snapshot: MonitorSnapshot) {
        if snapshot.instance != self.name {
            self.error = Some(t("Monitor response belongs to another instance").into());
            self.baseline = None;
            return;
        }
        if self.samples.back().is_some_and(|sample| {
            sample.snapshot.pid != snapshot.pid
                || number(&snapshot.uptime_secs) < number(&sample.snapshot.uptime_secs)
        }) {
            self.samples.clear();
            self.baseline = None;
        }
        self.error = snapshot.error.clone();
        let rates = self
            .baseline
            .as_ref()
            .map(|previous| rates(previous, &snapshot))
            .unwrap_or([None; 2]);
        self.baseline = if snapshot.error.is_none() && snapshot.pid.is_some() {
            Some(snapshot.clone())
        } else {
            None
        };
        self.samples.push_back(Sample { snapshot, rates });
        while self.samples.len() > LIMIT {
            self.samples.pop_front();
        }
    }

    pub(super) fn render(&self, f: &mut Frame<'_>, area: Rect) {
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
        self.max_scroll.set(max_scroll);
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
    }
}

fn number(value: &str) -> Option<u64> {
    value.parse().ok()
}

/// Completed includes successful and failed operations; cancelled work is
/// excluded from both duration and completion. Keep integer precision before
/// formatting milliseconds to three decimal places (whole microseconds).
fn mean_duration_ms(
    metric: &astra_model::monitor::Metric,
    metrics: &[astra_model::monitor::Metric],
) -> Option<String> {
    use astra_model::monitor::MetricKind;
    if metric.kind != MetricKind::Counter || metric.unit != "us" {
        return None;
    }
    let prefix = metric.name.strip_suffix(".duration_us")?;
    let completed_name = format!("{prefix}.completed");
    let completed = metrics.iter().find(|candidate| {
        candidate.name == completed_name
            && candidate.kind == MetricKind::Counter
            && candidate.unit == "count"
    })?;
    let count = completed.value.parse::<u128>().ok().filter(|n| *n > 0)?;
    let mean_us = metric.value.parse::<u128>().ok()? / count;
    Some(format!("{}.{:03} ms", mean_us / 1000, mean_us % 1000))
}

fn rates(previous: &MonitorSnapshot, current: &MonitorSnapshot) -> [Option<f64>; 2] {
    if previous.instance != current.instance
        || previous.pid != current.pid
        || current.pid.is_none()
        || previous.error.is_some()
        || current.error.is_some()
        || number(&current.uptime_secs) < number(&previous.uptime_secs)
    {
        return [None; 2];
    }
    let elapsed = number(&current.sampled_at_ms)
        .zip(number(&previous.sampled_at_ms))
        .and_then(|(now, before)| now.checked_sub(before))
        .filter(|ms| *ms > 0);
    let rate = |old: &Option<String>, new: &Option<String>| {
        let delta = number(new.as_deref()?)?.checked_sub(number(old.as_deref()?)?)?;
        Some(delta as f64 * 1000.0 / elapsed? as f64)
    };
    [
        rate(&previous.upload_bytes, &current.upload_bytes),
        rate(&previous.download_bytes, &current.download_bytes),
    ]
}

fn bytes(value: Option<f64>) -> String {
    let Some(mut value) = value.filter(|v| v.is_finite() && *v >= 0.0) else {
        return "—".into();
    };
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut i = 0;
    while value >= 1024.0 && i < units.len() - 1 {
        value /= 1024.0;
        i += 1;
    }
    format!("{value:.1} {}", units[i])
}

fn trend(values: &[Option<f64>], width: usize) -> String {
    let visible = &values[values.len().saturating_sub(width)..];
    let max = visible
        .iter()
        .flatten()
        .copied()
        .filter(|v| v.is_finite())
        .fold(0.0, f64::max);
    let levels = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    visible
        .iter()
        .map(|value| match value {
            Some(value) if value.is_finite() && *value >= 0.0 => {
                levels[if max > 0.0 {
                    (value / max * 7.0).round().clamp(0.0, 7.0) as usize
                } else {
                    0
                }]
            }
            _ => '·',
        })
        .collect()
}

#[cfg(test)]
#[path = "../test/ui_monitor_test.rs"]
mod tests;
