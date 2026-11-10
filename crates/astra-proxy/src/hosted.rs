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
mod traffic_history;
#[cfg(test)]
#[path = "test/hosted_transition_test.rs"]
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
    Monitor,
}

/// Additive JSON response: old hosts ignore node totals, new hosts accept old workers.
#[derive(Serialize, Deserialize)]
struct WorkerSample {
    #[serde(flatten)]
    snapshot: astra_model::runtime::Snapshot,
    #[serde(default)]
    node_totals: Option<BTreeMap<String, u64>>,
    #[serde(default)]
    dns: Option<astra_model::runtime::DnsQueryLog>,
    #[serde(default)]
    dns_resolver: Option<astra_model::runtime::DnsResolverStatus>,
}

fn worker_sample(app: &App) -> WorkerSample {
    let mut snapshot = app.metrics.snapshot();
    snapshot.groups = app.groups.snapshot(app);
    let mut nodes = app.metrics.node_totals();
    for outbound in &app.config.outbounds {
        nodes.entry(outbound.tag().to_owned()).or_default();
    }
    WorkerSample {
        snapshot,
        node_totals: Some(nodes),
        dns_resolver: Some(astra_model::runtime::DnsResolverStatus::from_config(
            app.dns.as_ref().map(|dns| &dns.config),
        )),
        dns: Some(
            app.dns
                .as_ref()
                .map(|dns| dns.query_log())
                .unwrap_or_default(),
        ),
    }
}

