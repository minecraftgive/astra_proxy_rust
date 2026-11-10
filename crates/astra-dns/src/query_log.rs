//! Per-resolver bounded history. A cancelled future also leaves a completed record.
use astra_model::runtime::{DnsQuery, DnsQueryLog, DNS_QUERY_LIMIT};
use hickory_proto::op::Message;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

pub(super) struct Store {
    resolver: String,
    state: Mutex<(u64, VecDeque<DnsQuery>, bool)>,
}
impl Default for Store {
    fn default() -> Self {
        Self {
            resolver: uuid::Uuid::new_v4().to_string(),
            state: Default::default(),
        }
    }
}
impl Store {
    pub fn snapshot(&self) -> DnsQueryLog {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        DnsQueryLog {
            records: state.1.iter().cloned().collect(),
            truncated: state.2,
        }
    }
}
pub(super) struct Query {
    store: Arc<Store>,
    started: Instant,
    record: Option<DnsQuery>,
}
fn bounded(value: &str, limit: usize) -> String {
    let mut bytes = 0;
    value
        .chars()
        .filter(|c| !c.is_control())
        .take_while(|c| {
            bytes += c.len_utf8();
            bytes <= limit
        })
        .collect()
}
impl Query {
    pub fn new(store: &Arc<Store>, message: Option<&Message>) -> Self {
        let record = message
            .and_then(|m| m.queries.first())
            .map(|query| DnsQuery {
                resolver: store.resolver.clone(),
                timestamp_ms: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
                domain: query
                    .name()
                    .to_ascii()
                    .trim_end_matches('.')
                    .to_ascii_lowercase(),
                query_type: query.query_type().to_string(),
                source: "upstream".into(),
                status: "CANCELLED".into(),
                ..Default::default()
            });
        Self {
            store: store.clone(),
            started: Instant::now(),
            record,
        }
    }
    pub fn source(&mut self, source: &str) {
        if let Some(record) = &mut self.record {
            record.source = source.into();
        }
    }
    pub fn upstream(&mut self, upstream: &str) {
        if let Some(record) = &mut self.record {
            record.upstream = Some(bounded(upstream, 200));
        }
    }
    pub fn finish(&mut self, result: Result<Option<&Message>, String>) {
        if let Some(record) = &mut self.record {
            match result {
                Ok(Some(reply)) => {
                    record.status = format!("{:?}", reply.metadata.response_code);
                    record.answers_truncated = reply.answers.len() > 8;
                    record.answers = reply
                        .answers
                        .iter()
                        .take(8)
                        .map(|r| {
                            let answer =
                                format!("{} {} TTL={} {}", r.name, r.record_type(), r.ttl, r.data);
                            record.answers_truncated |= answer.len() > 160;
                            bounded(&answer, 160)
                        })
                        .collect();
                }
                Ok(None) => record.status = "UNKNOWN".into(),
                Err(error) => {
                    record.status = "ERROR".into();
                    record.error = Some(bounded(&error, 240));
                }
            }
        }
        self.publish();
    }
    fn publish(&mut self) {
        let Some(mut record) = self.record.take() else {
            return;
        };
        record.duration_us = self.started.elapsed().as_micros().min(u64::MAX as u128) as u64;
        let mut state = self
            .store
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.0 += 1;
        record.id = state.0;
        if state.1.len() == DNS_QUERY_LIMIT {
            state.1.pop_back();
            state.2 = true;
        }
        state.1.push_front(record);
    }
}
impl Drop for Query {
    fn drop(&mut self) {
        self.publish();
    }
}
