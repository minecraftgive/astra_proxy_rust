//! Additive internal diagnostics. Counters are decimal strings across JSON.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub struct InternalSnapshot {
    /// Changes when the observed process restarts; counters must not span epochs.
    pub epoch: String,
    pub metrics: Vec<Metric>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub struct Metric {
    /// Capture time for this metric's process; worker samples may arrive later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "bindings", ts(optional))]
    pub sampled_at_ms: Option<String>,
    pub name: String,
    pub value: String,
    pub kind: MetricKind,
    pub unit: String,
    pub description: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub enum MetricKind {
    Counter,
    Gauge,
}
