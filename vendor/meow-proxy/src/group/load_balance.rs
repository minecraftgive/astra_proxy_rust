use async_trait::async_trait;
use meow_common::{
    AdapterType, DelayHistory, MeowError, Metadata, ProviderSlot, Proxy, ProxyAdapter, ProxyConn,
    ProxyHealth, ProxyPacketConn, Result,
};
use smol_str::SmolStr;
use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use super::{DialFailureTracker, UsageTracker};

#[derive(Debug)]
pub enum LbStrategy {
    RoundRobin,
    ConsistentHashing,
}

pub struct LoadBalanceGroup {
    name: SmolStr,
    static_proxies: Vec<Arc<dyn Proxy>>,
    /// Provider-sourced members (`use:` / `include-all`), each a live
    /// slot whose contents the owning provider swaps on refresh — the
    /// same `ProviderSlot` shape url-test/fallback/selector carry.
    /// Invariant: slots only ever hold leaf adapters (provider payloads
    /// cannot declare groups), so member walks never recurse into another
    /// group's slot guards.
    provider_slots: Vec<ProviderSlot>,
    strategy: LbStrategy,
    counter: AtomicUsize,
    /// Group health-check `url:` (upstream `TestUrl`) — pick-time member
    /// eligibility is `alive_for_url(test_url)`, and the sweep/API read it
    /// via `Proxy::test_url`.
    test_url: String,
    expected_status: String,
    health: ProxyHealth,
    usage: UsageTracker,
    /// mihomo `GroupBase.onDialFailed` escalation: repeated member dial
    /// failures mark the member dead between sweeps — an additional
    /// liveness signal on top of the periodic sweep, which probes provider
    /// members too via `member_proxies()` (issue #543).
    dial_failures: DialFailureTracker,
}

impl LoadBalanceGroup {
    pub fn new(name: &str, proxies: Vec<Arc<dyn Proxy>>, strategy: LbStrategy) -> Self {
        Self::new_with_providers(name, proxies, strategy, Vec::new())
    }

    pub fn new_with_providers(
        name: &str,
        proxies: Vec<Arc<dyn Proxy>>,
        strategy: LbStrategy,
        slots: Vec<ProviderSlot>,
    ) -> Self {
        Self {
            name: SmolStr::from(name),
            static_proxies: proxies,
            provider_slots: slots,
            strategy,
            counter: AtomicUsize::new(0),
            test_url: "https://www.gstatic.com/generate_204".to_string(),
            expected_status: String::new(),
            health: ProxyHealth::new(),
            usage: UsageTracker::new(),
            dial_failures: DialFailureTracker::new(),
        }
    }

    /// Visit every member in canonical order — static `proxies:` entries
    /// first, then each provider slot under its read guard (the same order
    /// url-test/selector enumerate). `f` returning `false` stops the walk.
    fn for_each_member(&self, mut f: impl FnMut(&Arc<dyn Proxy>) -> bool) {
        for p in &self.static_proxies {
            if !f(p) {
                return;
            }
        }
        for slot in &self.provider_slots {
            let guard = slot.read();
            for p in guard.iter() {
                if !f(p) {
                    return;
                }
            }
        }
    }

    fn any_member(&self, mut pred: impl FnMut(&Arc<dyn Proxy>) -> bool) -> bool {
        let mut hit = false;
        self.for_each_member(|p| {
            hit = pred(p);
            !hit
        });
        hit
    }

    /// First alive member in canonical order (for `current()`/`delay_history`).
    fn first_alive_member(&self) -> Option<Arc<dyn Proxy>> {
        let mut out = None;
        self.for_each_member(|p| {
            if p.alive() {
                out = Some(Arc::clone(p));
                false
            } else {
                true
            }
        });
        out
    }

    /// Smallest positive delay across alive members (0 = none measured).
    fn min_alive_delay(&self, mut delay: impl FnMut(&Arc<dyn Proxy>) -> u16) -> u16 {
        let mut best = 0u16;
        self.for_each_member(|p| {
            if p.alive() {
                let d = delay(p);
                if d > 0 && (best == 0 || d < best) {
                    best = d;
                }
            }
            true
        });
        best
    }

    /// Attach the group health-check `url:` (upstream `TestUrl`). Member
    /// eligibility on every pick is `alive_for_url(test_url)`, and the
    /// sweep/API read it via `Proxy::test_url` (issue #621).
    #[must_use]
    pub fn with_test_url(mut self, test_url: String) -> Self {
        self.test_url = test_url;
        self
    }

    /// Attach the group-level `expected-status` probe expression
    /// (upstream `GroupCommonOption`; the health sweep reads it via
    /// `Proxy::expected_status`).
    #[must_use]
    pub fn with_expected_status(mut self, expected_status: String) -> Self {
        self.expected_status = expected_status;
        self
    }

    /// Select a proxy from the alive set for a TCP connection.
    ///
    /// Returns `None` if no alive proxy exists.
    ///
    /// TODO(perf M2): cache alive-set or use a pre-filtered index if profiling shows this hot
    pub fn select(&self, metadata: &Metadata) -> Option<Arc<dyn Proxy>> {
        self.pick(metadata, false, true)
    }

