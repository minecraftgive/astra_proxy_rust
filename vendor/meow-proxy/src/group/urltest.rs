use super::selector_store::SelectorStore;
use super::{DialFailureTracker, UsageTracker};
use async_trait::async_trait;
use meow_common::{
    AdapterType, DelayHistory, MeowError, Metadata, ProviderSlot, Proxy, ProxyAdapter, ProxyConn,
    ProxyHealth, ProxyPacketConn, ProxySelection, Result,
};
use parking_lot::RwLock;
use smol_str::SmolStr;
use std::sync::Arc;

pub struct UrlTestGroup {
    name: SmolStr,
    static_proxies: Vec<Arc<dyn Proxy>>,
    provider_slots: Vec<ProviderSlot>,
    tolerance: u16,
    /// User-fixed member. This is independent from `fastest`, which remains
    /// the automatic URL-test incumbent.
    fixed: RwLock<Option<SmolStr>>,
    store: Option<Arc<SelectorStore>>,
    test_url: String,
    expected_status: String,
    /// Name of the currently selected proxy; `None` means "not yet picked,
    /// use the first available".  Updated by `pick_for_dial` whenever it
    /// promotes a new best.
    fastest: RwLock<Option<SmolStr>>,
    health: ProxyHealth,
    usage: UsageTracker,
    dial_failures: DialFailureTracker,
}

impl UrlTestGroup {
    pub fn new(name: &str, proxies: Vec<Arc<dyn Proxy>>, tolerance: u16) -> Self {
        Self {
            name: SmolStr::from(name),
            static_proxies: proxies,
            provider_slots: Vec::new(),
            tolerance,
            fixed: RwLock::new(None),
            store: None,
            test_url: "https://www.gstatic.com/generate_204".to_string(),
            expected_status: String::new(),
            fastest: RwLock::new(None),
            health: ProxyHealth::new(),
            usage: UsageTracker::new(),
            dial_failures: DialFailureTracker::new(),
        }
    }

    pub fn new_with_providers(
        name: &str,
        proxies: Vec<Arc<dyn Proxy>>,
        tolerance: u16,
        slots: Vec<ProviderSlot>,
    ) -> Self {
        Self {
            name: SmolStr::from(name),
            static_proxies: proxies,
            provider_slots: slots,
            tolerance,
            fixed: RwLock::new(None),
            store: None,
            test_url: "https://www.gstatic.com/generate_204".to_string(),
            expected_status: String::new(),
            fastest: RwLock::new(None),
            health: ProxyHealth::new(),
            usage: UsageTracker::new(),
            dial_failures: DialFailureTracker::new(),
        }
    }

    #[must_use]
    pub fn with_runtime_options(
        mut self,
        test_url: String,
        expected_status: String,
        store: Option<Arc<SelectorStore>>,
    ) -> Self {
        self.test_url = test_url;
        self.expected_status = expected_status;
        if let Some(store) = store {
            if let Some(prev) = store.get(&self.name).filter(|v| !v.is_empty()) {
                *self.fixed.write() = Some(SmolStr::from(prev));
            }
            self.store = Some(store);
        }
        self
    }

    fn contains_name(&self, name: &str) -> bool {
        self.static_proxies.iter().any(|p| p.name() == name)
            || self.provider_slots.iter().any(|slot| {
                let guard = slot.read();
                guard.iter().any(|p| p.name() == name)
            })
    }

    fn fixed_proxy_if_alive(&self) -> Option<Arc<dyn Proxy>> {
        let name = self.fixed.read().clone()?;
        for p in &self.static_proxies {
            if p.name() == name && p.alive_for_url(&self.test_url) {
                return Some(Arc::clone(p));
            }
        }
        for slot in &self.provider_slots {
            let guard = slot.read();
            for p in guard.iter() {
                if p.name() == name && p.alive_for_url(&self.test_url) {
                    return Some(Arc::clone(p));
                }
            }
        }
        None
    }

