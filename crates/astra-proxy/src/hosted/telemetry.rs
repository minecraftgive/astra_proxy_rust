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
    pub node_traffic: Option<BTreeMap<String, astra_model::runtime::NodeTraffic>>,
    pub traffic_history_error: Option<String>,
    dns: astra_model::runtime::DnsQueryLog,
    dns_supported: bool,
    pub chains: Option<Box<astra_model::runtime::ChainTelemetry>>,
    pub dns_resolver: Option<Box<astra_model::runtime::DnsResolverStatus>>,
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
            node_traffic: None,
            traffic_history_error: None,
            dns: Default::default(),
            dns_supported: false,
            dns_resolver: None,
            chains: None,
        }
    }
}
impl Store {
    pub fn merge_dns(
        &mut self,
        logs: Vec<astra_model::runtime::DnsQueryLog>,
        supported: Option<bool>,
    ) {
        if let Some(supported) = supported {
            self.dns_supported = supported;
        }
        for log in logs {
            self.dns.truncated |= log.truncated;
            for record in log.records {
                if !self
                    .dns
                    .records
                    .iter()
                    .any(|old| old.resolver == record.resolver && old.id == record.id)
                {
                    self.dns.records.push(record);
                }
            }
        }
        self.dns.records.sort_by(|a, b| {
            b.timestamp_ms
                .cmp(&a.timestamp_ms)
                .then(b.id.cmp(&a.id))
                .then(b.resolver.cmp(&a.resolver))
        });
        self.dns.truncated |= self.dns.records.len() > astra_model::runtime::DNS_QUERY_LIMIT;
        self.dns
            .records
            .truncate(astra_model::runtime::DNS_QUERY_LIMIT);
    }

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
            dns: self.dns_supported.then(|| Box::new(self.dns.clone())),
            dns_resolver: self.dns_resolver.clone(),
            chains: self.chains.clone(),
            traffic: Some(Box::new(astra_model::runtime::TrafficTelemetry {
                nodes: self.node_traffic.clone(),
                history_error: self.traffic_history_error.clone(),
                selected_node: None,
            })),
        }
    }
}

#[cfg(test)]
#[path = "../test/hosted_telemetry_test.rs"]
mod tests;