fn selected_traffic_node(app: &App, snapshot: &astra_model::runtime::Snapshot) -> Option<String> {
    use astra_model::runtime::Mode;
    if snapshot.mode == Mode::Direct && !app.config.standalone {
        return None;
    }
    let single =
        || (app.config.outbounds.len() == 1).then(|| app.config.outbounds[0].tag().to_owned());
    let mut name = if app.config.standalone {
        app.config.standalone_outbound.clone().or_else(single)?
    } else if snapshot.groups.iter().any(|group| group.name == "PROXY") {
        "PROXY".to_owned()
    } else {
        single()?
    };
    for _ in 0..8 {
        match snapshot.groups.iter().find(|group| group.name == name) {
            Some(group) => name = group.selected.clone()?,
            None => {
                return app
                    .config
                    .outbounds
                    .iter()
                    .any(|outbound| outbound.tag() == name)
                    .then_some(name)
            }
        }
    }
    None
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
    /// Acquire admission before the caller changes live resources. Cancellation
    /// here still drops only the prepared generation.
    pub async fn lock_commit(
        self,
        route_leases: Vec<Arc<crate::tun::OwnedRoute>>,
    ) -> Result<RuntimeCommit> {
        let state = self.host.state.clone().lock_owned().await;
        self.host.ensure_consistent()?;
        self.host.check_generations(&state)?;
        *self.host.checked_lock(
            "candidate route ownership",
            &state.generations[&self.id].route_leases,
        )? = route_leases;
        Ok(RuntimeCommit {
            candidate: self,
            state,
        })
    }
}
pub(crate) struct RuntimeCommit {
    candidate: CandidateGuard,
    state: tokio::sync::OwnedMutexGuard<State>,
}
impl RuntimeCommit {
    /// All waits precede the live-state commit. Publishing the App and choosing
    /// its worker cannot be interrupted by a timeout or shutdown cancellation.
    pub fn commit(mut self, publish: impl FnOnce()) {
        let old = {
            let state = &mut self.state;
            let id = self.candidate.id;
            debug_assert_eq!(state.prepared, Some(id));
            debug_assert!(state.generations.contains_key(&id));
            let old = state.generations.get(&state.active).cloned();
            publish();
            state.prepared = None;
            state.active = id;
            old.filter(|old| old.id != id)
        };
        self.candidate.committed = true;
        // Keep ordering with subsequent rollback/activation, without delaying
        // the runtime commit on an old worker's response.
        let transition = self.candidate.transition.take();
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
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let _ = host.command(Command::Abort { generation }).await;
                });
            }
        }
    }
}
impl Drop for Preparation {
    fn drop(&mut self) {
        if !self.committed {
            if let Some(token) = &self.token {
                self.tokens
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(token);
            }
            if let Some(pid) = self.pid {
                crate::platform::process::unregister_worker(pid);
            }
            let state = self.state.clone();
            let id = self.id;
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let mut state = state.lock().await;
                    if state.prepared == Some(id) {
                        state.prepared = None;
                    }
                });
            }
        }
    }
}
struct OverrideAttempt<'a> {
    host: &'a Host,
    pending: bool,
}
impl Drop for OverrideAttempt<'_> {
    fn drop(&mut self) {
        if self.pending {
            let _ = self.host.consistency.set(
                "Runtime override acknowledgement lost; outcome unknown; controlled shutdown required".into(),
            );
        }
    }
}
pub struct Host {
    consistency: std::sync::OnceLock<String>,
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
    traffic_history: Arc<std::sync::Mutex<traffic_history::History>>,
    connection_ids: std::sync::Mutex<connection_ids::ConnectionIds>,
    retired: std::sync::Mutex<astra_model::runtime::Snapshot>,
    tokens: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    pub(crate) mappings: Arc<dyn astra_dns::MappingStore>,
    tasks: std::sync::Mutex<Vec<astra_platform::supervision::Task>>,
}
impl Drop for Host {
    fn drop(&mut self) {
        for task in self
            .tasks
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
        {
            task.abort();
        }
        self.traffic_history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .save();
        astra_platform::platform::remove_socket(&self.endpoint);
    }
}
impl Host {
    fn poisoned(&self, name: &str) -> anyhow::Error {
        let message = format!("network host {name} lock poisoned; controlled shutdown required");
        let _ = self.consistency.set(message.clone());
        anyhow::anyhow!(message)
    }
    fn checked_lock<'a, T>(
        &self,
        name: &str,
        lock: &'a std::sync::Mutex<T>,
    ) -> Result<std::sync::MutexGuard<'a, T>> {
        lock.lock().map_err(|_| self.poisoned(name))
    }
    fn check_generations(&self, state: &State) -> Result<()> {
        for generation in state.generations.values() {
            if generation.route_leases.is_poisoned() {
                return Err(self.poisoned("generation route ownership"));
            }
            if generation.snapshot.is_poisoned() {
                return Err(self.poisoned("generation telemetry"));
            }
        }
        Ok(())
    }
    pub(crate) fn ensure_consistent(&self) -> Result<()> {
        for (name, poisoned) in [
            ("tokens", self.tokens.is_poisoned()),
            ("tasks", self.tasks.is_poisoned()),
            ("telemetry", self.telemetry.is_poisoned()),
            ("connection identities", self.connection_ids.is_poisoned()),
            ("retired telemetry", self.retired.is_poisoned()),
        ] {
            if poisoned {
                return Err(self.poisoned(name));
            }
        }
        if let Some(error) = self.consistency.get() {
            bail!("{error}");
        }
        Ok(())
    }
    pub(crate) async fn task_failure(&self) -> Option<String> {
        if let Err(error) = self.ensure_consistent() {
            return Some(error.to_string());
        }
        let finished = {
            let mut tasks = match self.checked_lock("tasks", &self.tasks) {
                Ok(tasks) => tasks,
                Err(error) => return Some(error.to_string()),
            };
            tasks
                .iter()
                .position(|task| task.is_finished())
                .map(|index| tasks.remove(index))
        };
        if let Some(mut task) = finished {
            return Some(task.failure().await);
        }
        let active = {
            let state = self.state.lock().await;
            if let Err(error) = self.check_generations(&state) {
                return Some(error.to_string());
            }
            state.generations.get(&state.active).cloned()
        };
        if let Some(active) = active {
            match active.child.lock().await.try_wait() {
                Ok(Some(status)) => {
                    return Some(format!(
                        "active proxy worker {} exited unexpectedly: {status}",
                        active.id
                    ))
                }
                Err(error) => return Some(format!("cannot observe active proxy worker: {error}")),
                Ok(None) => {}
            }
        }
        None
    }
    pub(crate) async fn prepare_runtime(self: &Arc<Self>, app: &App) -> Result<CandidateGuard> {
        let transition = self.transition.clone().lock_owned().await;
        self.ensure_consistent()?;
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
            consistency: Default::default(),
            boot: uuid::Uuid::new_v4().to_string(),
            next_flow: AtomicU64::new(1),
            endpoint,
            traffic_history: Arc::new(std::sync::Mutex::new(traffic_history::History::open(
                astra_platform::storage::runtime_path(&path, "traffic-history"),
            ))),
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
        host.checked_lock("tasks", &host.tasks)?.push(astra_platform::supervision::Task::spawn_result("network host control listener", async move {
            let mut clients = tokio::task::JoinSet::new();
            let permits = Arc::new(tokio::sync::Semaphore::new(128));
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        let (mut stream, _) = match result {
                            Ok(value)=>value,
                            Err(error) if astra_platform::supervision::transient_io(&error)=>{tokio::time::sleep(Duration::from_millis(100)).await;continue;},
                            Err(error)=>return Err(error),
                        };
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
            Ok::<(),std::io::Error>(())
        }));
        let weak = Arc::downgrade(&host);
        host.checked_lock("tasks", &host.tasks)?
            .push(astra_platform::supervision::Task::spawn(
                "network host telemetry/reaper",
                async move {
                    loop {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        let Some(host) = weak.upgrade() else { break };
                        host.refresh_telemetry().await;
                        host.reap().await;
                    }
                },
            ));
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
        self.ensure_consistent()?;
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
            self.check_generations(&state)?;
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
        self.checked_lock("tokens", &self.tokens)?
            .insert(token.clone());
        preparation.token = Some(token.clone());
        let mut table: toml::Table = toml::from_str(
            app.effective_text
                .as_deref()
                .context("hosted configuration snapshot missing")?,
        )?;
        if let Some(socket) = &rule_socket {
            table.insert("rule_socket".into(), socket.clone().into());
        }
        let rule_socket = rule_socket.unwrap_or_else(|| app.config.rule_socket.clone());
        table.insert("watch_config".into(), false.into());
        let config = WorkerConfig {
            path: self.path.clone(),
            text: toml::to_string(&table)?,
            overrides: (*app.overrides.load_full()).clone(),
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
            self.tokens
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&token);
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
        self.ensure_consistent()?;
        let (new, old, previous) = {
            let state = self.state.lock().await;
            self.check_generations(&state)?;
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
        let Ok(store) = self.checked_lock("telemetry", &self.telemetry) else {
            snapshot.truncated = true;
            return;
        };
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
        self: &Arc<Self>,
        value: astra_model::runtime::Overrides,
        disconnect: Option<String>,
        publish: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        let transition = self.transition.clone().lock_owned().await;
        self.ensure_consistent()?;
        let (active, draining) = {
            let state = self.state.lock().await;
            self.check_generations(&state)?;
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
        let mut attempt = OverrideAttempt {
            host: self,
            pending: true,
        };
        let result: Result<std::result::Result<(), String>> = Self::rpc(
            &active,
            WorkerRequest::Overrides {
                value,
                disconnect: disconnect.clone(),
            },
        )
        .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                attempt.pending = false;
                bail!("{error}");
            }
            Err(error) => {
                return Err(error.context("Runtime override acknowledgement lost; outcome unknown"))
            }
        }
        if let Err(error) = publish() {
            let message = format!("Runtime override outcome unknown: host checkpoint failed after worker commit: {error:#}");
            let _ = self.consistency.set(message.clone());
            bail!("{message}");
        }
        attempt.pending = false;
        tokio::spawn(async move {
            let _transition = transition;
            if let Some(group) = disconnect {
                futures_util::future::join_all(draining.iter().map(|generation|async {
                if let Err(error) = Self::rpc::<bool>(generation,WorkerRequest::DisconnectGroup(group.clone())).await {
                    tracing::warn!(generation=generation.id,%group,%error,"could not disconnect draining worker group");
                }
                })).await;
            }
        });
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
            let target = self
                .checked_lock("connection identities", &self.connection_ids)?
                .resolve(id)
                .cloned();
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
        let mut combined = match self.checked_lock("retired telemetry", &self.retired) {
            Ok(snapshot) => snapshot.clone(),
            Err(_) => return,
        };
        // A slow draining worker must not serialize every healthy worker's sample.
        let mut samples = stream::iter(generations.into_iter().map(|generation| async move {
            let result = Self::rpc::<WorkerSample>(&generation, WorkerRequest::Snapshot).await;
            (generation, result, std::time::Instant::now())
        }))
        .buffer_unordered(4)
        .collect::<Vec<_>>()
        .await;
        samples.sort_by_key(|(generation, _, _)| generation.id);
        let mut snapshots = Vec::with_capacity(samples.len());
        let mut health = BTreeMap::new();
        let now = traffic_history::minute();
        let mut node_coverage = true;
        let mut dns_logs = Vec::new();
        let mut dns_supported = None;
        let mut dns_resolver = None;
        for (generation, result, sampled_at) in samples {
            health.insert(generation.id, result.is_ok().then_some(sampled_at));
            let mut stored = match self.checked_lock("generation telemetry", &generation.snapshot) {
                Ok(stored) => stored,
                Err(_) => return,
            };
            if let Ok(sample) = result {
                if generation.id == active {
                    dns_supported = Some(sample.dns.is_some());
                    dns_resolver = Some(sample.dns_resolver);
                }
                if let Some(log) = sample.dns {
                    dns_logs.push(log);
                }
                if let Some(nodes) = sample.node_totals {
                    self.traffic_history
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .observe(generation.id, &nodes, now);
                } else {
                    node_coverage = false;
                }
                *stored = sample.snapshot;
            } else {
                node_coverage = false;
            }
            let mut snapshot = stored.clone();
            snapshot.truncated |= snapshot.connections.len() > 128;
            snapshot.connections.truncate(128);
            snapshots.push((generation.id, snapshot));
        }
        // Checkpoint on a blocking worker, outside the network/telemetry locks.
        let history = self.traffic_history.clone();
        if history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .checkpoint_due()
        {
            let _ = tokio::task::spawn_blocking(move || {
                history
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .save();
            })
            .await;
        }
        let Ok(mut ids) = self.checked_lock("connection identities", &self.connection_ids) else {
            return;
        };
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
        if let Ok(mut telemetry) = self.checked_lock("telemetry", &self.telemetry) {
            telemetry.publish(combined, health, origins);
            telemetry.merge_dns(dns_logs, dns_supported);
            if let Some(resolver) = dns_resolver {
                telemetry.dns_resolver = resolver.map(Box::new);
            }
            let mut history = self
                .traffic_history
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Ok(app) = self.app() {
                history.observe(0, &app.metrics.node_totals(), now);
            }
            let summary = history.summary(now);
            telemetry.node_traffic = node_coverage.then_some(summary);
            telemetry.traffic_history_error = history.error.clone();
        }
    }
    /// Diagnostics never hold admission/state locks while contacting workers.
    /// Old workers may not support this additive request; omit their data and
    /// report an unavailable count rather than reuse stale counters.
    async fn monitor(&self) -> astra_model::monitor::InternalSnapshot {
        use astra_model::monitor::{Metric, MetricKind};
        use futures_util::{stream, StreamExt};
        let (active, paused, generations) = {
            let state = self.state.lock().await;
            (
                state.active,
                state.paused,
                state
                    .generations
                    .values()
                    .take(16)
                    .cloned()
                    .collect::<Vec<_>>(),
            )
        };
        let mut snapshot = astra_observe::snapshot(self.boot.clone());
        let mut gauge = |name: &str, description: &str, value: u64| {
            snapshot.metrics.push(Metric {
                sampled_at_ms: None,
                name: name.into(),
                description: description.into(),
                value: value.to_string(),
                unit: "count".into(),
                kind: MetricKind::Gauge,
            });
        };
        gauge(
            "runtime.host.generations",
            "宿主：存活代数（含准备及排空）",
            generations.len() as u64,
        );
        gauge(
            "runtime.host.active_generation",
            "宿主：当前活动代号",
            active,
        );
        gauge(
            "runtime.host.paused",
            "宿主：已暂停接纳（1 为暂停）",
            u64::from(paused),
        );
        gauge(
            "runtime.host.failed",
            "宿主：已标记失败代数",
            generations
                .iter()
                .filter(|g| g.failed.load(Ordering::Acquire))
                .count() as u64,
        );
        gauge(
            "runtime.host.flows",
            "宿主：当前代流租约",
            generations.iter().fold(0u64, |total, g| {
                total.saturating_add(g.flows.load(Ordering::Acquire) as u64)
            }),
        );
        let mut unavailable = generations.len() as u64;
        let mut samples = stream::iter(generations.into_iter().map(|generation| async move {
            let result = Self::rpc::<astra_model::monitor::InternalSnapshot>(
                &generation,
                WorkerRequest::Monitor,
            )
            .await;
            (generation.id, result)
        }))
        .buffer_unordered(4);
        // A diagnostic request must finish even when every worker is wedged.
        // Dropping pending RPC futures closes their dedicated streams.
        let deadline = tokio::time::sleep(Duration::from_secs(2));
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                biased;
                _ = &mut deadline => break,
                sample = samples.next() => {
                    let Some((generation, result)) = sample else { break };
                    if let Ok(worker) = result {
                        if append_worker_monitor(&mut snapshot, generation, worker) {
                            unavailable -= 1;
                        }
                    }
                }
            }
        }
        snapshot.metrics.push(Metric {
            sampled_at_ms: None,
            name: "runtime.host.unavailable_workers".into(),
            description: "宿主：本次内部统计不可用的 worker 数".into(),
            value: unavailable.to_string(),
            unit: "count".into(),
            kind: MetricKind::Gauge,
        });
        snapshot.metrics.sort_by(|a, b| a.name.cmp(&b.name));
        snapshot
    }
    async fn lease(&self) -> Result<Lease> {
        loop {
            self.ensure_consistent()?;
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
        crate::proxy::relay::relay(&mut client, &mut remote).await?;
        Ok(())
    }
    async fn command(&self, command: Command) -> Result<serde_json::Value> {
        if !matches!(
            &command,
            Command::Status | Command::Telemetry | Command::Monitor | Command::Abort { .. }
        ) {
            self.ensure_consistent()?;
        }
        match command {
            Command::Monitor => Ok(serde_json::to_value(self.monitor().await)?),
            Command::Telemetry => {
                let state = self.state.lock().await;
                let app = self.app()?;
                let mut snapshot = app.metrics.snapshot();
                snapshot.mode = app.overrides.load().mode;
                snapshot.system_proxy = app.config.system_proxy.is_some();
                snapshot.tun = app.config.tun.is_some();
                snapshot.allow_lan = app.lan_enabled();
                snapshot.groups = app.groups.snapshot(&app);
                let store = self.checked_lock("telemetry", &self.telemetry)?;
                Self::merge_metrics(&mut snapshot, &store.snapshot);
                let mut metadata = store.metadata(&self.boot, state.active, state.paused);
                if let Some(traffic) = &mut metadata.traffic {
                    traffic.selected_node = selected_traffic_node(&app, &snapshot);
                }
                Ok(serde_json::to_value(astra_model::runtime::RuntimeView {
                    snapshot,
                    telemetry: Some(metadata),
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
                    capabilities: vec!["runtime_telemetry_v1".into(), "internal_monitor_v1".into()],
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
            self.checked_lock("tokens", &self.tokens)?.contains(token),
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
            // Capture the final bytes of closed flows before retiring a worker.
            if let Ok(sample) = Self::rpc::<WorkerSample>(&g, WorkerRequest::Snapshot).await {
                if let Some(nodes) = sample.node_totals {
                    self.traffic_history
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .observe(g.id, &nodes, traffic_history::minute());
                }
                if let Ok(mut last) = self.checked_lock("generation telemetry", &g.snapshot) {
                    *last = sample.snapshot;
                }
            }
            self.traffic_history
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .forget_generation(g.id);
            self.tokens
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&g.token);
            {
                if let (Ok(last), Ok(mut totals)) = (
                    self.checked_lock("generation telemetry", &g.snapshot),
                    self.checked_lock("retired telemetry", &self.retired),
                ) {
                    totals.upload += last.upload;
                    totals.download += last.download;
                    totals.proxy_upload += last.proxy_upload;
                    totals.proxy_download += last.proxy_download;
                    totals.total += last.total;
                }
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

fn append_worker_monitor(
    snapshot: &mut astra_model::monitor::InternalSnapshot,
    generation: u64,
    worker: astra_model::monitor::InternalSnapshot,
) -> bool {
    // A generation is never reused in a host boot. Preserve that identity in
    // each metric name so updates cannot splice unrelated counter baselines.
    if worker.epoch != format!("{}:{generation}", snapshot.epoch) {
        return false;
    }
    snapshot
        .metrics
        .extend(worker.metrics.into_iter().map(|mut metric| {
            metric.name = format!("workers.{generation}.{}", metric.name);
            metric.description = format!("worker {generation} · {}", metric.description);
            metric
        }));
    true
}

pub async fn worker() -> Result<()> {
    crate::platform::file_limit::prepare();
    let config: WorkerConfig = receive(&mut tokio::io::stdin()).await?;
    let mut app = App::from_text(&config.path, config.text)?;
    crate::platform::network_recovery::set_enabled(app.config.experimental.network_recovery);
    app.overrides.store(Arc::new(config.overrides));
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
    {
        let routing = &mut *app;
        if let Some(dns) = &mut routing.dns {
            dns.set_mapping_store(Arc::new(routing.worker_host.as_ref().unwrap().clone()))?;
        }
    }
    if let Some(rule) = &mut app.rule_client {
        rule.start_observer();
    }
    let app = Arc::new(app);
    let hook_app = app.clone();
    meow_common::set_host_resolver(Arc::new(crate::dns::HostHook(Box::new(move || {
        hook_app.clone()
    }))));
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
    let mut refresh =
        astra_platform::supervision::Task::spawn("worker group refresh", async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if refresh_enabled.load(Ordering::Acquire) {
                    refresh_app.groups.refresh_all(&refresh_app).await;
                }
            }
        });
    let outcome = loop {
        tokio::select! {
            _ = stop.notified() => break Ok(()),
            error = refresh.failure() => break Err(anyhow::anyhow!(error)),
            accepted = listener.accept() => {
                let (mut stream,_) = match accepted { Ok(value)=>value,Err(error)=>break Err(error.into()) };
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
                                send(&mut stream,worker_sample(&app)).await
                            }
                            WorkerRequest::Monitor => {
                                let snapshot = astra_observe::snapshot(format!("{boot}:{generation}"));
                                send(&mut stream, snapshot).await
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
    };
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    outcome
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
    let _route_leases = app.route_leases.clone();
    drop(app);
    remote.write_all(&prefix).await?;
    drop(prefix);
    crate::proxy::relay::relay(&mut stream, &mut remote).await?;
    Ok(())
}

#[cfg(test)]
#[path = "test/hosted_monitor_test.rs"]
mod monitor_tests;
