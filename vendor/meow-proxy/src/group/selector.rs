use super::selector_store::SelectorStore;
use async_trait::async_trait;
use meow_common::{
    AdapterType, DelayHistory, MeowError, Metadata, ProviderSlot, Proxy, ProxyAdapter, ProxyConn,
    ProxyHealth, ProxyPacketConn, ProxySelection, Result,
};
use parking_lot::RwLock;
use smol_str::SmolStr;
use std::sync::Arc;

pub struct SelectorGroup {
    name: SmolStr,
    static_proxies: Vec<Arc<dyn Proxy>>,
    provider_slots: Vec<ProviderSlot>,
    /// Name of the currently selected proxy; `None` means use the first.
    /// (Upstream prefers a `COMPATIBLE` member as the initial default;
    /// we keep the simpler first-member default — COMPATIBLE dials like
    /// DIRECT either way, so the divergence is cosmetic.)
    selected: RwLock<Option<SmolStr>>,
    /// Optional write-through persistence; primed at construction.
    store: Option<Arc<SelectorStore>>,
    health: ProxyHealth,
}

impl SelectorGroup {
    pub fn new(name: &str, proxies: Vec<Arc<dyn Proxy>>) -> Self {
        Self {
            name: SmolStr::from(name),
            static_proxies: proxies,
            provider_slots: Vec::new(),
            selected: RwLock::new(None),
            store: None,
            health: ProxyHealth::new(),
        }
    }

    pub fn new_with_providers(
        name: &str,
        proxies: Vec<Arc<dyn Proxy>>,
        slots: Vec<ProviderSlot>,
    ) -> Self {
        Self {
            name: SmolStr::from(name),
            static_proxies: proxies,
            provider_slots: slots,
            selected: RwLock::new(None),
            store: None,
            health: ProxyHealth::new(),
        }
    }

    /// Attach a persistent store. The previously-saved choice (if any) is
    /// loaded into `selected` immediately so the group dials it on first use.
    /// Subsequent successful `select()` calls flush back through the store.
    #[must_use]
    pub fn with_store(mut self, store: Arc<SelectorStore>) -> Self {
        if let Some(prev) = store.get(&self.name) {
            *self.selected.write() = Some(SmolStr::from(prev));
        }
        self.store = Some(store);
        self
    }

    fn contains_name(&self, name: &str) -> bool {
        if self.static_proxies.iter().any(|p| p.name() == name) {
            return true;
        }
        for slot in &self.provider_slots {
            let guard = slot.read();
            if guard.iter().any(|p| p.name() == name) {
                return true;
            }
        }
        false
    }

    /// Select the proxy with the given name. Returns `true` if found.
    /// Selection survives provider refreshes because it is stored by name.
    pub fn select(&self, name: &str) -> bool {
        if self.contains_name(name) {
            *self.selected.write() = Some(SmolStr::from(name));
            if let Some(store) = &self.store {
                store.set(&self.name, name);
            }
            true
        } else {
            false
        }
    }

    /// Resolve `selected` to an `Arc<dyn Proxy>` without allocating a unified
    /// `Vec`. Walks `static_proxies` then provider slots; falls back to the
    /// first proxy if `selected` is unset or names something no longer present.
    pub fn selected_proxy(&self) -> Option<Arc<dyn Proxy>> {
        let sel: Option<SmolStr> = self.selected.read().clone();
        let mut first_any: Option<Arc<dyn Proxy>> = None;
        if let Some(name) = sel {
            for p in &self.static_proxies {
                if first_any.is_none() {
                    first_any = Some(Arc::clone(p));
                }
                if p.name() == name {
                    return Some(Arc::clone(p));
                }
            }
            for slot in &self.provider_slots {
                let guard = slot.read();
                for p in guard.iter() {
                    if first_any.is_none() {
                        first_any = Some(Arc::clone(p));
                    }
                    if p.name() == name {
                        return Some(Arc::clone(p));
                    }
                }
            }
            return first_any;
        }
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

    pub fn proxy_names(&self) -> Vec<String> {
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
    /// `proxy_names` order.
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
impl ProxyAdapter for SelectorGroup {
    fn name(&self) -> &str {
        &self.name
    }

    fn adapter_type(&self) -> AdapterType {
        AdapterType::Selector
    }

    fn addr(&self) -> &str {
        ""
    }

    fn support_udp(&self) -> bool {
        self.selected_proxy().is_some_and(|p| p.support_udp())
    }

    async fn dial_tcp(&self, metadata: &Metadata) -> Result<Box<dyn ProxyConn>> {
        let proxy = self
            .selected_proxy()
            .ok_or_else(|| MeowError::Proxy("no proxy selected".into()))?;
        proxy.dial_tcp(metadata).await
    }

    async fn dial_udp(&self, metadata: &Metadata) -> Result<Box<dyn ProxyPacketConn>> {
        let proxy = self
            .selected_proxy()
            .ok_or_else(|| MeowError::Proxy("no proxy selected".into()))?;
        proxy.dial_udp(metadata).await
    }

    fn unwrap_proxy(&self, _metadata: &Metadata, _touch: bool) -> Option<Arc<dyn Proxy>> {
        self.selected_proxy()
    }

    fn health(&self) -> &ProxyHealth {
        &self.health
    }
}

impl Proxy for SelectorGroup {
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    fn alive(&self) -> bool {
        // The delegated half keeps a member dead-mark visible; the own
        // `health` half is what `record_dial_failure` escalation and probe
        // sweeps write — without it the write is unreadable and a dead
        // selector member wedges its parent group forever (issue #681).
        self.health.alive() && self.selected_proxy().is_some_and(|p| p.alive())
    }

    fn alive_for_url(&self, url: &str) -> bool {
        self.health.alive() && self.selected_proxy().is_some_and(|p| p.alive_for_url(url))
    }

    fn last_delay(&self) -> u16 {
        self.selected_proxy().map_or(0, |p| p.last_delay())
    }

    fn last_delay_for_url(&self, url: &str) -> u16 {
        self.selected_proxy()
            .map_or(0, |p| p.last_delay_for_url(url))
    }

    fn delay_history(&self) -> Vec<DelayHistory> {
        self.selected_proxy()
            .map(|p| p.delay_history())
            .unwrap_or_default()
    }

    fn members(&self) -> Option<Vec<String>> {
        Some(self.proxy_names())
    }

    fn member_proxies(&self) -> Option<Vec<Arc<dyn Proxy>>> {
        Some(self.member_proxy_list())
    }

    fn current(&self) -> Option<String> {
        self.selected_proxy().map(|p| p.name().into())
    }

    fn selection(&self) -> Option<&dyn ProxySelection> {
        Some(self)
    }
}

#[async_trait]
impl ProxySelection for SelectorGroup {
    async fn set(&self, name: &str) -> Result<()> {
        if self.select(name) {
            Ok(())
        } else {
            Err(MeowError::Proxy("proxy not exist".into()))
        }
    }

    fn force_set(&self, name: Option<&str>) {
        // SelectorGroup is a manual-pick group with no automatic fallback.
        // Clearing (None) is intentionally a no-op — a selector without a
        // selection has no sensible routing target. Automatic groups (UrlTest,
        // Fallback) handle None by reverting to auto mode.
        if let Some(name) = name {
            *self.selected.write() = Some(SmolStr::from(name));
            if let Some(store) = &self.store {
                store.set(&self.name, name);
            }
        }
    }

    fn fixed(&self) -> Option<String> {
        None
    }

    fn can_unfix(&self) -> bool {
        false
    }
}

#[cfg(test)]
#[path = "../test/group_selector_test.rs"]
mod tests;
