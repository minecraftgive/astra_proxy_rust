//! Rendering-free monitor history, sampling and keyboard controls.
use crate::{i18n::t, input::KeyCode, protocol::Action};
use anyhow::{Context, Result};
use astra_management::v2::MonitorSnapshot;
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};
use tokio::task::JoinHandle;
pub const LIMIT: usize = 120;
pub const INTERVAL: Duration = Duration::from_secs(1);

pub struct Sample {
    pub snapshot: MonitorSnapshot,
    pub rates: [Option<f64>; 2],
}

pub struct View {
    pub waker: crate::waker::Waker,
    pub name: String,
    pub samples: VecDeque<Sample>,
    pub baseline: Option<MonitorSnapshot>,
    pub pending: Option<JoinHandle<Result<MonitorSnapshot>>>,
    pub next: Instant,
    pub paused: bool,
    pub error: Option<String>,
    pub scroll: u16,
    pub max_scroll: u16,
}

impl Drop for View {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl View {
    pub fn new(name: String) -> Self {
        Self {
            name,
            waker: Default::default(),
            samples: VecDeque::new(),
            baseline: None,
            pending: None,
            next: Instant::now(),
            paused: false,
            error: None,
            scroll: 0,
            max_scroll: 0,
        }
    }

    pub fn cancel(&mut self) {
        if let Some(pending) = self.pending.take() {
            pending.abort();
        }
    }

    pub fn key(&mut self, key: crate::input::Key) {
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
            KeyCode::Down => self.scroll = self.scroll.saturating_add(1).min(self.max_scroll),
            KeyCode::Up => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_add(10).min(self.max_scroll),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::Home => self.scroll = 0,
            KeyCode::End => self.scroll = self.max_scroll,
            _ => {}
        }
    }

    pub async fn poll(&mut self, socket: &std::path::Path) -> bool {
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
            let waker = self.waker.clone();
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
                waker.wake();
                result
            }));
        }
        changed
    }

    pub fn accept(&mut self, snapshot: MonitorSnapshot) {
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
}

pub fn number(value: &str) -> Option<u64> {
    value.parse().ok()
}

/// Completed includes successful and failed operations; cancelled work is
/// excluded from both duration and completion. Keep integer precision before
/// formatting milliseconds to three decimal places (whole microseconds).
pub fn mean_duration_ms(
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

pub fn rates(previous: &MonitorSnapshot, current: &MonitorSnapshot) -> [Option<f64>; 2] {
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

pub fn bytes(value: Option<f64>) -> String {
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

pub fn trend(values: &[Option<f64>], width: usize) -> String {
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
