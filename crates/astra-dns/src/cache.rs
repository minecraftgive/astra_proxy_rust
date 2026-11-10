use crate::maintenance::{Maintain, Underused};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::Arc,
    time::{Duration, Instant},
};
const BYTE_LIMIT: usize = 16 * 1024 * 1024;
struct Entry {
    wire: Arc<[u8]>,
    inserted: Instant,
    expires: Instant,
    order: u64,
}
#[derive(Default)]
pub(crate) struct Cache {
    entries: HashMap<Arc<[u8]>, Entry>,
    expiry: BTreeSet<(Instant, u64, Arc<[u8]>)>,
    order: BTreeMap<u64, Arc<[u8]>>,
    next: u64,
    bytes: usize,
    underused: Underused,
}
impl Cache {
    fn remove(&mut self, key: &[u8]) -> bool {
        let Some((key, entry)) = self.entries.remove_entry(key) else {
            return false;
        };
        self.expiry
            .remove(&(entry.expires, entry.order, key.clone()));
        self.order.remove(&entry.order);
        let bytes = key.len() + entry.wire.len();
        self.bytes -= bytes;
        astra_observe::gauge!("dns.cache.bytes", "DNS 缓存报文字节", "bytes")
            .subtract(bytes as u64);
        astra_observe::gauge!("dns.cache.entries", "DNS 缓存条目", "count").subtract(1);
        true
    }
    pub fn clear(&mut self) {
        *self = Self::default();
    }
    pub fn get(&mut self, key: &[u8], now: Instant) -> Option<(Arc<[u8]>, u32)> {
        let entry = self.entries.get(key)?;
        if now >= entry.expires {
            self.remove(key);
            astra_observe::counter!("dns.cache.expired", "DNS 缓存过期移除").add(1);
            return None;
        }
        Some((
            entry.wire.clone(),
            now.duration_since(entry.inserted).as_secs() as u32,
        ))
    }
    pub fn expire(&mut self, now: Instant) {
        while let Some((expires, _, key)) = self.expiry.first() {
            if *expires > now {
                break;
            }
            let key = key.clone();
            self.remove(&key);
            astra_observe::counter!("dns.cache.expired", "DNS 缓存过期移除").add(1);
        }
    }
    pub fn insert(&mut self, key: Vec<u8>, wire: Vec<u8>, ttl: u32, capacity: usize, now: Instant) {
        self.expire(now);
        self.remove(&key);
        let bytes = key.len() + wire.len();
        if capacity == 0 || ttl == 0 || bytes > BYTE_LIMIT {
            return;
        }
        while self.entries.len() >= capacity || self.bytes + bytes > BYTE_LIMIT {
            let Some((_, key)) = self.order.first_key_value() else {
                break;
            };
            let key = key.clone();
            self.remove(&key);
            astra_observe::counter!("dns.cache.evictions", "DNS 缓存容量淘汰").add(1);
        }
        let key: Arc<[u8]> = key.into();
        let expires = now + Duration::from_secs(ttl.into());
        let order = self.next;
        self.next += 1;
        self.entries.insert(
            key.clone(),
            Entry {
                wire: wire.into(),
                inserted: now,
                expires,
                order,
            },
        );
        self.expiry.insert((expires, order, key.clone()));
        self.order.insert(order, key);
        if self.entries.len().saturating_mul(4) >= self.entries.capacity() {
            self.underused = Underused::default();
        }
        self.bytes += bytes;
        astra_observe::gauge!("dns.cache.bytes", "DNS 缓存报文字节", "bytes").add(bytes as u64);
        astra_observe::gauge!("dns.cache.entries", "DNS 缓存条目", "count").add(1);
    }
}
impl Maintain for Cache {
    fn maintain(&mut self, now: Instant) {
        self.expire(now);
        if self
            .underused
            .ready(self.entries.len(), self.entries.capacity(), now)
        {
            self.entries.shrink_to_fit();
        }
    }
}
impl Drop for Cache {
    fn drop(&mut self) {
        astra_observe::gauge!("dns.cache.bytes", "DNS 缓存报文字节", "bytes")
            .subtract(self.bytes as u64);
        astra_observe::gauge!("dns.cache.entries", "DNS 缓存条目", "count")
            .subtract(self.entries.len() as u64);
    }
}
#[cfg(test)]
#[path = "test/cache_test.rs"]
mod tests;
