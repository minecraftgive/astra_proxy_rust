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
mod tests {
    use super::*;

    fn snapshot(timestamp: u64, upload: u64) -> MonitorSnapshot {
        MonitorSnapshot {
            sampled_at_ms: timestamp.to_string(),
            instance: "proxy".into(),
            pid: Some(42),
            uptime_secs: "10".into(),
            cpu_percent: Some(12.0),
            memory_bytes: Some("4096".into()),
            virtual_memory_bytes: None,
            upload_bytes: Some(upload.to_string()),
            download_bytes: Some("0".into()),
            active_connections: Some(1),
            total_connections: Some("2".into()),
            error: None,
            internal: None,
        }
    }

    #[test]
    fn rates_preserve_large_counters_and_reject_reset_pid_clock_and_error() {
        let old = snapshot(1000, u64::MAX - 100);
        let mut new = snapshot(2000, u64::MAX - 50);
        assert_eq!(rates(&old, &new), [Some(50.0), Some(0.0)]);
        new.upload_bytes = Some("0".into());
        assert_eq!(rates(&old, &new)[0], None);
        new.pid = Some(43);
        assert_eq!(rates(&old, &new), [None; 2]);
        new = snapshot(1000, u64::MAX - 50);
        assert_eq!(rates(&old, &new), [None; 2]);
        new.sampled_at_ms = "500".into();
        assert_eq!(rates(&old, &new), [None; 2]);
        new = snapshot(2000, u64::MAX - 50);
        new.error = Some("unavailable".into());
        assert_eq!(rates(&old, &new), [None; 2]);
    }

    #[test]
    fn history_is_bounded_pause_resets_baseline_and_restart_clears_history() {
        let mut view = View::new("proxy".into());
        for i in 0..150 {
            view.accept(snapshot(i * 1000, i * 100));
        }
        assert_eq!(view.samples.len(), LIMIT);
        view.key(event::KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        assert!(view.paused);
        assert!(view.baseline.is_none());
        view.key(event::KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        view.accept(snapshot(150_000, 15_000));
        assert_eq!(view.samples.back().unwrap().rates, [None; 2]);
        let mut restarted = snapshot(151_000, 0);
        restarted.pid = Some(43);
        view.accept(restarted);
        assert_eq!(view.samples.len(), 1);
        view.key(event::KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        assert!(view.samples.is_empty());
        assert!(view.baseline.is_none());
    }

    #[test]
    fn unavailable_trend_values_are_gaps_not_zeroes() {
        assert_eq!(
            trend(&[None, Some(0.0), Some(10.0), Some(f64::NAN)], 4),
            "·▁█·"
        );
        assert_eq!(trend(&[Some(10.0)], 0), "");
    }

    #[test]
    fn mean_duration_preserves_integer_precision_and_requires_completed_work() {
        use astra_model::monitor::{Metric, MetricKind};
        let duration = Metric {
            sampled_at_ms: None,
            name: "dns.answer.duration_us".into(),
            value: "9007199254740993".into(),
            kind: MetricKind::Counter,
            unit: "us".into(),
            description: String::new(),
        };
        let mut completed = Metric {
            sampled_at_ms: None,
            name: "dns.answer.completed".into(),
            value: "3".into(),
            kind: MetricKind::Counter,
            unit: "count".into(),
            description: String::new(),
        };
        assert_eq!(
            mean_duration_ms(&duration, &[completed.clone()]).as_deref(),
            Some("3002399751580.331 ms")
        );
        completed.value = "0".into();
        assert!(mean_duration_ms(&duration, &[completed.clone()]).is_none());
        completed.value = "invalid".into();
        assert!(mean_duration_ms(&duration, &[completed]).is_none());
        assert!(mean_duration_ms(&duration, &[]).is_none());
    }

    #[tokio::test]
    async fn in_flight_requests_do_not_overlap_and_pause_or_drop_cancels_them() {
        let mut view = View::new("proxy".into());
        let pending = tokio::spawn(std::future::pending::<Result<MonitorSnapshot>>());
        let id = pending.id();
        let abort = pending.abort_handle();
        view.pending = Some(pending);
        assert!(!view.poll(std::path::Path::new("unused")).await);
        assert_eq!(view.pending.as_ref().unwrap().id(), id);
        view.key(event::KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        tokio::task::yield_now().await;
        assert!(abort.is_finished());
        assert!(!view.poll(std::path::Path::new("unused")).await);
        assert!(view.pending.is_none());
        let pending = tokio::spawn(std::future::pending::<Result<MonitorSnapshot>>());
        let abort = pending.abort_handle();
        view.pending = Some(pending);
        drop(view);
        tokio::task::yield_now().await;
        assert!(abort.is_finished());
    }

    #[test]
    fn monitor_renders_in_all_languages_and_small_terminals() {
        use crate::i18n::{with_language, Language};
        let mut view = View::new("proxy".into());
        let mut sample = snapshot(1000, 100);
        sample.internal = Some(astra_model::monitor::InternalSnapshot {
            epoch: "core-epoch".into(),
            metrics: vec![astra_model::monitor::Metric {
                sampled_at_ms: None,
                name: "dns.cache.hits".into(),
                value: "9007199254740993".into(),
                kind: astra_model::monitor::MetricKind::Counter,
                unit: "count".into(),
                description: "DNS 缓存命中".into(),
            }],
        });
        view.accept(sample);
        for language in [
            Language::English,
            Language::Simplified,
            Language::Traditional,
        ] {
            with_language(language, || {
                for (width, height) in [(120, 24), (60, 20), (20, 8), (8, 4)] {
                    let mut terminal =
                        Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
                    terminal.draw(|f| view.render(f, f.area())).unwrap();
                    if width == 120 {
                        let content = terminal
                            .backend()
                            .buffer()
                            .content
                            .iter()
                            .map(|cell| cell.symbol())
                            .collect::<String>();
                        assert!(content.contains("dns.cache.hits"));
                        assert!(content.contains("9007199254740993"));
                    }
                }
            });
        }
    }

    #[test]
    fn shortcut_captures_input_and_preserves_the_underlying_draft() {
        let mut ui = Ui::new(Session::default());
        ui.page = PROFILES;
        ui.rows.push(Row {
            instance: Instance {
                name: "proxy".into(),
                kind: Kind::Proxy,
                config: "proxy.toml".into(),
            },
            pid: Some(42),
            state: "running".into(),
            uptime: 10,
            changed: false,
        });
        ui.editor = Some(Editor::new("proxy".into(), "standalone=true\n".into()));
        ui.prompt(Prompt::AddName);
        ui.paste_prompt("pending name");
        let editor = serde_json::to_value(ui.editor.as_ref().unwrap()).unwrap();
        ui.key(event::KeyEvent::new(KeyCode::F(9), KeyModifiers::NONE));
        assert_eq!(ui.monitor.as_ref().unwrap().name, "proxy");
        ui.paste("must not reach the draft");
        ui.key(event::KeyEvent::new(
            KeyCode::Char('s'),
            KeyModifiers::CONTROL,
        ));
        ui.key(event::KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        assert!(ui.monitor.as_ref().unwrap().paused);
        ui.key(event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(ui.monitor.is_none());
        assert_eq!(ui.input, "pending name");
        assert_eq!(
            serde_json::to_value(ui.editor.as_ref().unwrap()).unwrap(),
            editor
        );
        ui.key(event::KeyEvent::new(KeyCode::F(9), KeyModifiers::NONE));
        ui.key(event::KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));
        assert!(ui.monitor.is_none());
        assert!(ui.palette.is_some());
    }
}
