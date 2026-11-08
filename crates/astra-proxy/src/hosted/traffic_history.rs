//! Durable per-profile, per-outbound accounting, independent of UI connections.
//! Workers expose monotonic totals; only deltas enter the history, including
//! the first sample of a new generation. Minute buckets bound retained history.
use astra_model::runtime::NodeTraffic;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const MONTH: u64 = 30 * 24 * 60;
const HISTORY_LIMIT: u64 = 64 * 1024 * 1024;

#[derive(Clone, Default, Serialize, Deserialize)]
struct Record {
    total: u64,
    minutes: BTreeMap<u64, u64>,
}

#[derive(Default)]
pub(super) struct History {
    records: BTreeMap<String, Record>,
    session: BTreeMap<String, u64>,
    seen: BTreeMap<(u64, String), u64>,
    path: Option<PathBuf>,
    saved: Option<Instant>,
    pub error: Option<String>,
}

pub(super) fn minute() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        / 60
}

impl History {
    pub fn open(path: std::io::Result<PathBuf>) -> Self {
        let mut history = Self::default();
        let result = (|| -> anyhow::Result<()> {
            let path = path?;
            history.records = match astra_platform::storage::read_bounded(&path, HISTORY_LIMIT) {
                Ok(bytes) => serde_json::from_slice(&bytes)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
                Err(error) => return Err(error.into()),
            };
            history.path = Some(path);
            Ok(())
        })();
        if let Err(error) = result {
            // Preserve an unreadable history instead of overwriting it with zeros.
            history.error = Some(format!("Traffic history unavailable: {error:#}"));
        }
        history
    }

    pub fn observe(&mut self, generation: u64, nodes: &BTreeMap<String, u64>, now: u64) {
        for (name, &total) in nodes {
            let seen = self.seen.entry((generation, name.clone())).or_default();
            // Cached/older responses and counter resets cannot double-count bytes.
            let delta = total.saturating_sub(*seen);
            *seen = (*seen).max(total);
            let record = self.records.entry(name.clone()).or_default();
            let session = self.session.entry(name.clone()).or_default();
            *session = session.saturating_add(delta);
            record.total = record.total.saturating_add(delta);
            if delta > 0 {
                let bucket = record.minutes.entry(now).or_default();
                *bucket = bucket.saturating_add(delta);
            }
        }
    }

    pub fn forget_generation(&mut self, generation: u64) {
        self.seen.retain(|(id, _), _| *id != generation);
    }

    pub fn summary(&mut self, now: u64) -> BTreeMap<String, NodeTraffic> {
        self.records
            .iter_mut()
            .map(|(name, record)| {
                record
                    .minutes
                    .retain(|minute, _| *minute > now.saturating_sub(MONTH));
                let sum = |window: u64| {
                    record
                        .minutes
                        .range((
                            std::ops::Bound::Excluded(now.saturating_sub(window)),
                            std::ops::Bound::Included(now),
                        ))
                        .fold(0u64, |total, (_, bytes)| total.saturating_add(*bytes))
                };
                (
                    name.clone(),
                    NodeTraffic {
                        session: self.session.get(name).copied().unwrap_or(0),
                        hour: sum(60),
                        day: sum(24 * 60),
                        week: sum(7 * 24 * 60),
                        month: sum(MONTH),
                        persist: record.total,
                    },
                )
            })
            .collect()
    }

    pub fn checkpoint_due(&self) -> bool {
        self.path.is_some()
            && self
                .saved
                .is_none_or(|at| at.elapsed() >= Duration::from_secs(30))
    }

    pub fn save(&mut self) {
        let Some(path) = &self.path else { return };
        self.saved = Some(Instant::now());
        let result = (|| -> anyhow::Result<()> {
            let bytes = serde_json::to_vec(&self.records)?;
            anyhow::ensure!(
                bytes.len() as u64 <= HISTORY_LIMIT,
                "traffic history exceeds 64 MiB"
            );
            astra_platform::storage::atomic_write(
                path,
                &bytes,
                astra_platform::storage::WritePolicy::runtime(),
            )?;
            Ok(())
        })();
        self.error = result
            .err()
            .map(|error| format!("Cannot save traffic history: {error}"));
        if let Some(error) = &self.error {
            tracing::warn!(%error);
        }
    }
}

#[cfg(test)]
#[path = "../test/hosted_traffic_history_test.rs"]
mod tests;
