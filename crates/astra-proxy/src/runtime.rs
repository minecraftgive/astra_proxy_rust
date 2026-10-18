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
impl Drop for Listener {
    fn drop(&mut self) {
        self.task.abort();
    }
}
pub struct State {
    path: PathBuf,
    text: String,
    current: Current,
    listeners: HashMap<Key, Listener>,
    tunnel: Option<tun::Guard>,
    tproxy_udp: Option<transparent::UdpGuard>,
    system: system_proxy::Guard,
    system_journal: PathBuf,
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
                        TcpListener::bind(&key.1).await?
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
        let replace_system = candidate.config.system_proxy != old.config.system_proxy
            || (candidate.config.system_proxy.is_some()
                && (self.listeners.is_empty()
                    || candidate.config.socks5_listen != old.config.socks5_listen));
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
        self.current.store(Arc::new(candidate));
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
        astra_ipc::control::applied(&self.path, &self.text);
        tracing::info!(config=%self.path.display(), "configuration committed; existing sessions retain their generation");
        Ok(())
    }
    async fn reload(&mut self, path: Option<PathBuf>) -> Result<()> {
        let path = std::fs::canonicalize(path.unwrap_or_else(|| self.path.clone()))?;
        let load_path = path.clone();
        let app = tokio::task::spawn_blocking(move || App::load(&load_path)).await??;
        self.apply(path, app).await
    }
}
pub async fn run(path: PathBuf, app: App) -> Result<()> {
    let current = Arc::new(ArcSwap::from_pointee(app));
    let mut state = State {
        system_journal: std::env::var_os("ASTRA_CONTROL_SOCKET")
            .map(PathBuf::from)
            .unwrap_or_else(|| path.clone())
            .with_extension("system-proxy-state.toml"),
        text: String::new(),
        path: path.clone(),
        current: current.clone(),
        listeners: HashMap::new(),
        tunnel: None,
        tproxy_udp: None,
        system: Default::default(),
    };
    let first = App::load(&path)?;
    state.apply(path, first).await?;
    let state = Arc::new(Mutex::new(state));
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
    let service = control.run(|path| {
        let state = state.clone();
        async move {
            state
                .lock()
                .await
                .reload(path)
                .await
                .map_err(|e| format!("{e:#}"))
        }
    });
    let result = tokio::select! {
        result = service => result,
        _ = fatal.recv() => Err(std::io::Error::other("packet inbound stopped unexpectedly; see core log")),
    };
    watch.abort();
    let mut state = state.lock().await;
    state.listeners.clear();
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