    /// Single-pass dial-path selector: walks `static_proxies` + provider
    /// slots once without allocating a unified Vec, updates `self.fastest`
    /// if a strictly-better-by-tolerance alternative exists (or if the
    /// current pick has died), and returns the chosen proxy.
    ///
    /// Previously this was three separate full scans per dial:
    /// `update_fastest` cloned the Vec once, scanned to find best, then
    /// scanned again to read the current proxy's delay/aliveness; then
    /// `fastest_proxy` cloned the Vec a second time to look up by name.
    fn pick_for_dial(&self) -> Option<Arc<dyn Proxy>> {
        if let Some(proxy) = self.fixed_proxy_if_alive() {
            return Some(proxy);
        }
        let current_name: Option<SmolStr> = self.fastest.read().clone();

        let mut best_proxy: Option<Arc<dyn Proxy>> = None;
        let mut best_delay: u16 = u16::MAX;
        let mut current_proxy: Option<Arc<dyn Proxy>> = None;
        let mut current_delay: u16 = u16::MAX;
        let mut current_alive = false;
        let mut first_any: Option<Arc<dyn Proxy>> = None;

        // Inline visit logic to avoid an `FnMut` closure that would conflict
        // with the multiple mutable borrows below.
        macro_rules! visit {
            ($p:expr) => {{
                let p: &Arc<dyn Proxy> = $p;
                if first_any.is_none() {
                    first_any = Some(Arc::clone(p));
                }
                if p.alive() {
                    let d = p.last_delay();
                    if let Some(ref n) = current_name {
                        if p.name() == n.as_str() {
                            current_alive = true;
                            current_delay = d;
                            current_proxy = Some(Arc::clone(p));
                        }
                    }
                    if d > 0 && d < best_delay {
                        best_delay = d;
                        best_proxy = Some(Arc::clone(p));
                    }
                }
            }};
        }

        for p in &self.static_proxies {
            visit!(p);
        }
        for slot in &self.provider_slots {
            let guard = slot.read();
            for p in guard.iter() {
                visit!(p);
            }
        }

        if let Some(bp) = best_proxy.as_ref() {
            if best_delay.saturating_add(self.tolerance) < current_delay || !current_alive {
                *self.fastest.write() = Some(SmolStr::from(bp.name()));
                return Some(Arc::clone(bp));
            }
        } else if !current_alive {
            let fb = first_any.clone();
            *self.fastest.write() = fb.as_ref().map(|p| SmolStr::from(p.name()));
            return fb;
        }
        current_proxy.or(best_proxy).or(first_any)
    }

    /// Read-only lookup of whatever `fastest` currently points at — used by
    /// the REST/info methods below.  No Vec allocation; falls back to the
    /// first proxy if `fastest` is unset or names something no longer present.
    fn fastest_proxy(&self) -> Option<Arc<dyn Proxy>> {
        if let Some(proxy) = self.fixed_proxy_if_alive() {
            return Some(proxy);
        }
        let name = self.fastest.read().clone();
        let mut first_any: Option<Arc<dyn Proxy>> = None;
        if let Some(n) = name {
            for p in &self.static_proxies {
                if first_any.is_none() {
                    first_any = Some(Arc::clone(p));
                }
                if p.name() == n {
                    return Some(Arc::clone(p));
                }
            }
            for slot in &self.provider_slots {
                let guard = slot.read();
                for p in guard.iter() {
                    if first_any.is_none() {
                        first_any = Some(Arc::clone(p));
                    }
                    if p.name() == n {
                        return Some(Arc::clone(p));
                    }
                }
            }
            return first_any;
        }
        // No selection yet: return first proxy if any.
        if let Some(p) = self.static_proxies.first() {
            return Some(Arc::clone(p));
        }
        for slot in &self.provider_slots {
            let guard = slot.read();
            if let Some(p) = guard.first() {
                return Some(Arc::clone(p));
            }
        }
        None
    }

    fn member_names(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .static_proxies
            .iter()
            .map(|p| p.name().to_string())
            .collect();
        for slot in &self.provider_slots {
            let guard = slot.read();
            for p in guard.iter() {
                out.push(p.name().to_string());
            }
        }
        out
    }

    /// Static members followed by every provider-slot member, in
    /// `member_names` order.
    fn member_proxy_list(&self) -> Vec<Arc<dyn Proxy>> {
        let mut out: Vec<Arc<dyn Proxy>> = self.static_proxies.iter().map(Arc::clone).collect();
        for slot in &self.provider_slots {
            let guard = slot.read();
            for p in guard.iter() {
                out.push(Arc::clone(p));
            }
        }
        out
    }
}