    fn select_udp(&self, metadata: &Metadata) -> Option<Arc<dyn Proxy>> {
        self.pick(metadata, true, true)
    }

    /// Eligibility for a pick: alive for the group's test URL (upstream
    /// `AliveForTestUrl(testUrl)` — for leaf adapters the single health
    /// flag makes this identical to `alive()`), and UDP-capable when the
    /// pick is for a UDP flow (upstream LB does not filter by UDP — that
    /// asymmetry is local).
    fn eligible(&self, p: &Arc<dyn Proxy>, udp_only: bool) -> bool {
        p.alive_for_url(&self.test_url) && (!udp_only || p.support_udp())
    }

    /// One immutable member snapshot per pick — statics first, then each
    /// provider slot's current contents. The previous two-pass form
    /// (count eligible, then re-walk under fresh slot read-guards) could
    /// observe a member death or a provider-slot swap between passes and
    /// shift the pick or yield `None` for one dial (issue #621).
    fn pick(&self, metadata: &Metadata, udp_only: bool, advance: bool) -> Option<Arc<dyn Proxy>> {
        let members = self.member_proxies().unwrap_or_default();
        match self.strategy {
            LbStrategy::RoundRobin => {
                // Evaluate eligibility once: a health flag can flip between
                // a count pass and a pick pass, which would shrink the set
                // under `c % old_len` and yield `None` for a dial that had
                // live members (issue #621 review).
                let eligible: Vec<&Arc<dyn Proxy>> = members
                    .iter()
                    .filter(|p| self.eligible(p, udp_only))
                    .collect();
                if eligible.is_empty() {
                    return None;
                }
                // `advance=false` is the match-time peek (`Unwrap(metadata,
                // false)` upstream): round-robin reads the counter without
                // committing it.
                let c = if advance {
                    self.counter.fetch_add(1, Ordering::Relaxed)
                } else {
                    self.counter.load(Ordering::Relaxed)
                };
                Some(Arc::clone(eligible[c % eligible.len()]))
            }
            LbStrategy::ConsistentHashing => {
                // mihomo `strategyConsistentHashing`: jump-hash the
                // destination key over the FULL member list; on an
                // ineligible member retry with the incremented hash, up
                // to five times, then fall back to a linear scan
                // (adapter/outbound/loadbalance.go).
                if members.is_empty() {
                    return None;
                }
                let mut key = fnv1a64(get_key(metadata).as_bytes());
                for _ in 0..5 {
                    let idx = jump_hash(key, members.len()) as usize;
                    let p = &members[idx];
                    if self.eligible(p, udp_only) {
                        return Some(Arc::clone(p));
                    }
                    key = key.wrapping_add(1);
                }
                members
                    .iter()
                    .find(|p| self.eligible(p, udp_only))
                    .map(Arc::clone)
            }
        }
    }
}

/// mihomo `getKey`: the consistent-hashing key derives from the
/// *destination*, not the client. An IP-literal host is used verbatim, a
/// domain is reduced to its eTLD+1 (`a.b.example.co.uk` → `example.co.uk`,
/// so all of a registrable domain's hosts share one member), and anything
/// else falls back to `dst_ip` (empty when neither exists — every such
/// connection then lands on the same member, deterministic not random).
fn get_key(metadata: &Metadata) -> String {
    if !metadata.host.is_empty() {
        if metadata.host.parse::<IpAddr>().is_ok() {
            return metadata.host.to_string();
        }
        // Lowercasing is a deliberate improvement, not upstream parity —
        // Go's publicsuffix lookup is case-sensitive, so `getKey` there
        // returns the mixed-case eTLD+1; DNS is case-insensitive, so
        // folding first groups `WWW.Example.COM` with `example.com`
        // instead of splitting them across members. Unreachable in
        // practice either way: every ingress already lowercases `host`.
        //
        // A host still carrying a port/brackets (`example.com:443`,
        // `[::1]`) isn't a bare domain. Upstream's helper *succeeds* on
        // dotted ones (wildcard rule → the whole `domain:port` string
        // becomes the key) and only fails on single-label port strings —
        // we instead fall back to `dst_ip`, which keeps the destination
        // keyed rather than inventing a suffix. Defensive: no ingress
        // leaves a port in `host` today.
        let lower = metadata.host.to_ascii_lowercase();
        if !lower.contains(':') {
            if let Some(domain) = psl::domain_str(&lower) {
                return domain.to_string();
            }
        }
    }
    metadata.dst_ip.map(|ip| ip.to_string()).unwrap_or_default()
}

