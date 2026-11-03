//! A coherent cached sample; publishing a cached response does not advance its clock.
use astra_model::runtime::Coverage;
use astra_model::runtime::GenerationTelemetry;
use astra_model::runtime::RuntimeTelemetry;
use astra_model::runtime::Snapshot;
use std::{
    collections::{BTreeMap, HashMap},
    time::Instant,
};

pub(super) struct Store {
    started: Instant,
    pub snapshot: Snapshot,
    sequence: u64,
    sampled_ms: u64,
    observed: HashMap<u64, u64>,
    health: BTreeMap<u64, bool>,
    pub origins: BTreeMap<u64, u64>,
}
impl Default for Store {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            snapshot: Snapshot::default(),
            sequence: 0,
            sampled_ms: 0,
            observed: HashMap::new(),
            health: BTreeMap::new(),
            origins: BTreeMap::new(),
        }
    }
}
impl Store {
    pub fn publish(
        &mut self,
        snapshot: Snapshot,
        observations: BTreeMap<u64, Option<Instant>>,
        origins: BTreeMap<u64, u64>,
    ) {
        let now = self.started.elapsed().as_millis() as u64;
        let health: BTreeMap<_, _> = observations
            .iter()
            .map(|(&id, at)| (id, at.is_some()))
            .collect();
        self.observed.retain(|id, _| health.contains_key(id));
        if health.values().any(|healthy| *healthy) {
            self.sequence = self.sequence.saturating_add(1);
            self.sampled_ms = now;
            for (&id, &at) in &observations {
                if let Some(at) = at {
                    self.observed.insert(
                        id,
                        at.saturating_duration_since(self.started).as_millis() as u64,
                    );
                }
            }
        }
        self.snapshot = snapshot;
        self.health = health;
        self.origins = origins;
    }
    pub fn metadata(&self, boot: &str, active: u64, paused: bool) -> RuntimeTelemetry {
        let now = self.started.elapsed().as_millis() as u64;
        let age = now.saturating_sub(self.sampled_ms);
        let healthy = self
            .health
            .iter()
            .filter(|(id, value)| {
                **value
                    && self
                        .observed
                        .get(id)
                        .is_some_and(|at| now.saturating_sub(*at) <= 3000)
            })
            .count();
        let coverage = if self.sequence == 0 {
            Coverage::Unknown
        } else if healthy == 0 || age > 3000 {
            Coverage::Stale
        } else if healthy < self.health.len() || !self.health.contains_key(&active) {
            Coverage::Partial
        } else {
            Coverage::Complete
        };
        RuntimeTelemetry {
            host_boot_id: boot.into(),
            active_generation: Some(active),
            ingress_paused: paused,
            sample_sequence: self.sequence,
            sample_elapsed_ms: self.sampled_ms,
            sample_age_ms: age,
            coverage,
            generations: self
                .health
                .iter()
                .map(|(&generation, &healthy)| GenerationTelemetry {
                    generation,
                    healthy: healthy
                        && self
                            .observed
                            .get(&generation)
                            .is_some_and(|at| now.saturating_sub(*at) <= 3000),
                    sample_age_ms: self
                        .observed
                        .get(&generation)
                        .map(|at| now.saturating_sub(*at)),
                })
                .collect(),
            connection_generations: self.origins.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cached_reads_and_failed_rounds_do_not_become_new_samples() {
        let mut store = Store::default();
        store.publish(
            Snapshot::default(),
            BTreeMap::from([(1, Some(Instant::now()))]),
            BTreeMap::new(),
        );
        let first = store.metadata("boot", 1, false);
        assert_eq!(first.coverage, Coverage::Complete);
        assert_eq!(
            store.metadata("boot", 2, true).sample_sequence,
            first.sample_sequence
        );
        store.publish(
            Snapshot::default(),
            BTreeMap::from([(1, None)]),
            BTreeMap::new(),
        );
        let failed = store.metadata("boot", 2, true);
        assert_eq!(failed.sample_sequence, first.sample_sequence);
        assert_eq!(failed.coverage, Coverage::Stale);
        assert!(failed.ingress_paused);
    }
    #[test]
    fn partial_generations_and_origins_are_explicit() {
        let mut store = Store::default();
        store.publish(
            Snapshot::default(),
            BTreeMap::from([(1, None), (2, Some(Instant::now()))]),
            BTreeMap::from([(99, 2)]),
        );
        let view = store.metadata("a", 2, false);
        assert_eq!(view.coverage, Coverage::Partial);
        assert_eq!(view.generations[0].sample_age_ms, None);
        assert_eq!(view.connection_generations[&99], 2);
    }
    #[test]
    fn activating_an_unsampled_generation_marks_coverage_partial() {
        let mut store = Store::default();
        store.publish(
            Snapshot::default(),
            BTreeMap::from([(1, Some(Instant::now()))]),
            BTreeMap::new(),
        );
        let view = store.metadata("boot", 2, false);
        assert_eq!(view.coverage, Coverage::Partial);
        assert_eq!(view.active_generation, Some(2));
        assert_eq!(
            view.generations.len(),
            1,
            "missing data is not a fabricated sample"
        );
        assert_eq!(view.generations[0].generation, 1);
        store.publish(
            Snapshot::default(),
            BTreeMap::from([(2, Some(Instant::now()))]),
            BTreeMap::new(),
        );
        let complete = store.metadata("boot", 2, false);
        assert_eq!(complete.coverage, Coverage::Complete);
        assert_eq!(complete.generations.len(), 1);
        assert_eq!(complete.generations[0].generation, 2);
    }
    #[test]
    fn slow_round_does_not_relabel_an_old_worker_observation_as_fresh() {
        let mut store = Store {
            started: Instant::now() - std::time::Duration::from_secs(5),
            ..Default::default()
        };
        store.publish(
            Snapshot::default(),
            BTreeMap::from([(1, Some(Instant::now() - std::time::Duration::from_secs(4)))]),
            BTreeMap::new(),
        );
        let view = store.metadata("boot", 1, false);
        assert_eq!(view.coverage, Coverage::Stale);
        assert!(!view.generations[0].healthy);
        assert!(view.generations[0].sample_age_ms.unwrap() >= 4000);
    }
}