#[async_trait]
impl ProxyAdapter for UrlTestGroup {
    fn name(&self) -> &str {
        &self.name
    }

    fn adapter_type(&self) -> AdapterType {
        AdapterType::UrlTest
    }

    fn addr(&self) -> &str {
        ""
    }

    fn support_udp(&self) -> bool {
        // Answer for the member `dial_udp` would actually pick — not the
        // stale `fastest` cache: `pick_for_dial` recomputes the selection
        // (including tolerance hysteresis) so a UDP query neither under-
        // advertises before the first TCP dial nor over-advertises when
        // `fastest` names a member that has since gone dead.
        self.pick_for_dial().is_some_and(|p| p.support_udp())
    }

    async fn dial_tcp(&self, metadata: &Metadata) -> Result<Box<dyn ProxyConn>> {
        self.usage.touch_user_traffic(metadata);
        let proxy = self
            .pick_for_dial()
            .ok_or_else(|| MeowError::Proxy("no proxy available".into()))?;
        let attempt = super::DialAttempt::new(&self.name, &self.dial_failures, &proxy);
        attempt.finish(proxy.dial_tcp(metadata).await)
    }

    async fn dial_udp(&self, metadata: &Metadata) -> Result<Box<dyn ProxyPacketConn>> {
        self.usage.touch_user_traffic(metadata);
        let proxy = self
            .pick_for_dial()
            .ok_or_else(|| MeowError::Proxy("no proxy available".into()))?;
        let attempt = super::DialAttempt::new(&self.name, &self.dial_failures, &proxy);
        attempt.finish(proxy.dial_udp(metadata).await)
    }

    fn unwrap_proxy(&self, metadata: &Metadata, touch: bool) -> Option<Arc<dyn Proxy>> {
        if touch {
            self.usage.touch_user_traffic(metadata);
        }
        // upstream `Unwrap` → `fast(touch)`: `touch` gates only usage
        // recording — the recompute and the `fastest` update happen on
        // peeks too, so the probe can never disagree with the next dial.
        self.pick_for_dial()
    }

    fn health(&self) -> &ProxyHealth {
        &self.health
    }
}

impl Proxy for UrlTestGroup {
    fn alive(&self) -> bool {
        // Own `health` bit AND delegated member liveness — see
        // `FallbackGroup::alive` (issue #681).
        self.health.alive() && self.fastest_proxy().is_some_and(|p| p.alive())
    }

    fn alive_for_url(&self, url: &str) -> bool {
        self.health.alive() && self.fastest_proxy().is_some_and(|p| p.alive_for_url(url))
    }

    fn last_delay(&self) -> u16 {
        self.fastest_proxy().map_or(0, |p| p.last_delay())
    }

    fn last_delay_for_url(&self, url: &str) -> u16 {
        self.fastest_proxy()
            .map_or(0, |p| p.last_delay_for_url(url))
    }

    fn delay_history(&self) -> Vec<DelayHistory> {
        self.fastest_proxy()
            .map(|p| p.delay_history())
            .unwrap_or_default()
    }

    fn members(&self) -> Option<Vec<String>> {
        Some(self.member_names())
    }

    fn member_proxies(&self) -> Option<Vec<Arc<dyn Proxy>>> {
        Some(self.member_proxy_list())
    }

    fn current(&self) -> Option<String> {
        self.fastest_proxy().map(|p| p.name().into())
    }

    fn selection(&self) -> Option<&dyn ProxySelection> {
        Some(self)
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

#[async_trait]
impl ProxySelection for UrlTestGroup {
    async fn set(&self, name: &str) -> Result<()> {
        if !self.contains_name(name) {
            return Err(MeowError::Proxy("proxy not exist".into()));
        }
        self.force_set(Some(name));
        Ok(())
    }

    fn force_set(&self, name: Option<&str>) {
        *self.fixed.write() = name.map(SmolStr::from);
        if let Some(store) = &self.store {
            store.set(&self.name, name.unwrap_or(""));
        }
    }

    fn fixed(&self) -> Option<String> {
        Some(
            self.fixed
                .read()
                .as_ref()
                .map_or_else(String::new, ToString::to_string),
        )
    }

    fn can_unfix(&self) -> bool {
        true
    }
}

#[cfg(test)]
#[path = "../test/group_urltest_test.rs"]
mod tests;
