//! Bounded process-local instrumentation. No exporter thread, I/O, task labels,
//! request payloads or packet-path allocations. Metric sites register once.
use astra_model::monitor::{InternalSnapshot, Metric, MetricKind};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::Instant,
};

struct Entry {
    description: &'static str,
    unit: &'static str,
    kind: MetricKind,
    value: Arc<AtomicU64>,
}
fn registry() -> &'static Mutex<BTreeMap<&'static str, Entry>> {
    static REGISTRY: OnceLock<Mutex<BTreeMap<&'static str, Entry>>> = OnceLock::new();
    REGISTRY.get_or_init(Mutex::default)
}
pub struct Handle(Arc<AtomicU64>);
impl Handle {
    pub fn register(
        name: &'static str,
        description: &'static str,
        unit: &'static str,
        kind: MetricKind,
    ) -> Self {
        let mut registry = registry().lock().unwrap_or_else(|e| e.into_inner());
        let entry = registry.entry(name).or_insert_with(|| Entry {
            description,
            unit,
            kind,
            value: Arc::new(AtomicU64::new(0)),
        });
        Self(entry.value.clone())
    }
    pub fn add(&self, amount: u64) {
        self.0.fetch_add(amount, Ordering::Relaxed);
    }
    pub fn set(&self, value: u64) {
        self.0.store(value, Ordering::Relaxed);
    }
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
    pub fn enter(&'static self) -> GaugeGuard {
        self.add(1);
        GaugeGuard(self)
    }
}
pub struct GaugeGuard(&'static Handle);
impl Drop for GaugeGuard {
    fn drop(&mut self) {
        self.0 .0.fetch_sub(1, Ordering::Relaxed);
    }
}
pub fn snapshot(epoch: impl Into<String>) -> InternalSnapshot {
    let sampled_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .to_string();
    InternalSnapshot {
        epoch: epoch.into(),
        metrics: registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(name, entry)| Metric {
                sampled_at_ms: Some(sampled_at_ms.clone()),
                name: (*name).into(),
                value: entry.value.load(Ordering::Relaxed).to_string(),
                kind: entry.kind,
                unit: entry.unit.into(),
                description: entry.description.into(),
            })
            .collect(),
    }
}
pub use astra_model::monitor::MetricKind as Kind;
#[macro_export]
macro_rules! counter {
    ($name:literal, $description:literal) => {{
        static VALUE: std::sync::LazyLock<$crate::Handle> = std::sync::LazyLock::new(|| {
            $crate::Handle::register($name, $description, "count", $crate::Kind::Counter)
        });
        &*VALUE
    }};
}
#[macro_export]
macro_rules! gauge {
    ($name:literal, $description:literal, $unit:literal) => {{
        static VALUE: std::sync::LazyLock<$crate::Handle> = std::sync::LazyLock::new(|| {
            $crate::Handle::register($name, $description, $unit, $crate::Kind::Gauge)
        });
        &*VALUE
    }};
}
pub struct OperationMetrics {
    pub started: Handle,
    pub completed: Handle,
    pub failed: Handle,
    pub cancelled: Handle,
    pub in_flight: Handle,
    pub duration_us: Handle,
}
impl OperationMetrics {
    pub fn start(&'static self) -> Operation {
        self.started.add(1);
        self.in_flight.add(1);
        Operation {
            metrics: self,
            started: Instant::now(),
            finished: false,
        }
    }
}
pub struct Operation {
    metrics: &'static OperationMetrics,
    started: Instant,
    finished: bool,
}
impl Operation {
    /// Completed includes successes and errors. Duration excludes cancellations.
    pub fn finish(mut self, success: bool) {
        self.metrics.completed.add(1);
        if !success {
            self.metrics.failed.add(1);
        }
        self.metrics
            .duration_us
            .add(self.started.elapsed().as_micros().min(u64::MAX as u128) as u64);
        self.finished = true;
    }
}
impl Drop for Operation {
    fn drop(&mut self) {
        self.metrics.in_flight.0.fetch_sub(1, Ordering::Relaxed);
        if !self.finished {
            self.metrics.cancelled.add(1);
        }
    }
}
#[macro_export]
macro_rules! operation {
    ($prefix:literal, $description:literal) => {{
        static VALUE: std::sync::LazyLock<$crate::OperationMetrics> =
            std::sync::LazyLock::new(|| $crate::OperationMetrics {
                started: $crate::Handle::register(
                    concat!($prefix, ".started"),
                    concat!($description, "：开始"),
                    "count",
                    $crate::Kind::Counter,
                ),
                completed: $crate::Handle::register(
                    concat!($prefix, ".completed"),
                    concat!($description, "：完成（含错误）"),
                    "count",
                    $crate::Kind::Counter,
                ),
                failed: $crate::Handle::register(
                    concat!($prefix, ".failed"),
                    concat!($description, "：错误"),
                    "count",
                    $crate::Kind::Counter,
                ),
                cancelled: $crate::Handle::register(
                    concat!($prefix, ".cancelled"),
                    concat!($description, "：取消 / 中断"),
                    "count",
                    $crate::Kind::Counter,
                ),
                in_flight: $crate::Handle::register(
                    concat!($prefix, ".in_flight"),
                    concat!($description, "：正在处理"),
                    "count",
                    $crate::Kind::Gauge,
                ),
                duration_us: $crate::Handle::register(
                    concat!($prefix, ".duration_us"),
                    concat!($description, "：累计完成耗时"),
                    "us",
                    $crate::Kind::Counter,
                ),
            });
        VALUE.start()
    }};
}
#[cfg(test)]
mod monitor_tests {
    use super::*;
    #[test]
    fn completion_failure_cancellation_and_gauge_are_distinct() {
        fn start() -> Operation {
            operation!("test.operation", "测试")
        }
        start().finish(true);
        start().finish(false);
        drop(start());
        let snapshot = snapshot("epoch");
        let value = |name: &str| {
            snapshot
                .metrics
                .iter()
                .find(|m| m.name == format!("test.operation.{name}"))
                .unwrap()
                .value
                .clone()
        };
        assert_eq!(value("started"), "3");
        assert_eq!(value("completed"), "2");
        assert_eq!(value("failed"), "1");
        assert_eq!(value("cancelled"), "1");
        assert_eq!(value("in_flight"), "0");
    }
    #[test]
    fn counters_keep_integer_precision_and_registration_is_shared() {
        let a = Handle::register("test.precision", "测试", "count", Kind::Counter);
        let b = Handle::register("test.precision", "测试", "count", Kind::Counter);
        a.add(9_007_199_254_740_993);
        assert_eq!(b.get(), 9_007_199_254_740_993);
        assert_eq!(
            snapshot("epoch")
                .metrics
                .iter()
                .find(|m| m.name == "test.precision")
                .unwrap()
                .value,
            "9007199254740993"
        );
    }
}
