//! Resident ingress and immutable worker generations. Public listeners never
//! move during an update; each admitted flow owns a generation lease.
use crate::platform::worker::WorkerChild;
use crate::{runtime::Current, App};
use anyhow::{bail, ensure, Context, Result};
use astra_model::Inbound;
use astra_model::Target;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, Weak,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::Mutex,
};

mod connection_ids;
mod dns;
mod telemetry;
#[cfg(test)]
mod transition_tests;
mod udp;
tokio::task_local! { pub(crate) static INGRESS_PROCESS: Option<astra_model::ProcessInfo>; }
pub use astra_ipc::host::receive_message as receive;
pub use astra_ipc::host::request_json_control as request;
pub use astra_ipc::host::send_message as send;
pub use astra_model::host_control::{endpoint, Command, GenerationStatus, Status};

#[derive(Clone, Serialize, Deserialize)]
pub struct Metadata {
    #[serde(default)]
    pub flow_id: Option<astra_ipc::host::FlowId>,
    pub inbound: Inbound,
    pub source: SocketAddr,
    pub local: SocketAddr,
    pub destination: Option<SocketAddr>,
    pub process: Option<astra_model::ProcessInfo>,
}
#[derive(Clone, Serialize, Deserialize)]
struct WorkerConfig {
    path: PathBuf,
    text: String,
    overrides: astra_model::runtime::Overrides,
    interface: Option<String>,
    host: PathBuf,
    token: String,
    endpoint: PathBuf,
    generation: u64,
    boot: String,
}
#[derive(Serialize, Deserialize)]
enum WorkerRequest {
    Ready,
    Tcp(Metadata),
    Udp(Metadata),
    Snapshot,
    Disconnect(Option<u64>),
    Overrides {
        value: astra_model::runtime::Overrides,
        disconnect: Option<String>,
    },
    DisconnectGroup(String),
    TestGroup(String),
    Active(bool),
    Retire,
    Probe,
    Dns(Vec<u8>),
    DnsTcp,
}
#[derive(Serialize, Deserialize)]
struct Auth<T> {
    token: String,
    command: T,
}

