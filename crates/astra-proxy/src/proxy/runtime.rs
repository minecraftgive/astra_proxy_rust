mod controls;
use crate::{inbound, system_proxy, transparent, tun, App};
use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use astra_ipc::Inbound;
use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};
use tokio::{net::TcpListener, sync::Mutex, task::JoinHandle};

pub type Current = Arc<ArcSwap<App>>;
#[derive(Clone, Hash, PartialEq, Eq)]
struct Key(Inbound, String);
struct Listener {
    task: JoinHandle<()>,
    socket: Arc<TcpListener>,
}
struct GroupWorker(JoinHandle<()>);
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
    generation: u64,
    path: PathBuf,
    text: String,
    current: Current,
    listeners: HashMap<Key, Listener>,
    tunnel: Option<tun::Guard>,
    tproxy_udp: Option<transparent::UdpGuard>,
    dns: Option<crate::dns::Guard>,
    system: system_proxy::Guard,
    system_journal: PathBuf,
    group_worker: Option<GroupWorker>,
    outbound_interface: Option<meow_common::outbound_iface::OutboundIfaceGuard>,
}
impl State {
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
    async fn apply(&mut self, path: PathBuf, mut candidate: App) -> Result<()> {
        astra_ipc::progress::report("Prepare runtime", path.display().to_string());
        let old = self.current.load_full();
        candidate.metrics = old.metrics.clone();
        candidate.overrides = if self.generation > 0 && path != self.path {
            Arc::new(std::sync::RwLock::new(controls::load(&path)?))
        } else {
            old.overrides.clone()
        };
        {
            let overrides = candidate.overrides.read().unwrap();
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
        let dns_addresses = candidate
            .dns
            .as_ref()
            .map(|dns| dns.config.listen.clone())
            .unwrap_or_default();
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
                    self.system = system_proxy::Guard::apply(&old.config, &self.system_journal)
                        .context("restore old system proxy after failed switch")?;
                    return Err(error);
                }
            }
        }
        candidate.route_leases = if replace_tun {
            tunnel
                .as_ref()
                .map(|prepared| prepared.bypass_leases())
                .unwrap_or_default()
        } else {
            old.route_leases.clone()
        };
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
        candidate.groups.inherit(&old, &candidate.config);
        candidate.groups.committed(&old, &candidate.config);
        let egress_key = |tun: Option<&crate::mode_config::Tun>| {
            tun.map(|tun| (tun.interface.clone(), tun.gateway, tun.gateway6))
        };
        if egress_key(candidate.config.tun.as_ref()) == egress_key(old.config.tun.as_ref()) {
            candidate.egress_signal = old.egress_signal.clone();
        } else {
            old.egress_signal.send_replace(true);
        }
        self.current.store(Arc::new(candidate));
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
            .any(|g| g.mode == astra_ipc::groups::Mode::Auto)
        {
            let app = self.current.load_full();
            self.group_worker = Some(GroupWorker(tokio::spawn(async move {
                loop {
                    app.groups.refresh_all(&app).await;
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            })));
        }
        for (key, listener) in prepared {
            let task = tokio::spawn(accept(listener.clone(), key.0, self.current.clone()));
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
            let previous = self.tunnel.take();
            self.tunnel = tunnel.map(|prepared| prepared.start(self.current.clone(), previous));
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
    let system_journal = if let Some(socket) = std::env::var_os("ASTRA_CONTROL_SOCKET") {
        PathBuf::from(socket).with_extension("system-proxy-state.toml")
    } else {
        astra_ipc::storage::runtime_path(&path, "system-proxy")?
    };
    astra_ipc::storage::prepare_parent(&system_journal)?;
    *app.overrides.write().unwrap() = controls::load(&path)?;
    let current = Arc::new(ArcSwap::from_pointee(app));
    let mut state = State {
        generation: 0,
        system_journal,
        text: String::new(),
        path: path.clone(),
        current: current.clone(),
        listeners: HashMap::new(),
        group_worker: None,
        outbound_interface: None,
        tunnel: None,
        tproxy_udp: None,
        dns: None,
        system: Default::default(),
    };
    let first = crate::subscription::cached_app(&path, App::load(&path)?);
    state.apply(path, first).await?;
    let state = Arc::new(Mutex::new(state));
    let refresh = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let subscription_worker = {
        let refresh = refresh.clone();
        let state = state.clone();
        let current = current.clone();
        tokio::spawn(async move {
            let mut generation = 0;
            let mut requested = 0;
            let mut effective = String::new();
            let mut ownership = std::collections::BTreeMap::new();
            let mut due = std::collections::BTreeMap::new();
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
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
                let merge_source = source.clone();
                let source_path = path.clone();
                let download_app = current.load_full();
                let download_runtime = tokio::runtime::Handle::current();
                let result = tokio::task::spawn_blocking(move || -> Result<_> {
                    let converted = astra_convert::subscription::fetch_with(
                        &settings,
                        source_path.parent().context("config has no parent")?,
                        &|url, agent| {
                            download_runtime.block_on(crate::subscription::download(
                                &download_app,
                                url,
                                agent,
                            ))
                        },
                    )
                    .context("fetch subscription nodes")?;
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
                    continue;
                }
                match result {
                    Ok(Ok((text, managed, app))) => match state.apply(path, app).await {
                        Ok(()) => {
                            generation = state.generation;
                            effective = text;
                            tracing::info!(
                                subscription = %source,
                                nodes = managed.managed_nodes.len(),
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
                            }
                        }
                        Err(error) => {
                            tracing::warn!(subscription = %source, %endpoint,
                                elapsed_ms = started.elapsed().as_millis() as u64, retry_secs,
                                error = %astra_convert::subscription::diagnostics::error(&error),
                                "subscription apply failed; previous nodes retained")
                        }
                    },
                    Ok(Err(error)) => tracing::warn!(
                        subscription = %source, %endpoint,
                        elapsed_ms = started.elapsed().as_millis() as u64, retry_secs,
                        error = %astra_convert::subscription::diagnostics::error(&error),
                        "subscription download or validation failed; previous nodes retained"
                    ),
                    Err(error) => tracing::warn!(
                        subscription = %source, %endpoint,
                        elapsed_ms = started.elapsed().as_millis() as u64, retry_secs,
                        error = %astra_convert::subscription::diagnostics::error(&error.into()),
                        "subscription worker failed; previous nodes retained"
                    ),
                }
            }
        })
    };
    let control = astra_ipc::control::Control::from_env()?;
    let (failures, mut fatal) = tokio::sync::mpsc::channel(1);
    let watch = {
        let state = state.clone();
        tokio::spawn(async move {
            let mut previous_failure = String::new();
            let mut previous_egress_failure = String::new();
            let mut pending_egress = None;
            let mut egress_tick = 0u8;
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let mut state = state.lock().await;
                if state.tunnel.as_ref().is_some_and(tun::Guard::finished)
                    || state
                        .tproxy_udp
                        .as_ref()
                        .is_some_and(transparent::UdpGuard::finished)
                {
                    let _ = failures.send(()).await;
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
                Ok(astra_ipc::control::ConfigSnapshot {
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
                if matches!(command, astra_ipc::runtime::Command::Snapshot) {
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
        result = service => result,
        _ = fatal.recv() => Err(std::io::Error::other("packet inbound stopped unexpectedly; see core log")),
    };
    watch.abort();
    subscription_worker.abort();
    let mut state = state.lock().await;
    state.listeners.clear();
    state.group_worker.take();
    state.current.load().groups.stop_test();
    state.tunnel.take();
    state.tproxy_udp.take();
    state.dns.take();
    state.system.restore()?;
    let app = current.load_full();
    // Existing sessions retain their own Arc; drain the current generation.
    let _ = tokio::time::timeout(
        Duration::from_secs(app.config.shutdown_timeout_secs),
        app.permits.idle(),
    )
    .await;
    result.map_err(Into::into)
}
async fn accept(listener: Arc<TcpListener>, inbound: Inbound, current: Current) {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(%error, "TCP accept error");
                tokio::time::sleep(Duration::from_millis(100)).await;
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
            return;
        };
        let _ = stream.set_nodelay(true);
        tokio::spawn(async move {
            let _permit = permit;
            let cancellation = app.clone();
            let session = async {
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
                _=cancellation.egress_cancelled()=>Err(anyhow::anyhow!("physical egress changed; reconnect session")),
                result=session=>result,
            };
            if let Err(error) = result {
                tracing::debug!(%peer, %error, "session ended");
            }
        });
    }
}

#[cfg(test)]
mod egress_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    fn app() -> App {
        App::from_text(&std::env::temp_dir().join("astra-egress-session.toml"),"standalone=true\nstandalone_outbound='direct'\nsocks5_listen='127.0.0.1:1080'\n[proxy_groups.direct]\nmembers=['DIRECT']\n".into()).unwrap()
    }
    async fn client(proxy: std::net::SocketAddr, target: u16) -> tokio::net::TcpStream {
        let mut stream = tokio::net::TcpStream::connect(proxy).await.unwrap();
        stream.write_all(&[5, 1, 0]).await.unwrap();
        let mut reply = [0; 2];
        stream.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, [5, 0]);
        let mut request = vec![5, 1, 0, 1, 127, 0, 0, 1];
        request.extend_from_slice(&target.to_be_bytes());
        stream.write_all(&request).await.unwrap();
        let mut reply = [0; 10];
        stream.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[1], 0);
        stream
    }
    #[tokio::test]
    async fn egress_change_closes_old_tcp_sessions_and_accepts_fresh_ones() {
        let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_port = echo.local_addr().unwrap().port();
        let echo_task = tokio::spawn(async move {
            let mut jobs = tokio::task::JoinSet::new();
            loop {
                tokio::select! {Some(_)=jobs.join_next(),if !jobs.is_empty()=>{},accepted=echo.accept()=>{let (mut stream,_)=accepted.unwrap();jobs.spawn(async move{let (mut r,mut w)=stream.split();let _=tokio::io::copy(&mut r,&mut w).await;});}}
            }
        });
        let listener = Arc::new(TcpListener::bind("127.0.0.1:0").await.unwrap());
        let address = listener.local_addr().unwrap();
        let current = Arc::new(ArcSwap::from_pointee(app()));
        let previous = current.load_full();
        let listener_task = tokio::spawn(accept(listener, Inbound::Socks5, current.clone()));
        let mut old = client(address, echo_port).await;
        old.write_all(b"a").await.unwrap();
        assert_eq!(old.read_u8().await.unwrap(), b'a');
        current.store(Arc::new(app()));
        previous.egress_signal.send_replace(true);
        let mut byte = [0];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), old.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        let mut fresh = client(address, echo_port).await;
        fresh.write_all(b"b").await.unwrap();
        assert_eq!(fresh.read_u8().await.unwrap(), b'b');
        assert!(tokio::time::timeout(
            Duration::from_millis(20),
            current.load_full().egress_cancelled()
        )
        .await
        .is_err());
        listener_task.abort();
        echo_task.abort();
    }
}
