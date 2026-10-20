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
    system: system_proxy::Guard,
    system_journal: PathBuf,
    group_worker: Option<GroupWorker>,
}
impl State {
    async fn apply(&mut self, path: PathBuf, mut candidate: App) -> Result<()> {
        let old = self.current.load_full();
        tun::validate_egress(&candidate.config).await?;
        let mut keys = vec![];
        if let Some(addr) = &candidate.config.socks5_listen {
            keys.push(Key(Inbound::Socks5, addr.clone()));
        }
        if let Some(addr) = &candidate.config.http_listen {
            keys.push(Key(Inbound::HttpConnect, addr.clone()));
        }
        if let Some(settings) = &candidate.config.tproxy {
            keys.push(Key(Inbound::Tproxy, settings.listen.to_string()));
        }
        if let Some(settings) = &candidate.config.redirect {
            keys.push(Key(Inbound::Redirect, settings.listen.to_string()));
        }
        // All binds happen before committing. A failure drops only new resources.
        let mut prepared = vec![];
        for key in &keys {
            if !self.listeners.contains_key(key) {
                let reusable = self
                    .listeners
                    .iter()
                    .find(|(old, _)| {
                        old.1 == key.1
                            && matches!(old.0, Inbound::Socks5 | Inbound::HttpConnect)
                            && matches!(key.0, Inbound::Socks5 | Inbound::HttpConnect)
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
        self.group_worker.take();
        candidate.groups.inherit(&old, &candidate.config);
        candidate.groups.committed(&old, &candidate.config);
        self.current.store(Arc::new(candidate));
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
        self.path = path;
        self.text = text;
        self.generation = self.generation.wrapping_add(1);
        astra_ipc::control::applied(&self.path, &self.text);
        tracing::info!(config=%self.path.display(), "configuration committed; existing sessions retain their generation");
        Ok(())
    }
    async fn reload(&mut self, path: Option<PathBuf>) -> Result<()> {
        let path = std::fs::canonicalize(path.unwrap_or_else(|| self.path.clone()))?;
        let load_path = path.clone();
        let app = tokio::task::spawn_blocking(move || {
            App::load(&load_path).map(|app| crate::subscription::cached_app(&load_path, app))
        })
        .await??;
        self.apply(path, app).await
    }
}
pub async fn run(path: PathBuf, app: App) -> Result<()> {
    let current = Arc::new(ArcSwap::from_pointee(app));
    let mut state = State {
        generation: 0,
        system_journal: std::env::var_os("ASTRA_CONTROL_SOCKET")
            .map(PathBuf::from)
            .unwrap_or_else(|| path.clone())
            .with_extension("system-proxy-state.toml"),
        text: String::new(),
        path: path.clone(),
        current: current.clone(),
        listeners: HashMap::new(),
        group_worker: None,
        tunnel: None,
        tproxy_udp: None,
        system: Default::default(),
    };
    let first = crate::subscription::cached_app(&path, App::load(&path)?);
    state.apply(path, first).await?;
    let state = Arc::new(Mutex::new(state));
    let subscription_worker = {
        let state = state.clone();
        tokio::spawn(async move {
            let mut generation = 0;
            let mut effective = String::new();
            let mut ownership = None;
            let mut due = tokio::time::Instant::now();
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let (revision, path, base, settings) = {
                    let state = state.lock().await;
                    let settings = state.current.load().config.subscription.clone();
                    (
                        state.generation,
                        state.path.clone(),
                        state.text.clone(),
                        settings,
                    )
                };
                let Some(settings) = settings else {
                    continue;
                };
                if revision != generation {
                    generation = revision;
                    effective = base.clone();
                    ownership = Some(settings.clone());
                    if let Some(cache) = crate::subscription::read(&path, &base) {
                        if App::from_text(&path, cache.effective.clone()).is_ok() {
                            effective = cache.effective;
                            ownership = Some(cache.ownership);
                        }
                    }
                    due = tokio::time::Instant::now();
                }
                if tokio::time::Instant::now() < due {
                    continue;
                }
                due = tokio::time::Instant::now() + Duration::from_secs(settings.interval);
                let previous = effective.clone();
                let mut managed = ownership.clone().unwrap_or(settings.clone());
                let source_path = path.clone();
                let result = tokio::task::spawn_blocking(move || -> Result<_> {
                    let converted = astra_convert::subscription::fetch(
                        &settings,
                        source_path.parent().context("config has no parent")?,
                    )?;
                    let text = astra_convert::subscription::merge(
                        &previous,
                        &converted.proxy,
                        &mut managed,
                    )?;
                    let mut app = App::from_text(&source_path, text.clone())?;
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
                                nodes = managed.managed_nodes.len(),
                                "subscription updated"
                            );
                            ownership = Some(managed);
                            let cache = crate::subscription::Cache {
                                base: state.text.clone(),
                                effective: effective.clone(),
                                ownership: ownership.clone().unwrap(),
                            };
                            let cache_path = state.path.clone();
                            // Serialize after commit; a cache failure cannot discard valid nodes.
                            drop(state);
                            if !matches!(
                                tokio::task::spawn_blocking(move || crate::subscription::save(
                                    &cache_path,
                                    &cache
                                ))
                                .await,
                                Ok(Ok(()))
                            ) {
                                tracing::warn!(
                                    "subscription updated but node cache could not be saved"
                                );
                            }
                        }
                        Err(_) => {
                            tracing::warn!("subscription apply failed; previous nodes retained")
                        }
                    },
                    _ => tracing::warn!(
                        "subscription download or validation failed; previous nodes retained"
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
    let service = control.run_with_snapshot(
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
    state.tunnel.take();
    state.tproxy_udp.take();
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
        let Ok(permit) = app.permits.clone().acquire_owned().await else {
            return;
        };
        let _ = stream.set_nodelay(true);
        tokio::spawn(async move {
            let _permit = permit;
            let result = match inbound {
                Inbound::Socks5 => inbound::handle_socks5(stream, app).await,
                Inbound::HttpConnect => inbound::handle_http(stream, app).await,
                Inbound::Tproxy | Inbound::Redirect => transparent::tcp(stream, app, inbound).await,
                Inbound::Tun => unreachable!(),
            };
            if let Err(error) = result {
                tracing::debug!(%peer, %error, "session ended");
            }
        });
    }
}