/// FNV-1a 64-bit over the destination key. Upstream hashes with
/// `utils.MapHash` (Go `maphash`, seeded per process — its assignments are
/// deliberately *not* reproducible across restarts), so parity targets the
/// key derivation and selection structure, not bit-identical buckets; a
/// fixed 64-bit hash gives us the stable cross-restart mapping upstream
/// itself cannot offer.
fn fnv1a64(data: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Lamping–Veach jump consistent hash — a direct port of mihomo's
/// `jumpHash` (adapter/outbound/loadbalance.go): same 64-bit LCG state and
/// the same float64 bucket arithmetic, so identical keys land on identical
/// bucket indices for a given member count.
fn jump_hash(mut key: u64, buckets: usize) -> u32 {
    if buckets == 0 {
        return 0;
    }
    let (mut b, mut j) = (-1i64, 0i64);
    while j < buckets as i64 {
        b = j;
        key = key.wrapping_mul(2862933555777941757).wrapping_add(1);
        j = ((b + 1) as f64 * ((1u64 << 31) as f64 / ((key >> 33) + 1) as f64)) as i64;
    }
    b as u32
}

#[async_trait]
impl ProxyAdapter for LoadBalanceGroup {
    fn name(&self) -> &str {
        &self.name
    }

    fn adapter_type(&self) -> AdapterType {
        AdapterType::LoadBalance
    }

    fn addr(&self) -> &str {
        ""
    }

    fn support_udp(&self) -> bool {
        // Mirror the UDP pick's eligibility filter: a member ineligible at
        // pick time cannot be picked, so it must not advertise capability
        // either — otherwise an empty `select_udp` would surface as
        // NoProxyAvailable (a real failure) instead of the capability
        // refusal it is.
        self.any_member(|p| self.eligible(p, true))
    }

    async fn dial_tcp(&self, metadata: &Metadata) -> Result<Box<dyn ProxyConn>> {
        self.usage.touch_user_traffic(metadata);
        let proxy = self.select(metadata).ok_or(MeowError::NoProxyAvailable)?;
        let attempt = super::DialAttempt::new(&self.name, &self.dial_failures, &proxy);
        attempt.finish(proxy.dial_tcp(metadata).await)
    }

    async fn dial_udp(&self, metadata: &Metadata) -> Result<Box<dyn ProxyPacketConn>> {
        self.usage.touch_user_traffic(metadata);
        // An empty UDP pick with alive members is a capability refusal
        // (nothing alive can carry UDP), not an availability failure —
        // keep the class exempt from dead-marking like a leaf's refusal.
        let proxy = self.select_udp(metadata).ok_or_else(|| {
            if self.any_member(|p| self.eligible(p, false)) {
                MeowError::NotSupported(format!(
                    "load-balance {}: no alive member can carry UDP",
                    self.name
                ))
            } else {
                MeowError::NoProxyAvailable
            }
        })?;
        let attempt = super::DialAttempt::new(&self.name, &self.dial_failures, &proxy);
        attempt.finish(proxy.dial_udp(metadata).await)
    }

    fn unwrap_proxy(&self, metadata: &Metadata, touch: bool) -> Option<Arc<dyn Proxy>> {
        if touch {
            self.usage.touch_user_traffic(metadata);
        }
        // Peek the same member space the upcoming dial uses: UDP flows pick
        // from UDP-capable members only (upstream LB does not filter by UDP,
        // so this asymmetry is local — keep probe and dial consistent).
        self.pick(
            metadata,
            metadata.network == meow_common::Network::Udp,
            touch,
        )
    }

    fn health(&self) -> &ProxyHealth {
        &self.health
    }
}

impl Proxy for LoadBalanceGroup {
    fn alive(&self) -> bool {
        // Own `health` bit AND delegated member liveness — see
        // `FallbackGroup::alive` (issue #681).
        self.health.alive() && self.any_member(|p| p.alive())
    }

    fn alive_for_url(&self, url: &str) -> bool {
        self.health.alive() && self.any_member(|p| p.alive_for_url(url))
    }

    fn last_delay(&self) -> u16 {
        self.min_alive_delay(|p| p.last_delay())
    }

    fn last_delay_for_url(&self, url: &str) -> u16 {
        self.min_alive_delay(|p| p.last_delay_for_url(url))
    }

    fn delay_history(&self) -> Vec<DelayHistory> {
        self.first_alive_member()
            .map(|p| p.delay_history())
            .unwrap_or_default()
    }

    fn members(&self) -> Option<Vec<String>> {
        let mut out = Vec::new();
        self.for_each_member(|p| {
            out.push(p.name().to_string());
            true
        });
        Some(out)
    }

    fn member_proxies(&self) -> Option<Vec<Arc<dyn Proxy>>> {
        let mut out = Vec::new();
        self.for_each_member(|p| {
            out.push(Arc::clone(p));
            true
        });
        Some(out)
    }

    fn current(&self) -> Option<String> {
        // For load-balance, no single "current" proxy; return first alive for API compat.
        self.first_alive_member().map(|p| p.name().to_string())
    }

    fn test_url(&self) -> Option<&str> {
        Some(&self.test_url)
    }

    fn expected_status(&self) -> Option<&str> {
        Some(&self.expected_status)
    }

    fn usage_generation(&self) -> u64 {
        self.usage.generation()
    }
}

#[cfg(test)]
#[path = "../test/group_load_balance_test.rs"]
mod tests;
