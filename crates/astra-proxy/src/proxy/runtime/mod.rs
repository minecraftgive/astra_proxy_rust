mod controls;
use crate::{inbound, system_proxy, transparent, tun, App};
use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use astra_model::Inbound;
use astra_platform::supervision::Task;
use meow_common::ErrorBackoff;
use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};
use tokio::{net::TcpListener, sync::Mutex};

pub type Current = Arc<ArcSwap<App>>;
#[derive(Clone, Hash, PartialEq, Eq)]
struct Key(Inbound, String);
struct Listener {
    task: Task,
    socket: Arc<TcpListener>,
}
struct GroupWorker(Task);
impl Drop for GroupWorker {
    fn drop(&mut self) {
        self.0.abort();
    }
}
impl Drop for Listener {
    fn drop(&mut self) {
        self.task.abort();
    }
}
pub struct State {
    consistency: Arc<std::sync::OnceLock<String>>,
    hosted: Option<Arc<crate::hosted::Host>>,
    generation: u64,
    path: PathBuf,
    text: String,
    current: Current,
    listeners: HashMap<Key, Listener>,
    /// Declared before `tunnel` so the OS resolver is restored first on drop.
    system_dns: Option<crate::platform::system_dns::Takeover>,
    tunnel: Option<tun::Guard>,
    tproxy_udp: Option<transparent::UdpGuard>,
    dns: Option<crate::dns::Guard>,
    system: system_proxy::Guard,
    system_journal: PathBuf,
    group_worker: Option<GroupWorker>,
    outbound_interface: Option<meow_common::outbound_iface::OutboundIfaceGuard>,
    elevated_tun: bool,
}
fn failed_system_proxy_switch(
    original: anyhow::Error,
    rollback: Result<system_proxy::Guard>,
    system: &mut system_proxy::Guard,
    consistency: &std::sync::OnceLock<String>,
) -> anyhow::Error {
    match rollback {
        Ok(previous) => {
            *system = previous;
            original
        }
        Err(rollback) => {
            let message = format!("System proxy switch failed: {original:#}; previous system proxy restoration also failed: {rollback:#}; outcome unknown; controlled shutdown required");
            let _ = consistency.set(message.clone());
            anyhow::anyhow!(message)
        }
    }
}
impl State {
    async fn task_failure(&mut self) -> Option<String> {
        if let Some(error) = self.consistency.get() {
            return Some(error.clone());
        }
        for listener in self.listeners.values_mut() {
            if let Some(error) = listener.task.check().await {
                return Some(error);
            }
        }
        if let Some(worker) = &mut self.group_worker {
            if let Some(error) = worker.0.check().await {
                return Some(error);
            }
        }
        if let Some(dns) = &mut self.dns {
            if let Some(error) = dns.failure().await {
                return Some(error);
            }
        }
        if let Some(host) = &self.hosted {
            if let Some(error) = host.task_failure().await {
                return Some(error);
            }
        }
        if let Some(tunnel) = &mut self.tunnel {
            if let Some(error) = tunnel.failure().await {
                return Some(error);
            }
        }
        if let Some(udp) = &mut self.tproxy_udp {
            if let Some(error) = udp.failure().await {
                return Some(error);
            }
        }
        None
    }
    async fn refresh_egress(
        &mut self,
        pending: &mut Option<crate::mode_config::Tun>,
    ) -> Result<bool> {
        let Some(mut observed) = self.current.load().config.tun.clone() else {
            *pending = None;
            return Ok(false);
        };
        if !observed.detects_interface() && !observed.auto_gateway {
            *pending = None;
            return Ok(false);
        }
        let previous = observed.clone();
        observed = tokio::task::spawn_blocking(move || -> Result<_> {
            crate::egress::prepare_interface(&mut observed)?;
            Ok(observed)
        })
        .await??;
        if !crate::egress::settled_update(&previous, observed, pending) {
            return Ok(false);
        }
        let path = self.path.clone();
        let text = self.text.clone();
        let candidate = tokio::task::spawn_blocking(move || {
            App::from_text(&path, text).map(|app| crate::subscription::cached_app(&path, app))
        })
        .await??;
        self.apply(self.path.clone(), candidate).await?;
        *pending = None;
        if let Some(tun) = &self.current.load().config.tun {
            tracing::info!(interface=?tun.interface,gateway=?tun.gateway,gateway6=?tun.gateway6,"physical egress changed; new connections use the updated interface");
        }
        Ok(true)
    }
    async fn apply(&mut self, path: PathBuf, candidate: App) -> Result<()> {
        self.apply_overrides(path, candidate, None).await
    }
    async fn apply_subscription(
        &mut self,
        path: PathBuf,
        text: &str,
        current_text: &str,
        candidate: App,
    ) -> Result<()> {
        if text == current_text {
            return Ok(());
        }
        self.apply(path, candidate).await
    }
    async fn apply_overrides(
        &mut self,
        path: PathBuf,
        mut candidate: App,
        overrides: Option<astra_model::runtime::Overrides>,
    ) -> Result<()> {
        if let Some(error) = self.consistency.get() {
            anyhow::bail!("{error}");
        }
        let override_checkpoint = overrides.clone();
        if let Some(host) = &self.hosted {
            host.ensure_consistent()?;
            anyhow::ensure!(
                !host.busy().await,
                "an update is prepared; retry configuration changes after activation"
            );
        }
        astra_ipc::progress::report("Prepare runtime", path.display().to_string());
        let old = self.current.load_full();
        candidate.metrics = old.metrics.clone();
        candidate.overrides = if let Some(overrides) = overrides {
            Arc::new(ArcSwap::from_pointee(overrides))
        } else if self.generation > 0 && path != self.path {
            Arc::new(ArcSwap::from_pointee(controls::load(&path)?))
        } else {
            old.overrides.clone()
        };
        {
            let overrides = candidate.overrides.load_full();
            crate::mode_config::runtime_modes(&mut candidate.config, &overrides);
        }
        let legacy_journal = path.with_extension("system-proxy-state.toml");
        anyhow::ensure!(candidate.config.system_proxy.is_none() || legacy_journal==self.system_journal || !legacy_journal.exists(),
            "Legacy system proxy recovery record exists: {}. Stop the old core and run --restore-system-proxy before enabling",legacy_journal.display());
        if let (Some(candidate_dns), Some(old_dns)) = (&mut candidate.dns, &old.dns) {
            candidate_dns.inherit(old_dns)?;
        } else if old.dns.as_ref().is_some_and(|dns| dns.config.fake_ip) && candidate.dns.is_none()
        {
            anyhow::bail!("disabling active Fake IP requires a proxy restart");
        }
        let mut dns_addresses = candidate
            .dns
            .as_ref()
            .map(|dns| dns.config.listen.clone())
            .unwrap_or_default();
        if let Some(local) = crate::platform::system_dns::local_listener(&candidate.config) {
            if !dns_addresses.contains(&local) {
                dns_addresses.push(local);
            }
        }
        let replace_dns = self.dns.as_ref().map(|guard| &guard.addresses) != Some(&dns_addresses);
        let dns_prepared = if replace_dns && !dns_addresses.is_empty() {
            Some(
                crate::dns::Prepared::bind(&dns_addresses, self.dns.as_ref())
                    .await
                    .context("bind DNS UDP/TCP listeners")?,
            )
        } else {
            None
        };
        if let Some(tun) = &mut candidate.config.tun {
            crate::egress::prepare_interface(tun)?;
        }
        crate::mode_config::validate(&candidate.config)?;
        let worker_generation = if let Some(host) = &self.hosted {
            candidate.hosted = Some(host.clone());
            candidate.route_leases = old.route_leases.clone();
            if let Some(dns) = &mut candidate.dns {
                dns.set_mapping_store(host.mappings.clone())?;
            }
            Some(host.prepare_runtime(&candidate).await?)
        } else {
            None
        };
        let mut keys = vec![];
        if let Some(addr) = &candidate.config.socks5_listen {
            keys.push(Key(Inbound::Socks5, addr.clone()));
        }
        if let Some(addr) = &candidate.config.http_listen {
            keys.push(Key(Inbound::HttpConnect, addr.clone()));
        }
        if let Some(addr) = &candidate.config.mixed_listen {
            keys.push(Key(Inbound::Mixed, addr.clone()));
        }
        if let Some(settings) = &candidate.config.tproxy {
            keys.push(Key(Inbound::Tproxy, settings.listen.to_string()));
        }
        if let Some(settings) = &candidate.config.redirect {
            keys.push(Key(Inbound::Redirect, settings.listen.to_string()));
        }
        // All binds happen before committing. A failure drops only new resources.
        astra_ipc::progress::report(
            "Bind listeners",
            format!("{} requested TCP listeners", keys.len()),
        );
        let mut prepared = vec![];
        for key in &keys {
            if !self.listeners.contains_key(key) {
                let reusable = self
                    .listeners
                    .iter()
                    .find(|(old, _)| {
                        old.1 == key.1
                            && matches!(
                                old.0,
                                Inbound::Socks5 | Inbound::HttpConnect | Inbound::Mixed
                            )
                            && matches!(
                                key.0,
                                Inbound::Socks5 | Inbound::HttpConnect | Inbound::Mixed
                            )
                    })
                    .map(|(_, value)| value.socket.clone());
                let listener = if let Some(socket) = reusable {
                    socket
                } else {
                    Arc::new(if matches!(key.0, Inbound::Tproxy | Inbound::Redirect) {
                        transparent::tcp_listener(&key.1, key.0 == Inbound::Tproxy)?
                    } else {
                        TcpListener::bind(&key.1)
                            .await
                            .with_context(|| format!("bind listener {}", key.1))?
                    })
                };
                prepared.push((key.clone(), listener));
            }
        }
        astra_ipc::progress::report(
            "Prepare network modes",
            "Preparing TUN, DNS and system proxy resources",
        );
        let replace_tun = candidate.config.tun != old.config.tun
            || (candidate.config.tun.is_some() && self.tunnel.is_none());
        let tunnel = if replace_tun {
            match &candidate.config.tun {
                Some(config) => Some(tun::prepare(config.clone(), self.tunnel.as_ref()).await?),
                None => None,
            }
        } else {
            None
        };
        let replace_udp = candidate.config.tproxy != old.config.tproxy
            || (candidate.config.tproxy.is_some() && self.tproxy_udp.is_none());
        let udp = if replace_udp {
            match &candidate.config.tproxy {
                Some(config) => Some(transparent::prepare_udp(config.listen)?),
                None => None,
            }
        } else {
            None
        };
        let system_listener_changed =
            crate::platform::system_listener_changed(&candidate.config, &old.config);
        let replace_system = candidate.config.system_proxy != old.config.system_proxy
            || (candidate.config.system_proxy.is_some()
                && (self.listeners.is_empty() || system_listener_changed));
        let text = candidate.source_text.clone();
        candidate.route_leases = if replace_tun {
            tunnel
                .as_ref()
                .map(|prepared| prepared.bypass_leases())
                .unwrap_or_default()
        } else {
            old.route_leases.clone()
        };
        // This is the last await before publishing live state. In particular,
        // system settings, limits, session cancellation and group workers must
        // remain unchanged while admission is held by another host operation.
        let worker_commit = match worker_generation {
            Some(generation) => Some(
                generation
                    .lock_commit(candidate.route_leases.clone())
                    .await?,
            ),
            None => None,
        };
        let mut override_checkpoint = override_checkpoint
            .as_ref()
            .map(|next| self.begin_override_checkpoint(&path, next))
            .transpose()?;
        // This is the default binding; a group can override it per connection.
        // A rejected reload drops the new
        // owner and restores the previous owner; old sessions keep their sockets.
        let outbound_interface = candidate
            .config
            .tun
            .as_ref()
            .and_then(|tun| tun.interface.as_deref())
            .map(meow_common::outbound_iface::install_outbound_interface)
            .transpose()?;
        if replace_system {
            self.system
                .restore()
                .context("restore previous system proxy")?;
            match system_proxy::Guard::apply(&candidate.config, &self.system_journal) {
                Ok(guard) => self.system = guard,
                Err(error) => {
                    return Err(failed_system_proxy_switch(
                        error,
                        system_proxy::Guard::apply(&old.config, &self.system_journal),
                        &mut self.system,
                        &self.consistency,
                    ));
                }
            }
        }
        candidate.permits = old.permits.clone();
        candidate.udp_permits = old.udp_permits.clone();
        candidate.permits.resize(candidate.config.max_connections);
        candidate.udp_permits.resize(candidate.config.max_udp_flows);
        if let Some(client) = &mut candidate.rule_client {
            client.start_observer();
        }
        // Prepare first; disconnect sessions only once the entire reload can commit.
        astra_ipc::progress::report(
            "Commit runtime",
            "Resources ready; switching the active generation",
        );
        self.group_worker.take();
        {
            let routing = &mut *candidate;
            routing.groups.inherit(&old, &routing.config);
        }
        candidate.groups.committed(&old, &candidate.config);
        let egress_key = |tun: Option<&crate::mode_config::Tun>| {
            tun.map(|tun| (tun.interface.clone(), tun.gateway, tun.gateway6))
        };
        if egress_key(candidate.config.tun.as_ref()) == egress_key(old.config.tun.as_ref()) {
            candidate.egress_signal = old.egress_signal.clone();
        } else {
            old.egress_signal.send_replace(true);
        }
        let candidate = Arc::new(candidate);
        if let Some(generation) = worker_commit {
            generation.commit(|| self.current.store(candidate));
        } else {
            self.current.store(candidate);
        }
        if replace_dns {
            self.dns = dns_prepared.map(|prepared| prepared.start(self.current.clone()));
        }
        self.outbound_interface = outbound_interface;
        self.group_worker.take();
        if self
            .current
            .load()
            .config
            .proxy_groups
            .values()
            .any(|g| g.mode == astra_model::groups::Mode::Auto)
        {
            let app = self.current.load_full();
            self.group_worker = Some(GroupWorker(Task::spawn(
                "proxy group refresh",
                async move {
                    loop {
                        app.groups.refresh_all(&app).await;
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                },
            )));
        }
        for (key, listener) in prepared {
            let task = Task::spawn_result(
                format!("{:?} TCP listener {}", key.0, key.1),
                accept(listener.clone(), key.0, self.current.clone()),
            );
            self.listeners.insert(
                key,
                Listener {
                    task,
                    socket: listener,
                },
            );
        }
        self.listeners.retain(|key, _| keys.contains(key));
        if replace_tun {
            // Restore the OS resolver while the old TUN can still answer.
            self.system_dns = None;
            let previous = self.tunnel.take();
            self.tunnel = tunnel.map(|prepared| prepared.start(self.current.clone(), previous));
        }
        let wanted = crate::platform::system_dns::Key::wanted(
            &self.current.load().config,
            self.tunnel.as_ref().and_then(|tunnel| tunnel.name().ok()),
        );
        if self.system_dns.as_ref().map(|takeover| &takeover.key) != wanted.as_ref() {
            self.system_dns = None;
            if let Some(key) = wanted {
                self.system_dns = Some(crate::platform::system_dns::Takeover::start(key).await);
            }
        }
        if replace_udp {
            self.tproxy_udp = udp.map(|prepared| prepared.start(self.current.clone()));
        }
        if let Some(tun) = &self.current.load().config.tun {
            if let Err(error) = crate::egress::cache_interface(&path, tun) {
                tracing::warn!(%error,"could not save diagnostic egress cache");
            }
        }
        self.path = path;
        self.text = text;
        self.generation = self.generation.wrapping_add(1);
        astra_ipc::control::applied(&self.path, &self.text);
        if let Some(checkpoint) = &mut override_checkpoint {
            checkpoint.commit();
        }
        tracing::info!(config=%self.path.display(), "configuration committed; existing sessions retain their generation");
        Ok(())
    }
    async fn reload(&mut self, path: Option<PathBuf>) -> Result<()> {
        let path = std::fs::canonicalize(path.unwrap_or_else(|| self.path.clone()))?;
        astra_ipc::progress::report("Read configuration", path.display().to_string());
        let load_path = path.clone();
        let app = tokio::task::spawn_blocking(move || {
            App::load(&load_path).map(|app| crate::subscription::cached_app(&load_path, app))
        })
        .await??;
        self.apply(path, app).await
    }
}
pub async fn run(path: PathBuf, app: App) -> Result<()> {
    run_inner(path, app, None).await
}

/// An elevated core is stopped through its lifeline to the unprivileged
/// manager. A committed core survives loss of the manager connection; only
/// an authenticated explicit shutdown request fires this stop receiver.
pub async fn run_with_lifeline(
    path: PathBuf,
    app: App,
    stop: tokio::sync::oneshot::Receiver<()>,
) -> Result<()> {
    run_inner(path, app, Some(stop)).await
}

async fn lifeline_closed(lifeline: &mut Option<tokio::sync::oneshot::Receiver<()>>) {
    match lifeline {
        Some(stop) => {
            if stop.await.is_err() {
                // The committed core keeps serving when its manager detaches.
                *lifeline = None;
                std::future::pending::<()>().await;
            }
        }
        None => std::future::pending().await,
    }
}

async fn run_inner(
    path: PathBuf,
    app: App,
    mut lifeline: Option<tokio::sync::oneshot::Receiver<()>>,
) -> Result<()> {
    crate::platform::file_limit::prepare();
    crate::platform::network_recovery::set_profile(&path);
    crate::platform::network_recovery::set_enabled(app.config.experimental.network_recovery);
    // Claim management ownership before preparing any OS takeover resources.
    let control = astra_ipc::control::Control::from_env()?;
    crate::platform::network_recovery::recover_abandoned()
        .context("recover previous network settings before startup")?;
    let system_journal = if let Some(socket) = std::env::var_os("ASTRA_CONTROL_SOCKET") {
        PathBuf::from(socket).with_extension("system-proxy-state.toml")
    } else {
        astra_platform::storage::runtime_path(&path, "system-proxy")?
    };
    astra_platform::storage::prepare_parent(&system_journal)?;
    let mut overrides = controls::load(&path)?;
    let elevated_tun = crate::ELEVATED_TUN.swap(false, std::sync::atomic::Ordering::SeqCst);
    if elevated_tun {
        overrides.tun = Some(true);
    }
    app.overrides.store(Arc::new(overrides));
    let current = Arc::new(ArcSwap::from_pointee(app));
    // Node hostnames resolved inside protocol adapters follow the current
    // generation's encrypted resolver instead of the system resolver.
    let hook_current = current.clone();
    meow_common::set_host_resolver(Arc::new(crate::dns::HostHook(Box::new(move || {
        hook_current.load_full()
    }))));
    let hosted = if std::env::var_os("ASTRA_HOSTED_EXPERIMENTAL").is_some() {
        Some(crate::hosted::Host::start(path.clone(), &current).await?)
    } else {
        None
    };
    let mut state = State {
        consistency: Default::default(),
        hosted,
        generation: 0,
        system_journal,
        text: String::new(),
        path: path.clone(),
        current: current.clone(),
        listeners: HashMap::new(),
        group_worker: None,
        outbound_interface: None,
        elevated_tun,
        system_dns: None,
        tunnel: None,
        tproxy_udp: None,
        dns: None,
        system: Default::default(),
    };
    let first = crate::subscription::cached_app(&path, App::load(&path)?);
    state.apply(path, first).await?;
    let state = Arc::new(Mutex::new(state));
    let refresh = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let mut subscription_worker = {
        let refresh = refresh.clone();
        let state = state.clone();
        let current = current.clone();
        Task::spawn("subscription refresh", async move {
            let mut generation = 0;
            let mut requested = 0;
            let mut effective = String::new();
            let mut ownership = std::collections::BTreeMap::new();
            let mut due = std::collections::BTreeMap::new();
            // First tick fires immediately: an uncached subscription starts
            // downloading at launch instead of one second later.
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                let (revision, path, base, settings) = {
                    let state = state.lock().await;
                    let settings = state.current.load().config.subscription_sources();
                    (
                        state.generation,
                        state.path.clone(),
                        state.text.clone(),
                        settings,
                    )
                };
                if settings.is_empty() {
                    continue;
                }
                if revision != generation {
                    generation = revision;
                    effective = base.clone();
                    ownership = settings.clone();
                    if let Some(cache) = crate::subscription::read(&path, &base) {
                        let cached = crate::subscription::effective_text(&cache)
                            .unwrap_or_else(|_| cache.effective.clone());
                        if App::from_text(&path, cached.clone()).is_ok() {
                            effective = cached;
                            ownership = cache.subscriptions;
                            if ownership.is_empty() {
                                ownership.insert("__legacy__".into(), cache.ownership);
                            }
                        }
                    }
                    due.clear();
                }
                let request = refresh.load(std::sync::atomic::Ordering::Relaxed);
                if request != requested {
                    due.clear();
                    requested = request;
                }
                let Some((source, settings)) = settings
                    .iter()
                    .find(|(name, _)| {
                        due.get(*name)
                            .is_none_or(|time| tokio::time::Instant::now() >= *time)
                    })
                    .map(|(name, settings)| (name.clone(), settings.clone()))
                else {
                    continue;
                };
                due.insert(
                    source.clone(),
                    tokio::time::Instant::now() + Duration::from_secs(settings.interval),
                );
                let previous = effective.clone();
                let started = std::time::Instant::now();
                let endpoint = astra_convert::subscription::diagnostics::endpoint(&settings.url);
                let retry_secs = settings.interval;
                let mut managed = ownership.get(&source).cloned().unwrap_or(settings.clone());
                let previous_nodes = managed.managed_nodes.len();
                tracing::info!(
                    subscription = %source, %endpoint,
                    proxy = %settings.proxy, import_groups = settings.import_groups,
                    previous_nodes, retry_secs,
                    "subscription update started"
                );
                let merge_source = source.clone();
                let source_path = path.clone();
                let download_app = current.load_full();
                let download_runtime = tokio::runtime::Handle::current();
                let span = tracing::info_span!("subscription_update", subscription = %source);
                let result = tokio::task::spawn_blocking(move || -> Result<_> {
                    let _entered = span.enter();
                    let converted = astra_convert::subscription::fetch_with(
                        &settings,
                        source_path.parent().context("config has no parent")?,
                        &|url, agent| {
                            download_runtime.block_on(crate::subscription::download(
                                &download_app,
                                url,
                                agent,
                                &settings.proxy,
                            ))
                        },
                    )
                    .context("fetch subscription nodes")?;
                    tracing::info!(
                        subscription = %merge_source, nodes = converted.nodes,
                        warnings = converted.warnings.len(),
                        "subscription converted"
                    );
                    for warning in converted.warnings.iter().take(50) {
                        tracing::warn!(
                            subscription = %merge_source,
                            detail = %astra_convert::subscription::diagnostics::error(&anyhow::anyhow!(warning.clone())),
                            "subscription conversion warning"
                        );
                    }
                    if converted.warnings.len() > 50 {
                        tracing::warn!(subscription = %merge_source,
                            omitted = converted.warnings.len() - 50,
                            "additional subscription conversion warnings omitted");
                    }
                    let text = astra_convert::subscription::merge_named(
                        &previous,
                        &converted.proxy,
                        &mut managed,
                        &merge_source,
                    )
                    .context("merge subscription with local configuration")?;
                    let mut app = App::from_text(&source_path, text.clone())
                        .context("validate merged subscription configuration")?;
                    // Management/editor snapshots describe the saved local configuration.
                    app.source_text = base.clone();
                    Ok((text, managed, app))
                })
                .await;
                let mut state = state.lock().await;
                if state.generation != revision {
                    tracing::info!(subscription = %source, %endpoint,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        "subscription result discarded because configuration changed");
                    continue;
                }
                match result {
                    Ok(Ok((text, managed, app))) => match state
                        .apply_subscription(path, &text, &effective, app)
                        .await
                    {
                        Ok(()) => {
                            generation = state.generation;
                            effective = text;
                            tracing::info!(
                                subscription = %source,
                                %endpoint, previous_nodes,
                                nodes = managed.managed_nodes.len(),
                                groups = managed.managed_groups.len(),
                                elapsed_ms = started.elapsed().as_millis() as u64, retry_secs,
                                "subscription updated"
                            );
                            ownership.insert(source.clone(), managed);
                            let cache = crate::subscription::Cache {
                                base: state.text.clone(),
                                effective: effective.clone(),
                                ownership: ownership.get("__legacy__").cloned().unwrap_or_default(),
                                subscriptions: ownership.clone(),
                            };
                            let cache_path = state.path.clone();
                            // Serialize after commit; a cache failure cannot discard valid nodes.
                            drop(state);
                            let saved = tokio::task::spawn_blocking(move || {
                                crate::subscription::save(&cache_path, &cache)
                            })
                            .await
                            .context("subscription cache worker failed")
                            .and_then(|result| result);
                            if let Err(error) = saved {
                                tracing::warn!(
                                    subscription = %source,
                                    error = %astra_convert::subscription::diagnostics::error(&error),
                                    "subscription updated but node cache could not be saved"
                                );
                            } else {
                                tracing::info!(subscription = %source,
                                    elapsed_ms = started.elapsed().as_millis() as u64,
                                    "subscription node cache saved");
                            }
                        }
                        Err(error) => {
                            tracing::warn!(subscription = %source, %endpoint, previous_nodes,
                                elapsed_ms = started.elapsed().as_millis() as u64, retry_secs,
                                error = %astra_convert::subscription::diagnostics::error(&error),
                                "subscription apply failed; previous nodes retained")
                        }
                    },
                    Ok(Err(error)) => tracing::warn!(
                        subscription = %source, %endpoint, previous_nodes,
                        elapsed_ms = started.elapsed().as_millis() as u64, retry_secs,
                        error = %astra_convert::subscription::diagnostics::error(&error),
                        "subscription download or validation failed; previous nodes retained"
                    ),
                    Err(error) => tracing::warn!(
                        subscription = %source, %endpoint, previous_nodes,
                        elapsed_ms = started.elapsed().as_millis() as u64, retry_secs,
                        error = %astra_convert::subscription::diagnostics::error(&error.into()),
                        "subscription worker failed; previous nodes retained"
                    ),
                }
            }
        })
    };
    let (failures, mut fatal) = tokio::sync::mpsc::channel(1);
    let mut watch = {
        let state = state.clone();
        Task::spawn("runtime watchdog", async move {
            let mut previous_failure = String::new();
            let mut previous_egress_failure = String::new();
            let mut pending_egress = None;
            let mut egress_tick = 0u8;
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let mut state = state.lock().await;
                if let Err(error) = crate::platform::network_recovery::healthy() {
                    let _ = failures
                        .send(format!("network recovery assistant failed: {error:#}"))
                        .await;
                    break;
                }
                if let Some(error) = state.task_failure().await {
                    let _ = failures.send(error).await;
                    break;
                }
                egress_tick = (egress_tick + 1) % 2;
                if egress_tick == 0 {
                    match state.refresh_egress(&mut pending_egress).await {
                        Ok(_) => previous_egress_failure.clear(),
                        Err(error) => {
                            pending_egress = None;
                            let detail = format!("{error:#}");
                            if detail != previous_egress_failure {
                                tracing::warn!(error=%detail,"egress refresh deferred; previous configuration retained, retrying when network recovers");
                                previous_egress_failure = detail;
                            }
                        }
                    }
                }
                if !state.current.load().config.watch_config {
                    continue;
                }
                let Ok(text) = tokio::fs::read_to_string(&state.path).await else {
                    continue;
                };
                if text == state.text || text == previous_failure {
                    continue;
                }
                if let Err(error) = state.reload(None).await {
                    previous_failure = text;
                    tracing::warn!(%error, "automatic reload rejected; current configuration retained");
                } else {
                    previous_failure.clear();
                }
            }
        })
    };
    let lookup_current = current.clone();
    let control = control
        .with_capabilities(vec![
            astra_model::control::NETWORK_DIAGNOSTICS_STREAM_CAPABILITY.into(),
            astra_model::control::NETWORK_DIAGNOSTICS_PINNED_CAPABILITY.into(),
            astra_model::control::NETWORK_DIAGNOSTICS_RESOLUTION_CAPABILITY.into(),
        ])
        .with_fake_ip_lookup(move |addresses| {
            let app = lookup_current.load_full();
            async move {
                addresses
                    .into_iter()
                    .map(|address| {
                        let domain = app
                            .dns
                            .as_ref()
                            .map(|dns| dns.lookup_fake(address))
                            .transpose()
                            .map_err(|error| format!("{error:#}"))?
                            .flatten();
                        Ok(astra_model::control::FakeIpMapping { address, domain })
                    })
                    .collect()
            }
        });
    let control = if state.lock().await.hosted.is_none() {
        let state = state.clone();
        let started = std::time::Instant::now();
        let sequence = Arc::new(std::sync::atomic::AtomicU64::new(0));
        control.with_runtime_telemetry(move || {
            let state = state.clone();
            let sequence = sequence.clone();
            async move {
                use astra_model::runtime::{
                    Coverage, DnsResolverStatus, RuntimeTelemetry, RuntimeView,
                };
                let state = state.lock().await;
                let app = state.current.load_full();
                let view = RuntimeView {
                    snapshot: controls::app_snapshot(&app),
                    telemetry: Some(RuntimeTelemetry {
                        host_boot_id: astra_platform::lifecycle::boot_id().into(),
                        active_generation: Some(state.generation),
                        sample_sequence: sequence
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                            + 1,
                        sample_elapsed_ms: started.elapsed().as_millis() as u64,
                        coverage: Coverage::Complete,
                        dns: Some(Box::new(
                            app.dns
                                .as_ref()
                                .map(|dns| dns.query_log())
                                .unwrap_or_default(),
                        )),
                        dns_resolver: Some(Box::new(DnsResolverStatus::from_config(
                            app.dns.as_ref().map(|dns| &dns.config),
                        ))),
                        ..Default::default()
                    }),
                };
                serde_json::to_string(&view).map_err(|error| error.to_string())
            }
        })
    } else {
        control
    };
    let service = control.run_with_runtime(
        |path| {
            let state = state.clone();
            async move {
                state
                    .lock()
                    .await
                    .reload(path)
                    .await
                    .map_err(|e| format!("{e:#}"))
            }
        },
        || {
            let state = state.clone();
            async move {
                let state = state.lock().await;
                Ok(astra_model::control::ConfigSnapshot {
                    api_version: astra_ipc::API_VERSION,
                    pid: std::process::id(),
                    path: state.path.clone(),
                    text: state.text.clone(),
                })
            }
        },
        |_| async { Err("Route explanation is not supported by Proxy Core".into()) },
        || {
            let refresh = refresh.clone();
            let state = state.clone();
            async move {
                if state
                    .lock()
                    .await
                    .current
                    .load()
                    .config
                    .subscription_sources()
                    .is_empty()
                {
                    return Err("No subscriptions configured".into());
                }
                refresh.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            }
        },
        |command| {
            let state = state.clone();
            let current = current.clone();
            async move {
                if matches!(command, astra_model::runtime::Command::Snapshot) {
                    return Ok(controls::snapshot(&current));
                }
                state
                    .lock()
                    .await
                    .runtime_control(command)
                    .await
                    .map_err(|error| format!("{error:#}"))
            }
        },
    );
    let result = tokio::select! {
        biased;
        reason = fatal.recv() => Err(std::io::Error::other(reason.unwrap_or_else(|| "runtime watchdog channel closed".into()))),
        result = service => result,
        reason = watch.failure() => Err(std::io::Error::other(reason)),
        reason = subscription_worker.failure() => Err(std::io::Error::other(reason)),
        _ = lifeline_closed(&mut lifeline) => {
            tracing::info!("authenticated lifecycle stop requested");
            Ok(())
        }
    };
    watch.abort();
    subscription_worker.abort();
    let mut state = state.lock().await;
    state.listeners.clear();
    state.group_worker.take();
    state.current.load().groups.stop_test();
    state.system_dns.take();
    state.tunnel.take();
    state.tproxy_udp.take();
    state.dns.take();
    let restore = state.system.restore();
    let app = current.load_full();
    // Existing sessions retain their permits; drain the shared connection quota.
    let _ = tokio::time::timeout(
        Duration::from_secs(app.config.shutdown_timeout_secs),
        app.permits.idle(),
    )
    .await;
    match (result, restore) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error.into()),
        (Ok(()), Err(error)) => {
            Err(error.context("system proxy cleanup failed; recovery remains pending"))
        }
        (Err(error), Err(cleanup)) => Err(anyhow::anyhow!(
            "{error}; system proxy cleanup also failed: {cleanup:#}"
        )),
    }
}
async fn accept(
    listener: Arc<TcpListener>,
    inbound: Inbound,
    current: Current,
) -> std::io::Result<()> {
    let mut backoff = ErrorBackoff::new();
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(value) => {
                backoff.succeeded();
                value
            }
            Err(error) => {
                if !astra_platform::supervision::transient_io(&error) {
                    return Err(error);
                }
                if backoff.failed(&error).await {
                    tracing::warn!(%error, ?inbound, "TCP accept error");
                } else {
                    tracing::debug!(%error, ?inbound, "TCP accept error");
                }
                continue;
            }
        };
        let app = current.load_full();
        if matches!(
            inbound,
            Inbound::Socks5 | Inbound::HttpConnect | Inbound::Mixed
        ) && !app.inbound_allowed(peer.ip())
        {
            continue;
        }
        let Ok(permit) = app.permits.clone().acquire_owned().await else {
            return Ok(());
        };
        let _ = stream.set_nodelay(true);
        tokio::spawn(async move {
            let _permit = permit;
            let cancellation = app.egress_signal.clone();
            let session = async move {
                if let Some(host) = app.hosted.clone() {
                    drop(app);
                    let local = stream.local_addr()?;
                    let destination = match inbound {
                        Inbound::Tproxy | Inbound::Redirect => {
                            Some(transparent::destination(&stream, inbound)?)
                        }
                        _ => None,
                    };
                    let process = crate::platform::process::lookup(
                        astra_model::Network::Tcp,
                        peer,
                        destination.unwrap_or(local),
                    )
                    .await;
                    return host
                        .relay(
                            stream,
                            crate::hosted::Metadata {
                                flow_id: None,
                                inbound,
                                source: peer,
                                local,
                                destination,
                                process,
                            },
                        )
                        .await;
                }
                match inbound {
                    Inbound::Socks5 => inbound::handle_socks5(stream, app).await,
                    Inbound::HttpConnect => inbound::handle_http(stream, app).await,
                    Inbound::Mixed => inbound::handle_mixed(stream, app).await,
                    Inbound::Tproxy | Inbound::Redirect => {
                        transparent::tcp(stream, app, inbound).await
                    }
                    Inbound::Tun => unreachable!(),
                }
            };
            let result = tokio::select! {
                biased;
                _=cancellation.cancelled()=>Err(anyhow::anyhow!("physical egress changed; reconnect session")),
                result=session=>result,
            };
            if let Err(error) = result {
                tracing::debug!(%peer, %error, "session ended");
            }
        });
    }
}

#[cfg(test)]
#[path = "../../test/proxy_runtime_egress_test.rs"]
mod egress_tests;