struct Generation {
    id: u64,
    directory: PathBuf,
    endpoint: PathBuf,
    token: String,
    log: PathBuf,
    child: Mutex<WorkerChild>,
    flows: AtomicUsize,
    rule_socket: String,
    snapshot: std::sync::Mutex<astra_model::runtime::Snapshot>,
    failed: std::sync::atomic::AtomicBool,
    route_leases: std::sync::Mutex<Vec<Arc<crate::tun::OwnedRoute>>>,
}
struct Lease(Arc<Generation>);
impl Drop for Lease {
    fn drop(&mut self) {
        self.0.flows.fetch_sub(1, Ordering::AcqRel);
    }
}
struct State {
    active: u64,
    prepared: Option<u64>,
    paused: bool,
    generations: BTreeMap<u64, Arc<Generation>>,
}
struct Preparation {
    state: Arc<Mutex<State>>,
    id: u64,
    committed: bool,
    tokens: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    token: Option<String>,
    pid: Option<u32>,
}
pub(crate) struct CandidateGuard {
    host: Arc<Host>,
    id: u64,
    committed: bool,
    transition: Option<tokio::sync::OwnedMutexGuard<()>>,
}
impl CandidateGuard {
    /// Readiness and Active(true) have already succeeded while the transition
    /// guard was held. Publish runtime and generation under the admission lock;
    /// nothing fallible remains in this commit.
    pub async fn commit(
        mut self,
        route_leases: Vec<Arc<crate::tun::OwnedRoute>>,
        publish: impl FnOnce(),
    ) {
        let old = {
            let mut state = self.host.state.lock().await;
            debug_assert_eq!(state.prepared, Some(self.id));
            debug_assert!(state.generations.contains_key(&self.id));
            *state.generations[&self.id].route_leases.lock().unwrap() = route_leases;
            let old = state.generations.get(&state.active).cloned();
            publish();
            state.prepared = None;
            state.active = self.id;
            old.filter(|old| old.id != self.id)
        };
        self.committed = true;
        // Keep ordering with subsequent rollback/activation, without delaying
        // the runtime commit on an old worker's response.
        let transition = self.transition.take();
        tokio::spawn(async move {
            let _transition = transition;
            if let Some(old) = old {
                let _ = Host::rpc::<bool>(&old, WorkerRequest::Active(false)).await;
            }
        });
    }
}
impl Drop for CandidateGuard {
    fn drop(&mut self) {
        if !self.committed {
            // Abort acquires the same lock; release it before scheduling cleanup.
            self.transition.take();
            let host = self.host.clone();
            let generation = self.id;
            tokio::spawn(async move {
                let _ = host.command(Command::Abort { generation }).await;
            });
        }
    }
}
impl Drop for Preparation {
    fn drop(&mut self) {
        if !self.committed {
            if let Some(token) = &self.token {
                self.tokens.lock().unwrap().remove(token);
            }
            if let Some(pid) = self.pid {
                crate::platform::process::unregister_worker(pid);
            }
            let state = self.state.clone();
            let id = self.id;
            tokio::spawn(async move {
                let mut state = state.lock().await;
                if state.prepared == Some(id) {
                    state.prepared = None;
                }
            });
        }
    }
}
pub struct Host {
    boot: String,
    next_flow: AtomicU64,
    endpoint: PathBuf,
    path: PathBuf,
    current: Weak<arc_swap::ArcSwap<App>>,
    state: Arc<Mutex<State>>,
    sequence: AtomicU64,
    transition: Arc<Mutex<()>>,
    admissions: tokio::sync::Notify,
    telemetry: std::sync::Mutex<telemetry::Store>,
    connection_ids: std::sync::Mutex<connection_ids::ConnectionIds>,
    retired: std::sync::Mutex<astra_model::runtime::Snapshot>,
    tokens: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    pub(crate) mappings: Arc<dyn astra_dns::MappingStore>,
    tasks: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
}
impl Drop for Host {
    fn drop(&mut self) {
        for task in self.tasks.get_mut().unwrap().drain(..) {
            task.abort();
        }
        astra_platform::platform::remove_socket(&self.endpoint);
    }
}
impl Host {
    pub(crate) async fn prepare_runtime(self: &Arc<Self>, app: &App) -> Result<CandidateGuard> {
        let transition = self.transition.clone().lock_owned().await;
        let (directory, rule_socket) = match self.active_origin().await {
            Some((directory, socket)) => (directory, Some(socket)),
            None => (
                std::env::current_exe()?
                    .parent()
                    .context("missing binary directory")?
                    .to_path_buf(),
                None,
            ),
        };
        let id = self.prepare_inner(app, directory, rule_socket).await?;
        let guard = CandidateGuard {
            host: self.clone(),
            id,
            committed: false,
            transition: Some(transition),
        };
        let generation = self.state.lock().await.generations[&id].clone();
        let _: bool = Self::rpc(&generation, WorkerRequest::Active(true)).await?;
        Ok(guard)
    }
    pub async fn start(path: PathBuf, current: &Current) -> Result<Arc<Self>> {
        let control = std::env::var_os("ASTRA_CONTROL_SOCKET")
            .context("resident mode requires ASTRA_CONTROL_SOCKET")?;
        let endpoint = endpoint(Path::new(&control));
        crate::platform::worker::validate_endpoint_directory(
            astra_platform::platform::endpoint_path(&endpoint)
                .parent()
                .context("resident endpoint requires a private state directory")?,
        )?;
        let listener = astra_platform::platform::bind_listener(&endpoint)?;
        astra_platform::platform::set_private(
            astra_platform::platform::endpoint_path(&endpoint),
            0o600,
        )?;
        let host = Arc::new(Self {
            boot: uuid::Uuid::new_v4().to_string(),
            next_flow: AtomicU64::new(1),
            endpoint,
            path,
            current: Arc::downgrade(current),
            state: Arc::new(Mutex::new(State {
                active: 0,
                prepared: None,
                paused: false,
                generations: BTreeMap::new(),
            })),
            transition: Default::default(),
            admissions: Default::default(),
            telemetry: Default::default(),
            retired: Default::default(),
            connection_ids: Default::default(),
            tokens: Default::default(),
            mappings: current
                .load()
                .dns
                .as_ref()
                .map(|dns| dns.mapping_store())
                .unwrap_or_else(|| Arc::new(astra_dns::LocalMappingStore::default())),
            sequence: AtomicU64::new(1),
            tasks: Default::default(),
        });
        let weak = Arc::downgrade(&host);
        host.tasks.lock().unwrap().push(tokio::spawn(async move {
            let mut clients = tokio::task::JoinSet::new();
            let permits = Arc::new(tokio::sync::Semaphore::new(128));
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        let Ok((mut stream, _)) = result else { break };
                        let Ok(permit) = permits.clone().try_acquire_owned() else { continue };
                        let Some(host) = weak.upgrade() else { break };
                        clients.spawn(async move {
                            let _permit = permit;
                            let result = async {
                                let command = tokio::time::timeout(Duration::from_secs(3), receive(&mut stream)).await??;
                                if let Command::UdpPermit { token } = command {
                                    return host.hold_udp_permit(&mut stream, &token, _permit).await;
                                }
                                let reply = host.command(command).await.map_err(|e| format!("{e:#}"));
                                send(&mut stream, reply).await
                            }.await;
                            if let Err(error) = result { tracing::debug!(%error,"host request ended"); }
                        });
                    }
                    Some(_) = clients.join_next(), if !clients.is_empty() => {}
                }
            }
        }));
        let weak = Arc::downgrade(&host);
        host.tasks.lock().unwrap().push(tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let Some(host) = weak.upgrade() else { break };
                host.refresh_telemetry().await;
                host.reap().await;
            }
        }));
        Ok(host)
    }
    fn app(&self) -> Result<Arc<App>> {
        Ok(self.current.upgrade().context("host stopped")?.load_full())
    }
    pub async fn busy(&self) -> bool {
        self.state.lock().await.prepared.is_some()
    }
    pub async fn prepare(
        &self,
        app: &App,
        directory: PathBuf,
        rule_socket: Option<String>,
    ) -> Result<u64> {
        let _transition = self.transition.lock().await;
        self.prepare_inner(app, directory, rule_socket).await
    }
    async fn prepare_inner(
        &self,
        app: &App,
        directory: PathBuf,
        rule_socket: Option<String>,
    ) -> Result<u64> {
        let id = self.sequence.fetch_add(1, Ordering::Relaxed);
        {
            let mut state = self.state.lock().await;
            ensure!(
                state.prepared.is_none(),
                "another update is already prepared"
            );
            ensure!(
                state.generations.len() < 16,
                "too many live generations; wait for connections to finish"
            );
            state.prepared = Some(id);
        }
        let mut preparation = Preparation {
            state: self.state.clone(),
            id,
            committed: false,
            tokens: self.tokens.clone(),
            token: None,
            pid: None,
        };
        let endpoint = self
            .endpoint
            .with_file_name(format!("w-{}-{id}.sock", std::process::id()));
        let token = uuid::Uuid::new_v4().to_string();
        self.tokens.lock().unwrap().insert(token.clone());
        preparation.token = Some(token.clone());
        let mut table: toml::Table = toml::from_str(&app.effective_text)?;
        if let Some(socket) = &rule_socket {
            table.insert("rule_socket".into(), socket.clone().into());
        }
        let rule_socket = rule_socket.unwrap_or_else(|| app.config.rule_socket.clone());
        table.insert("watch_config".into(), false.into());
        let config = WorkerConfig {
            path: self.path.clone(),
            text: toml::to_string(&table)?,
            overrides: app.overrides.read().unwrap().clone(),
            interface: app.config.tun.as_ref().and_then(|t| t.interface.clone()),
            host: self.endpoint.clone(),
            token: token.clone(),
            endpoint: endpoint.clone(),
            generation: id,
            boot: self.boot.clone(),
        };
        let binary = directory.join(astra_platform::platform::executable("astra-proxy"));
        let mut config_bytes = Vec::new();
        send(&mut config_bytes, config).await?;
        let log = self.endpoint.with_file_name(format!(
            "w-{}-{id}-{}.log",
            std::process::id(),
            self.boot
        ));
        let output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&log)
            .with_context(|| format!("create worker log {}", log.display()))?;
        astra_platform::platform::set_private(&log, 0o600)?;
        let mut child = crate::platform::worker::spawn(&binary, config_bytes, output).await?;
        let pid = child.id().context("worker has no PID")?;
        crate::platform::process::register_worker(pid);
        preparation.pid = Some(pid);
        let result = async {
            tokio::time::timeout(Duration::from_secs(15), async {
                loop {
                    if let Some(status) = child.try_wait()? {
                        bail!("candidate exited: {status}");
                    }
                    if let Ok(mut stream) =
                        astra_platform::platform::connect_trusted(&endpoint).await
                    {
                        send(
                            &mut stream,
                            Auth {
                                token: token.clone(),
                                command: WorkerRequest::Ready,
                            },
                        )
                        .await?;
                        let ready: std::result::Result<astra_ipc::host::Hello, String> =
                            receive(&mut stream).await?;
                        ready.map_err(anyhow::Error::msg)?.validate_worker()?;
                        let mut probe =
                            astra_platform::platform::connect_trusted(&endpoint).await?;
                        send(
                            &mut probe,
                            Auth {
                                token: token.clone(),
                                command: WorkerRequest::Probe,
                            },
                        )
                        .await?;
                        let mut probe = astra_ipc::host::HostStream::new(probe);
                        probe.write_all(b"astra").await?;
                        probe.shutdown().await?;
                        let mut answer = Vec::new();
                        tokio::io::AsyncReadExt::take(&mut probe, 6)
                            .read_to_end(&mut answer)
                            .await?;
                        ensure!(answer == b"astra", "worker data channel probe failed");
                        return Ok::<_, anyhow::Error>(());
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            })
            .await??;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(error) = result {
            self.tokens.lock().unwrap().remove(&token);
            let _ = child.kill().await;
            crate::platform::process::unregister_worker(pid);
            astra_platform::platform::remove_socket(&endpoint);
            return Err(error);
        }
        let mut state = self.state.lock().await;
        ensure!(
            state.prepared == Some(id),
            "candidate preparation was cancelled"
        );
        state.generations.insert(
            id,
            Arc::new(Generation {
                id,
                directory,
                endpoint,
                token,
                log,
                rule_socket,
                snapshot: Default::default(),
                failed: Default::default(),
                route_leases: std::sync::Mutex::new(app.route_leases.clone()),
                child: Mutex::new(child),
                flows: AtomicUsize::new(0),
            }),
        );
        preparation.committed = true;
        Ok(id)
    }
    pub async fn activate(&self, id: u64) -> Result<()> {
        self.select(id, true).await?;
        Ok(())
    }
    async fn select(&self, id: u64, prepared: bool) -> Result<()> {
        let _transition = self.transition.lock().await;
        let (new, old, previous) = {
            let state = self.state.lock().await;
            ensure!(
                if prepared {
                    state.prepared == Some(id)
                } else {
                    state.paused
                },
                "generation is not prepared or admission is not paused"
            );
            let new = state
                .generations
                .get(&id)
                .context("generation is unavailable")?
                .clone();
            ensure!(!new.failed.load(Ordering::Acquire), "generation has failed");
            (
                new,
                state.generations.get(&state.active).cloned(),
                state.active,
            )
        };
        let _: bool = Self::rpc(&new, WorkerRequest::Active(true)).await?;
        {
            let mut state = self.state.lock().await;
            ensure!(
                state.active == previous,
                "active generation changed concurrently"
            );
            if prepared {
                ensure!(state.prepared == Some(id), "candidate cancelled");
                state.prepared = None;
            }
            state.active = id;
        }
        if let Some(old) = old.filter(|old| old.id != id) {
            let _ = Self::rpc::<bool>(&old, WorkerRequest::Active(false)).await;
        }
        Ok(())
    }
    pub async fn active_origin(&self) -> Option<(PathBuf, String)> {
        let state = self.state.lock().await;
        state
            .generations
            .get(&state.active)
            .map(|g| (g.directory.clone(), g.rule_socket.clone()))
    }
    pub fn merge_snapshot(&self, snapshot: &mut astra_model::runtime::Snapshot) {
        let store = self.telemetry.lock().unwrap();
        Self::merge_metrics(snapshot, &store.snapshot);
    }
    fn merge_metrics(
        snapshot: &mut astra_model::runtime::Snapshot,
        metrics: &astra_model::runtime::Snapshot,
    ) {
        snapshot.upload += metrics.upload;
        snapshot.download += metrics.download;
        snapshot.proxy_upload += metrics.proxy_upload;
        snapshot.proxy_download += metrics.proxy_download;
        snapshot.active += metrics.active;
        snapshot.total += metrics.total;
        snapshot.truncated |= metrics.truncated;
        snapshot
            .connections
            .extend(metrics.connections.iter().cloned());
        snapshot.truncated |= snapshot.connections.len() > 128;
        snapshot.connections.truncate(128);
        if !metrics.groups.is_empty() {
            snapshot.groups = metrics.groups.clone();
        }
    }
    async fn rpc<T: serde::de::DeserializeOwned>(
        generation: &Generation,
        command: WorkerRequest,
    ) -> Result<T> {
        tokio::time::timeout(Duration::from_secs(3), async {
            let mut stream =
                astra_platform::platform::connect_trusted(&generation.endpoint).await?;
            send(
                &mut stream,
                Auth {
                    token: generation.token.clone(),
                    command,
                },
            )
            .await?;
            receive(&mut stream).await
        })
        .await?
    }
    pub async fn overrides(
        &self,
        value: astra_model::runtime::Overrides,
        disconnect: Option<String>,
    ) -> Result<()> {
        let _transition = self.transition.lock().await;
        let (active, draining) = {
            let state = self.state.lock().await;
            ensure!(
                !state.paused,
                "admission is paused; retry runtime changes after the update"
            );
            ensure!(
                state.prepared.is_none(),
                "an update is prepared; retry runtime changes after activation"
            );
            let active = state
                .generations
                .get(&state.active)
                .context("no active worker")?
                .clone();
            ensure!(
                !active.failed.load(Ordering::Acquire),
                "active worker exited"
            );
            let draining = state
                .generations
                .values()
                .filter(|g| {
                    g.id != state.active
                        && g.flows.load(Ordering::Acquire) > 0
                        && !g.failed.load(Ordering::Acquire)
                })
                .cloned()
                .collect::<Vec<_>>();
            (active, draining)
        };
        let result: std::result::Result<(), String> = Self::rpc(
            &active,
            WorkerRequest::Overrides {
                value,
                disconnect: disconnect.clone(),
            },
        )
        .await?;
        result.map_err(anyhow::Error::msg)?;
        if let Some(group) = disconnect {
            futures_util::future::join_all(draining.iter().map(|generation|async {
                if let Err(error) = Self::rpc::<bool>(generation,WorkerRequest::DisconnectGroup(group.clone())).await {
                    tracing::warn!(generation=generation.id,%group,%error,"could not disconnect draining worker group");
                }
            })).await;
        }
        Ok(())
    }
    pub async fn test_group(&self, name: String) -> Result<()> {
        let lease = self.lease().await?;
        let result: std::result::Result<(), String> =
            Self::rpc(&lease.0, WorkerRequest::TestGroup(name)).await?;
        result.map_err(anyhow::Error::msg)
    }
    pub async fn disconnect(&self, id: Option<u64>) -> Result<bool> {
        if let Some(id) = id {
            let target = self.connection_ids.lock().unwrap().resolve(id).cloned();
            let Some(target) = target.filter(|target| target.boot == self.boot) else {
                return Ok(false);
            };
            let generation = self
                .state
                .lock()
                .await
                .generations
                .get(&target.generation)
                .cloned();
            let Some(generation) = generation else {
                return Ok(false);
            };
            return Self::rpc(&generation, WorkerRequest::Disconnect(Some(target.worker))).await;
        }
        let generations: Vec<_> = self
            .state
            .lock()
            .await
            .generations
            .values()
            .cloned()
            .collect();
        let mut found = true;
        for generation in generations {
            found |= Self::rpc::<bool>(&generation, WorkerRequest::Disconnect(None)).await?;
        }
        Ok(found)
    }
    async fn refresh_telemetry(&self) {
        use futures_util::{stream, StreamExt};
        let (active, generations) = {
            let state = self.state.lock().await;
            (
                state.active,
                state.generations.values().cloned().collect::<Vec<_>>(),
            )
        };
        let mut combined = self.retired.lock().unwrap().clone();
        // A slow draining worker must not serialize every healthy worker's sample.
        let mut samples = stream::iter(generations.into_iter().map(|generation| async move {
            let result =
                Self::rpc::<astra_model::runtime::Snapshot>(&generation, WorkerRequest::Snapshot)
                    .await;
            (generation, result, std::time::Instant::now())
        }))
        .buffer_unordered(4)
        .collect::<Vec<_>>()
        .await;
        samples.sort_by_key(|(generation, _, _)| generation.id);
        let mut snapshots = Vec::with_capacity(samples.len());
        let mut health = BTreeMap::new();
        for (generation, result, sampled_at) in samples {
            health.insert(generation.id, result.is_ok().then_some(sampled_at));
            if let Ok(snapshot) = result {
                *generation.snapshot.lock().unwrap() = snapshot;
            }
            let mut snapshot = generation.snapshot.lock().unwrap().clone();
            snapshot.truncated |= snapshot.connections.len() > 128;
            snapshot.connections.truncate(128);
            snapshots.push((generation.id, snapshot));
        }
        let mut ids = self.connection_ids.lock().unwrap();
        combined.truncated |= !ids.reconcile(
            &self.boot,
            snapshots.iter().flat_map(|(generation, snapshot)| {
                snapshot
                    .connections
                    .iter()
                    .map(|connection| (*generation, connection.id))
            }),
        );
        let mut origins = BTreeMap::new();
        for (generation, mut snapshot) in snapshots {
            combined.upload += snapshot.upload;
            combined.download += snapshot.download;
            combined.proxy_upload += snapshot.proxy_upload;
            combined.proxy_download += snapshot.proxy_download;
            combined.total += snapshot.total;
            combined.active += snapshot.active;
            combined.truncated |= snapshot.truncated;
            for connection in &mut snapshot.connections {
                let Some(id) = ids.id(&self.boot, generation, connection.id) else {
                    combined.truncated = true;
                    continue;
                };
                connection.id = id;
                if combined.connections.len() < 128 {
                    origins.insert(id, generation);
                    combined.connections.push(connection.clone());
                } else {
                    combined.truncated = true;
                }
            }
            if generation == active {
                combined.groups = snapshot.groups;
            }
        }
        self.telemetry
            .lock()
            .unwrap()
            .publish(combined, health, origins);
    }
    async fn lease(&self) -> Result<Lease> {
        loop {
            let ready = self.admissions.notified();
            tokio::pin!(ready);
            ready.as_mut().enable();
            let state = self.state.lock().await;
            if !state.paused {
                let generation = state
                    .generations
                    .get(&state.active)
                    .context("no active worker")?
                    .clone();
                ensure!(!generation.failed.load(Ordering::Acquire),"active worker exited; existing flows cannot be migrated; prepare a replacement or roll back");
                generation.flows.fetch_add(1, Ordering::AcqRel);
                return Ok(Lease(generation));
            }
            drop(state);
            ready.await;
        }
    }
    fn metadata(&self, mut metadata: Metadata, generation: u64) -> Metadata {
        metadata.flow_id = Some(astra_ipc::host::FlowId {
            boot: self.boot.clone(),
            generation,
            sequence: self.next_flow.fetch_add(1, Ordering::Relaxed),
        });
        metadata
    }
    pub async fn relay<S: AsyncRead + AsyncWrite + Unpin>(
        &self,
        mut client: S,
        metadata: Metadata,
    ) -> Result<()> {
        let lease = self.lease().await?;
        let metadata = self.metadata(metadata, lease.0.id);
        let mut remote = astra_platform::platform::connect_trusted(&lease.0.endpoint).await?;
        send(
            &mut remote,
            Auth {
                token: lease.0.token.clone(),
                command: WorkerRequest::Tcp(metadata),
            },
        )
        .await?;
        let ready: std::result::Result<(), String> = receive(&mut remote).await?;
        ready.map_err(anyhow::Error::msg)?;
        let mut remote = astra_ipc::host::HostStream::new(remote);
        tokio::io::copy_bidirectional(&mut client, &mut remote).await?;
        Ok(())
    }
    async fn command(&self, command: Command) -> Result<serde_json::Value> {
        match command {
            Command::Telemetry => {
                let state = self.state.lock().await;
                let app = self.app()?;
                let mut snapshot = app.metrics.snapshot();
                snapshot.mode = app.overrides.read().unwrap().mode;
                snapshot.system_proxy = app.config.system_proxy.is_some();
                snapshot.tun = app.config.tun.is_some();
                snapshot.allow_lan = app.lan_enabled();
                snapshot.groups = app.groups.snapshot(&app);
                let store = self.telemetry.lock().unwrap();
                Self::merge_metrics(&mut snapshot, &store.snapshot);
                Ok(serde_json::to_value(astra_model::runtime::RuntimeView {
                    snapshot,
                    telemetry: Some(store.metadata(&self.boot, state.active, state.paused)),
                })?)
            }
            Command::Mapping { token, operation } => self.mapping(&token, *operation).await,
            Command::Status => {
                let state = self.state.lock().await;
                let mut generations = vec![];
                for g in state.generations.values() {
                    generations.push(GenerationStatus {
                        generation: g.id,
                        directory: g.directory.clone(),
                        pid: g.child.lock().await.id(),
                        active: g.id == state.active,
                        prepared: state.prepared == Some(g.id),
                        flows: g.flows.load(Ordering::Acquire),
                        rule_socket: g.rule_socket.clone(),
                        failed: g.failed.load(Ordering::Acquire),
                        log: g.log.clone(),
                    });
                }
                Ok(serde_json::to_value(Status {
                    active: state.active,
                    generations,
                    boot_id: self.boot.clone(),
                    version: env!("CARGO_PKG_VERSION").into(),
                    binary: std::env::current_exe().ok(),
                    paused: state.paused,
                    busy: state.prepared.is_some() || state.paused,
                    capabilities: vec!["runtime_telemetry_v1".into()],
                })?)
            }
            Command::Prepare {
                directory,
                rule_socket,
            } => Ok(self
                .prepare(self.app()?.as_ref(), directory, rule_socket)
                .await?
                .into()),
            Command::Activate { generation } => {
                self.activate(generation).await?;
                Ok(true.into())
            }
            Command::Pause => {
                let _transition = self.transition.lock().await;
                self.state.lock().await.paused = true;
                Ok(true.into())
            }
            Command::Resume => {
                let _transition = self.transition.lock().await;
                self.state.lock().await.paused = false;
                self.admissions.notify_waiters();
                Ok(true.into())
            }
            Command::SelectExisting { generation } => {
                self.select(generation, false).await?;
                Ok(true.into())
            }
            Command::Abort { generation } => {
                let _transition = self.transition.lock().await;
                let mut state = self.state.lock().await;
                ensure!(
                    state.prepared == Some(generation),
                    "can only abort a prepared generation"
                );
                state.prepared = None;
                Ok(true.into())
            }
            Command::Reverse { ip, token } => {
                self.authenticate(&token).await?;
                Ok(serde_json::to_value(
                    self.app()?
                        .dns
                        .as_ref()
                        .map(|dns| dns.restore(ip))
                        .transpose()?
                        .flatten(),
                )?)
            }
            Command::Lookup { host, port, token } => {
                self.authenticate(&token).await?;
                Ok(serde_json::to_value(
                    self.app()?.lookup_real(&host, port).await?,
                )?)
            }
            Command::UdpPermit { .. } => bail!("UDP permit requires a dedicated channel"),
        }
    }
    async fn authenticate(&self, token: &str) -> Result<()> {
        ensure!(
            self.tokens.lock().unwrap().contains(token),
            "unknown worker"
        );
        Ok(())
    }
    async fn reap(&self) {
        let transition = self.transition.lock().await;
        let mut state = self.state.lock().await;
        for generation in state.generations.values() {
            let mut child = generation.child.lock().await;
            let pid = child.id();
            if child.try_wait().ok().flatten().is_some() {
                generation.failed.store(true, Ordering::Release);
                if let Some(pid) = pid {
                    crate::platform::process::unregister_worker(pid);
                }
            }
        }
        if state.paused {
            return;
        }
        let retired: Vec<_> = state
            .generations
            .iter()
            .filter(|(id, g)| {
                **id != state.active
                    && Some(**id) != state.prepared
                    && g.flows.load(Ordering::Acquire) == 0
            })
            .map(|(id, _)| *id)
            .collect();
        let retired: Vec<_> = retired
            .into_iter()
            .filter_map(|id| state.generations.remove(&id))
            .collect();
        drop(state);
        drop(transition);
        for g in retired {
            self.tokens.lock().unwrap().remove(&g.token);
            {
                let last = g.snapshot.lock().unwrap();
                let mut totals = self.retired.lock().unwrap();
                totals.upload += last.upload;
                totals.download += last.download;
                totals.proxy_upload += last.proxy_upload;
                totals.proxy_download += last.proxy_download;
                totals.total += last.total;
            }
            let mut child = g.child.lock().await;
            if let Some(pid) = child.id() {
                let _ = Self::rpc::<bool>(&g, WorkerRequest::Retire).await;
                let graceful = tokio::time::timeout(Duration::from_secs(3), async {
                    while child.try_wait()?.is_none() {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Ok::<_, std::io::Error>(())
                })
                .await;
                if !matches!(graceful, Ok(Ok(()))) {
                    let _ = child.kill().await;
                }
                crate::platform::process::unregister_worker(pid);
            }
            astra_platform::platform::remove_socket(&g.endpoint);
        }
    }
}

#[derive(Clone)]
pub(crate) struct WorkerHost {
    endpoint: PathBuf,
    token: String,
}

pub async fn worker() -> Result<()> {
    let config: WorkerConfig = receive(&mut tokio::io::stdin()).await?;
    let mut app = App::from_text(&config.path, config.text)?;
    *app.overrides.write().unwrap() = config.overrides;
    if let Some(tun) = &mut app.config.tun {
        tun.interface = config.interface.clone();
    }
    let _interface = config
        .interface
        .as_deref()
        .map(meow_common::outbound_iface::install_outbound_interface)
        .transpose()?;
    app.worker_host = Some(WorkerHost {
        endpoint: config.host.clone(),
        token: config.token.clone(),
    });
    if let Some(dns) = &mut app.dns {
        dns.set_mapping_store(Arc::new(app.worker_host.as_ref().unwrap().clone()))?;
    }
    if let Some(rule) = &mut app.rule_client {
        rule.start_observer();
    }
    let app = Arc::new(app);
    let listener = astra_platform::platform::bind_listener(&config.endpoint)?;
    astra_platform::platform::set_private(
        astra_platform::platform::endpoint_path(&config.endpoint),
        0o600,
    )?;
    let mut tasks = tokio::task::JoinSet::new();
    let enabled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop = Arc::new(tokio::sync::Notify::new());
    let refresh_app = app.clone();
    let refresh_enabled = enabled.clone();
    tasks.spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if refresh_enabled.load(Ordering::Acquire) {
                refresh_app.groups.refresh_all(&refresh_app).await;
            }
        }
    });
    loop {
        tokio::select! {
            _ = stop.notified() => break,
            accepted = listener.accept() => {
                let (mut stream,_) = accepted?;
                let app = app.clone(); let token = config.token.clone();
                let boot = config.boot.clone(); let generation = config.generation;
                let enabled = enabled.clone(); let stop = stop.clone();
                tasks.spawn(async move {
                    let result = async {
                        let auth: Auth<WorkerRequest> = tokio::time::timeout(Duration::from_secs(3),receive(&mut stream)).await??;
                        ensure!(auth.token == token, "worker authentication failed");
                        if let WorkerRequest::Tcp(meta) | WorkerRequest::Udp(meta) = &auth.command {
                            ensure!(meta.flow_id.as_ref().is_some_and(|id|id.boot == boot && id.generation == generation && id.sequence != 0),"wrong flow generation");
                        }
                        match auth.command {
                            WorkerRequest::Ready => {
                                let result = async {
                                    if app.rule_client.is_some() {
                                        let mut rule = astra_platform::platform::connect_trusted(&app.config.rule_socket).await?;
                                        astra_ipc::exchange_core_info(&mut rule,env!("CARGO_PKG_VERSION")).await?;
                                    }
                                    Ok::<_,anyhow::Error>(astra_ipc::host::Hello::worker(env!("CARGO_PKG_VERSION")))
                                }.await.map_err(|e|format!("{e:#}"));
                                send(&mut stream,result).await
                            }
                            WorkerRequest::Active(value) => { enabled.store(value,Ordering::Release); send(&mut stream,true).await }
                            WorkerRequest::Retire => { send(&mut stream,true).await?; stop.notify_one(); Ok(()) }
                            WorkerRequest::Probe => {
                                use tokio::io::AsyncReadExt;
                                let mut stream = astra_ipc::host::HostStream::new(stream);
                                let mut data = Vec::new(); (&mut stream).take(6).read_to_end(&mut data).await?;
                                ensure!(data == b"astra", "invalid data channel probe");
                                stream.write_all(&data).await?; stream.shutdown().await?; Ok(())
                            }
                            WorkerRequest::Dns(raw) => {
                                let response = app.dns_answer(&raw).await.map_err(|e|format!("{e:#}"));
                                send(&mut stream,response).await
                            }
                            WorkerRequest::DnsTcp => {
                                send(&mut stream,Ok::<(),String>(())).await?;
                                crate::dns::tcp(astra_ipc::host::HostStream::new(stream),app).await
                            }
                            WorkerRequest::Snapshot => {
                                let mut snapshot = app.metrics.snapshot(); snapshot.groups = app.groups.snapshot(&app);
                                send(&mut stream,snapshot).await
                            }
                            WorkerRequest::Disconnect(id) => send(&mut stream, app.metrics.disconnect(id)).await,
                            WorkerRequest::Overrides {value,disconnect} => {
                                app.groups.commit_overrides(&app,value,disconnect.as_deref());
                                send(&mut stream,Ok::<(),String>(())).await
                            }
                            WorkerRequest::DisconnectGroup(group) => {
                                app.groups.sessions.disconnect(&group,"manual");
                                send(&mut stream,true).await
                            }
                            WorkerRequest::TestGroup(name) => {
                                let result = crate::proxy::groups::Groups::request_test(app,name).map_err(|e|format!("{e:#}"));
                                send(&mut stream,result).await
                            }
                            WorkerRequest::Tcp(meta) => {
                                send(&mut stream, Ok::<(),String>(())).await?;
                                let stream = astra_ipc::host::HostStream::new(stream);
                                INGRESS_PROCESS.scope(meta.process.clone(),async move { match meta.inbound {
                                    Inbound::Socks5 | Inbound::HttpConnect | Inbound::Mixed => crate::inbound::handle_stream(stream,app,meta.inbound,meta.source,meta.local).await,
                                    _ => transparent_stream(stream,app,meta).await,
                                }}).await
                            }
                            WorkerRequest::Udp(meta) => udp::serve(stream, app, meta).await,
                        }
                    }.await;
                    if let Err(error) = result { tracing::debug!(%error,"host worker request ended"); }
                });
            }
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
        }
    }
    Ok(())
}
async fn transparent_stream<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    app: Arc<App>,
    meta: Metadata,
) -> Result<()> {
    let destination = meta
        .destination
        .context("transparent ingress requires original destination")?;
    let original = Target {
        host: destination.ip().to_string(),
        port: destination.port(),
    };
    let sniff = meta.inbound == Inbound::Tun
        && app.config.tun.as_ref().is_none_or(|tun| tun.sniff)
        && matches!(destination.port(), 80 | 443 | 8080 | 8443);
    let (prefix, hostname) = if sniff {
        crate::proxy::target::preface(&mut stream).await?
    } else {
        (vec![], None)
    };
    let (target, dial) = crate::proxy::target::select(&app, &original, hostname)?;
    let action = app
        .route_with_context(
            &target,
            meta.inbound,
            astra_model::Network::Tcp,
            meta.process,
            Some(destination.ip()).filter(|ip| !app.synthetic_ip(*ip)),
        )
        .await?;
    let mut remote = app
        .connect_tracked(&dial, &action, meta.inbound, Some(meta.source))
        .await?;
    remote.write_all(&prefix).await?;
    tokio::io::copy_bidirectional(&mut stream, &mut remote).await?;
    Ok(())
}
