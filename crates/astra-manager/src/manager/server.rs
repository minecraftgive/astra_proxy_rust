//! A serialized mutation actor with independent read, push and log connections.
use super::*;
mod group_diagnostic;
use astra_ipc::progress;
use astra_ipc::session::Event;
use futures_util::{stream, StreamExt};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use tokio::sync::{mpsc, oneshot, watch, Semaphore};

struct AbortTasks(Vec<tokio::task::AbortHandle>);
impl Drop for AbortTasks {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

#[derive(Clone)]
struct Route {
    instance: Instance,
    pid: Option<u32>,
    socket: PathBuf,
}
#[derive(Clone)]
struct Base {
    generation: u64,
    reply: Reply,
    routes: HashMap<String, Route>,
    registry: Registry,
    levels: HashMap<String, LogLevel>,
}
pub(super) struct Hub {
    runtime_dir: PathBuf,
    network_scheduler: std::sync::OnceLock<Arc<crate::network::Scheduler>>,
    home: PathBuf,
    config: PathBuf,
    bin: PathBuf,
    base: watch::Sender<Base>,
    list: watch::Sender<Reply>,
    updates: watch::Sender<crate::protocol::updates::UpdateStatus>,
    generation: AtomicU64,
    log: Arc<crate::tui_log::Log>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    elevations: std::sync::Mutex<HashMap<String, ElevationSlot>>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct ElevationSlot {
    ticket: String,
    updates: watch::Sender<Option<super::elevation::Outcome>>,
    started: std::time::Instant,
}
impl Hub {
    fn new(manager: &Manager) -> Result<Arc<Self>> {
        let initial = Base {
            generation: 0,
            reply: Reply::text(""),
            routes: HashMap::new(),
            registry: Registry::default(),
            levels: HashMap::new(),
        };
        let hub = Arc::new(Self {
            runtime_dir: manager.runtime_dir.clone(),
            network_scheduler: Default::default(),
            home: manager.home.clone(),
            config: manager.config_home.clone(),
            bin: manager.bin.clone(),
            base: watch::channel(initial).0,
            list: watch::channel(Reply::text("")).0,
            updates: watch::channel(manager.cached_update_status()).0,
            generation: AtomicU64::new(0),
            log: Arc::new(crate::tui_log::Log::open(&manager.home.join("tui.log"))?),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            elevations: std::sync::Mutex::new(HashMap::new()),
        });
        hub.publish(manager);
        Ok(hub)
    }
    fn spawn_legacy<T, F>(self: &Arc<Self>, work: F) -> tokio::task::JoinHandle<Result<T>>
    where
        T: Send + 'static,
        F: std::future::Future<Output = Result<T>> + Send + 'static,
    {
        let hub = self.clone();
        tokio::spawn(async move {
            let owner = hub.network_scheduler.get();
            let _permit = match owner {
                Some(owner) => Some(owner.legacy_permit().await?),
                None => None,
            };
            let result = work.await;
            if let Some(owner) = owner {
                owner.record_legacy_cleanup(&result);
            }
            result
        })
    }
    pub(super) fn publish(&self, manager: &Manager) {
        let previous = self.list.borrow().clone();
        let mut reply = Reply::text("");
        let mut details = HashMap::new();
        let mut routes = HashMap::new();
        for instance in &manager.registry.instances {
            let run = manager.running.get(&instance.name);
            let pid = run.and_then(|r| r.child.id());
            routes.insert(
                instance.name.clone(),
                Route {
                    instance: instance.clone(),
                    pid,
                    socket: manager.socket(&instance.name),
                },
            );
            details.insert(instance.name.clone(), manager.details(instance));
            reply.rows.push(Row {
                instance: instance.clone(),
                pid,
                state: if pid.is_some() {
                    "Running".into()
                } else {
                    manager
                        .exits
                        .get(&instance.name)
                        .cloned()
                        .unwrap_or("Stopped".into())
                },
                uptime: previous
                    .rows
                    .iter()
                    .find(|r| r.instance.name == instance.name && r.pid == pid)
                    .map_or(0, |r| r.uptime),
                changed: run.is_some_and(|r| {
                    read_config(&instance.config).is_ok_and(|s| s != r.config)
                        || instance.config != r.config_path
                }),
            });
        }
        reply.text = serde_json::to_string(&details).unwrap_or_default();
        self.base.send_replace(Base {
            generation: self.generation.fetch_add(1, Ordering::Relaxed) + 1,
            reply: reply.clone(),
            routes,
            registry: manager.registry.clone(),
            levels: manager.runtime.log_levels.clone(),
        });
        self.list.send_replace(reply);
        self.publish_updates(manager.cached_update_status());
    }
    pub(super) fn publish_updates(&self, mut status: crate::protocol::updates::UpdateStatus) {
        // Runtime observations are published independently of the mutation actor.
        let routes = self.base.borrow().routes.clone();
        self.updates.send_modify(|current| {
            status.instances = routes
                .iter()
                .filter(|(_, route)| route.instance.kind == Kind::Proxy && route.pid.is_some())
                .map(|(name, route)| {
                    (
                        name.clone(),
                        current
                            .instances
                            .get(name)
                            .filter(|instance| instance.pid == route.pid)
                            .cloned()
                            .unwrap_or_else(|| crate::protocol::updates::UpdateInstance {
                                pid: route.pid,
                                error: Some("Waiting for network host status".into()),
                                ..Default::default()
                            }),
                    )
                })
                .collect();
            status.observed_at_ms = current.observed_at_ms;
            apply_instance_gate(&mut status);
            *current = status.clone();
        });
    }
    pub(super) fn update_status_snapshot(&self) -> crate::protocol::updates::UpdateStatus {
        self.updates.borrow().clone()
    }
    fn reader(&self) -> Manager {
        let base = self.base.borrow();
        Manager {
            runtime_dir: self.runtime_dir.clone(),
            updates: Default::default(),
            home: self.home.clone(),
            config_home: self.config.clone(),
            bin: self.bin.clone(),
            registry: base.registry.clone(),
            runtime: RuntimeState {
                log_levels: base.levels.clone(),
            },
            running: HashMap::new(),
            blocked: HashMap::new(),
            exits: HashMap::new(),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            elevations: HashMap::new(),
            elevation_events: None,
            publisher: None,
        }
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(super) fn begin_elevation(&self, name: &str, ticket: &str) {
        let mut elevations = self.elevations.lock().unwrap();
        let now = std::time::Instant::now();
        elevations.retain(|_, slot| now.duration_since(slot.started) < Duration::from_secs(600));
        elevations.insert(
            name.to_owned(),
            ElevationSlot {
                ticket: ticket.to_owned(),
                updates: watch::channel(None).0,
                started: now,
            },
        );
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(super) fn finish_elevation(
        &self,
        name: &str,
        ticket: &str,
        outcome: super::elevation::Outcome,
    ) {
        let mut elevations = self.elevations.lock().unwrap();
        if let Some(slot) = elevations.get_mut(name) {
            if astra_platform::unix_handoff::ticket_eq(&slot.ticket, ticket) {
                slot.updates.send_replace(Some(outcome));
            }
            return;
        }
        elevations.insert(
            name.to_owned(),
            ElevationSlot {
                ticket: ticket.to_owned(),
                updates: watch::channel(Some(outcome)).0,
                started: std::time::Instant::now(),
            },
        );
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn wait_elevation(
        &self,
        name: &str,
        ticket: &str,
    ) -> Result<watch::Receiver<Option<super::elevation::Outcome>>> {
        let elevations = self.elevations.lock().unwrap();
        let slot = elevations.get(name).context("Unknown elevation request")?;
        anyhow::ensure!(
            astra_platform::unix_handoff::ticket_eq(&slot.ticket, ticket),
            "Elevation request changed"
        );
        Ok(slot.updates.subscribe())
    }
    fn route(&self, name: &str) -> Result<Route> {
        self.base.borrow().routes.get(name).cloned().ok_or_else(|| {
            crate::error::message(
                astra_management::v2::ErrorCode::NotFound,
                "Unknown instance",
            )
        })
    }
    fn plan(&self, name: &str) -> Result<DiagnosticPlan> {
        let route = self.route(name)?;
        anyhow::ensure!(
            route.instance.kind == Kind::Proxy,
            "Select a proxy instance for diagnostics"
        );
        Ok(DiagnosticPlan {
            runtime_dir: self.runtime_dir.clone(),
            log_path: self.home.join(format!("{name}.log")),
            executable: self.bin.join(Kind::Proxy.binary()),
            config_path: route.instance.config.clone(),
            text: if route.pid.is_some() {
                String::new()
            } else {
                read_config(&route.instance.config)?
            },
            running: route.pid.is_some(),
            control: route.pid.map(|pid| (route.socket, Some(pid))),
        })
    }
}
struct Work {
    action: Action,
    // Internal dispatch policy, never accepted from the wire. Execute clients
    // can use a registered privilege service but cannot request UAC.
    allow_runtime_uac: bool,
    reply: oneshot::Sender<Result<Reply>>,
    progress: progress::Sender,
    save: Option<oneshot::Sender<Result<astra_management::v2::SaveOutcome>>>,
}

fn apply_instance_gate(status: &mut crate::protocol::updates::UpdateStatus) {
    let capabilities = &mut status.capabilities;
    let reason = if !capabilities.experimental_enabled {
        Some("Online activation is disabled in this host session")
    } else if !capabilities.ordinary_user {
        Some("Online activation requires an ordinary-user manager")
    } else if capabilities.operation_running || capabilities.recovery_operation_id.is_some() {
        Some("An update is running or requires recovery")
    } else if status
        .instances
        .values()
        .any(|instance| !instance.reachable)
    {
        Some("A running instance is legacy or unavailable")
    } else if status.instances.values().any(|instance| {
        instance.status.as_ref().is_none_or(|s| {
            s.get("boot_id")
                .and_then(|v| v.as_str())
                .is_none_or(str::is_empty)
        })
    }) {
        Some("A network host does not support recoverable update identities")
    } else if status.instances.values().any(|instance| {
        instance.status.as_ref().is_some_and(|s| {
            s.get("paused").and_then(|v| v.as_bool()) == Some(true)
                || s.get("busy").and_then(|v| v.as_bool()) == Some(true)
                || s.get("generations")
                    .and_then(|v| v.as_array())
                    .is_some_and(|items| {
                        items
                            .iter()
                            .any(|g| g.get("prepared").and_then(|v| v.as_bool()) == Some(true))
                    })
        })
    }) {
        Some("A network host is paused or preparing a generation")
    } else {
        None
    };
    capabilities.activation_allowed = reason.is_none();
    capabilities.activation_blocked_reason = reason.map(str::to_owned);
}

#[derive(Clone)]
pub(crate) struct NativeClient {
    hub: Arc<Hub>,
    sender: mpsc::Sender<Work>,
    stopped: mpsc::Sender<()>,
    diagnostics: Arc<Semaphore>,
    probes: super::probes::Probes,
}
impl NativeClient {
    pub(crate) fn attach_network_scheduler(
        &self,
        scheduler: Arc<crate::network::Scheduler>,
    ) -> Result<()> {
        self.hub
            .network_scheduler
            .set(scheduler)
            .map_err(|_| anyhow::anyhow!("Diagnostic scheduler already attached"))
    }
    /// The background diagnostic scheduler owns concurrency and cancellation.
    pub(crate) async fn network_diagnostic(
        &self,
        name: &str,
        request: astra_model::nettool::DetailedRequest,
        output: mpsc::Sender<astra_model::nettool::Event>,
        cancel: watch::Receiver<bool>,
    ) -> Result<()> {
        self.hub
            .plan(name)?
            .network_diagnostic(request, output, cancel)
            .await
    }

    /// One header exit probe; `keep_alive` keeps the probe core loaded between rounds.
    pub(crate) async fn exit_probe(
        &self,
        name: &str,
        request: astra_model::diagnostics::ProbeRequest,
        keep_alive: Option<Duration>,
    ) -> Result<astra_model::diagnostics::Probe> {
        let plan = self.hub.plan(name)?;
        let probes = self.probes.clone();
        let name = name.to_owned();
        // Agent NAT exclusivity includes loaded probe cores; do not retain an
        // idle core that could perform profile background work outside its slot.
        let keep_alive = if self.hub.network_scheduler.get().is_some() {
            None
        } else {
            keep_alive
        };
        self.hub
            .spawn_legacy(async move { probes.probe(&name, plan, request, keep_alive).await })
            .await
            .context("Exit probe owner failed")?
    }
    pub(crate) async fn save(
        &self,
        name: String,
        original: String,
        text: String,
    ) -> Result<astra_management::v2::SaveOutcome> {
        let (sender, mut receiver) = oneshot::channel();
        let (reply, _) = oneshot::channel();
        let (progress, mut updates) = watch::channel(None);
        self.sender
            .send(Work {
                action: Action::Save {
                    name,
                    original,
                    text,
                },
                reply,
                progress,
                save: Some(sender),
                allow_runtime_uac: true,
            })
            .await
            .context("Native actor stopped")?;
        loop {
            tokio::select! {result=&mut receiver=>return result.map_err(|error| crate::error::classified(astra_management::v2::ErrorCode::OutcomeUnknown, anyhow::Error::new(error).context("Save outcome unknown; native actor stopped")))?,changed=updates.changed()=>{if changed.is_err(){return receiver.await.map_err(|error| crate::error::classified(astra_management::v2::ErrorCode::OutcomeUnknown, anyhow::Error::new(error).context("Save outcome unknown; native actor stopped")))?;}let update=updates.borrow_and_update().clone();if let Some(update)=update{progress::report(update.phase,update.detail);}}}
        }
    }
    pub(crate) async fn runtime_sample(&self, name: &str) -> Result<serde_json::Value> {
        let route = self.hub.route(name)?;
        anyhow::ensure!(
            route.instance.kind == Kind::Proxy && route.pid.is_some(),
            "Select a running proxy"
        );
        let endpoint = astra_management::host_control::endpoint(&route.socket);
        let telemetry = timeout(
            Duration::from_secs(2),
            astra_management::host_control::request(
                &endpoint,
                astra_management::host_control::Command::Telemetry,
            ),
        )
        .await;
        let value = match telemetry {
            Ok(Ok(value)) => value,
            _ => {
                let enhanced = self
                    .hub
                    .updates
                    .borrow()
                    .instances
                    .get(name)
                    .and_then(|instance| instance.status.as_ref())
                    .and_then(|status| status.get("capabilities"))
                    .and_then(|value| value.as_array())
                    .is_some_and(|caps| {
                        caps.iter()
                            .any(|v| v.as_str() == Some("runtime_telemetry_v1"))
                    });
                anyhow::ensure!(!enhanced,"Host telemetry is temporarily unavailable; refresh before changing runtime state");
                serde_json::to_value(
                    control::runtime(&route.socket, astra_model::runtime::Command::Snapshot)
                        .await?,
                )?
            }
        };
        anyhow::ensure!(
            value.get("pid").and_then(|v| v.as_u64()) == route.pid.map(u64::from)
                && self.hub.route(name)?.pid == route.pid,
            "Core PID changed"
        );
        Ok(value)
    }
    pub(crate) async fn call(&self, action: Action) -> Result<Reply> {
        self.call_with_runtime_policy(action, true).await
    }
    pub(crate) async fn enable_tun_without_uac(
        &self,
        name: String,
        expected_pid: Option<u32>,
    ) -> Result<Reply> {
        self.call_with_runtime_policy(
            Action::Runtime {
                name,
                command: astra_model::runtime::Command::Tun(true),
                expected_pid,
            },
            false,
        )
        .await
    }
    async fn call_with_runtime_policy(
        &self,
        action: Action,
        allow_runtime_uac: bool,
    ) -> Result<Reply> {
        let shutdown = matches!(action, Action::Shutdown);
        let immediate = if allow_runtime_uac {
            fast(&self.hub, &action).await
        } else {
            // Keep the no-UAC decision on the serialized mutation actor.
            None
        };
        let result = if let Some(result) = immediate {
            result
        } else if matches!(
            &action,
            Action::Diagnostics(_)
                | Action::DiagnosticTargets(_)
                | Action::DiagnosticsSelected { .. }
        ) || matches!(&action,Action::UpdateSubscriptions(name)if self.hub.route(name).is_ok_and(|r|r.pid.is_none()))
        {
            let _permit = self
                .diagnostics
                .try_acquire()
                .context("Diagnostics are busy; retry shortly")?;
            let name = match &action {
                Action::Diagnostics(n)
                | Action::DiagnosticTargets(n)
                | Action::UpdateSubscriptions(n)
                | Action::DiagnosticsSelected { name: n, .. } => n,
                _ => unreachable!(),
            };
            let plan = self.hub.plan(name)?;
            self.hub
                .spawn_legacy(async move {
                    match action {
                        Action::UpdateSubscriptions(_) => plan.run_refresh().await,
                        Action::DiagnosticTargets(_) => plan.run_selection(None, true).await,
                        Action::DiagnosticsSelected { targets, .. } => {
                            plan.run_selection(Some(targets), false).await
                        }
                        _ => plan.run().await,
                    }
                })
                .await
                .context("Legacy diagnostic owner failed")?
        } else {
            let (sender, mut response) = oneshot::channel();
            let (progress, mut updates) = watch::channel(None);
            self.sender
                .send(Work {
                    action,
                    reply: sender,
                    progress,
                    save: None,
                    allow_runtime_uac,
                })
                .await
                .context("Native actor stopped")?;
            loop {
                tokio::select! {result=&mut response=>break result.map_err(|error| crate::error::classified(astra_management::v2::ErrorCode::OutcomeUnknown, anyhow::Error::new(error).context("Native actor outcome unknown")))?,changed=updates.changed()=>{if changed.is_err(){break response.await.map_err(|error| crate::error::classified(astra_management::v2::ErrorCode::OutcomeUnknown, anyhow::Error::new(error).context("Native actor outcome unknown")))?;}let update=updates.borrow_and_update().clone();if let Some(update)=update{progress::report(update.phase,update.detail);}}}
            }
        }?;
        if shutdown {
            let _ = self.stopped.send(()).await;
        }
        if let Some(error) = result.error {
            anyhow::bail!(error);
        }
        Ok(result)
    }
    pub(crate) async fn nettool(
        &self,
        name: &str,
        request: astra_model::nettool::Request,
        output: mpsc::Sender<astra_model::nettool::Event>,
    ) -> Result<Vec<astra_model::nettool::Event>> {
        let _permit = self
            .diagnostics
            .try_acquire()
            .context("Diagnostics are busy; retry shortly")?;
        let plan = self.hub.plan(name)?;
        let (reader, writer) = tokio::io::duplex(64 * 1024);
        let producer = self.hub.spawn_legacy(async move {
            let mut writer = writer;
            plan.nettool(request, &mut writer).await
        });
        let mut reader = reader;
        let mut events = Vec::new();
        let received = async {
            loop {
                let reply: Reply = astra_ipc::receive_management(&mut reader).await?;
                if let Some(error) = reply.error {
                    anyhow::bail!(error);
                }
                let event: astra_model::nettool::Event = serde_json::from_str(&reply.text)?;
                let done = matches!(event, astra_model::nettool::Event::Done);
                anyhow::ensure!(events.len() < 256, "Network event limit exceeded");
                let _ = output.send(event.clone()).await;
                events.push(event);
                if done {
                    break;
                }
            }
            Ok::<_, anyhow::Error>(events)
        }
        .await;
        drop(reader);
        let produced = producer.await.context("Legacy network tool owner failed")?;
        produced?;
        received
    }
    pub(crate) async fn diagnostics(
        &self,
        name: &str,
        targets: Vec<String>,
        output: mpsc::Sender<astra_model::diagnostics::Event>,
    ) -> Result<astra_model::diagnostics::Report> {
        use astra_model::diagnostics::{Event, Report};
        let _permit = self
            .diagnostics
            .try_acquire()
            .context("Diagnostics are busy; retry shortly")?;
        let plan = self.hub.plan(name)?;
        let (mut reader, mut writer) = tokio::io::duplex(64 * 1024);
        let producer = self
            .hub
            .spawn_legacy(async move { plan.stream(targets, &mut writer).await });
        let mut report = Report {
            url: String::new(),
            running_config: false,
            probes: vec![],
        };
        let mut count = 0;
        let received = async {
            loop {
                let reply: Reply = astra_ipc::receive_management(&mut reader).await?;
                if let Some(error) = reply.error {
                    anyhow::bail!(error);
                }
                let event: Event = serde_json::from_str(&reply.text)?;
                count += 1;
                anyhow::ensure!(count <= 4096, "Diagnostics event limit exceeded");
                match &event {
                    Event::Started {
                        url,
                        running_config,
                        ..
                    } => {
                        report.url = url.clone();
                        report.running_config = *running_config;
                    }
                    Event::Finished { probe } => report.probes.push(probe.clone()),
                    _ => {}
                }
                let done = matches!(event, Event::Done);
                let _ = output.send(event).await;
                if done {
                    break;
                }
            }
            Ok::<_, anyhow::Error>(report)
        }
        .await;
        drop(reader);
        let produced = producer.await.context("Legacy diagnostic owner failed")?;
        produced?;
        received
    }
}
pub(super) async fn run(
    mut manager: Manager,
    listener: Option<UnixListener>,
    ready: Option<oneshot::Sender<NativeClient>>,
) -> Result<()> {
    let hub = Hub::new(&manager)?;
    manager.publisher = Some(hub.clone());
    let (sender, mut requests) = mpsc::channel::<Work>(32);
    let (stopped, mut stop) = mpsc::channel::<()>(1);
    let (elevation_sender, mut elevation_events) = mpsc::unbounded_channel::<ElevationEvent>();
    manager.elevation_events = Some(elevation_sender);
    let actor_hub = hub.clone();
    let mut owned_tasks = AbortTasks(Vec::new());
    let mut actor = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        loop {
            tokio::select! {
                Some(event)=elevation_events.recv()=>{
                    apply_elevation(&mut manager,event).await;
                    actor_hub.publish(&manager);
                }
                work=requests.recv()=>{
                    let Some(work)=work else {return Err(anyhow::anyhow!("manager command channel closed unexpectedly"));};
                    if let Some(save)=work.save{
                        let result=if let Action::Save{name,original,text}=work.action{progress::scope(work.progress,manager.save_typed(name,original,text)).await}else{Err(anyhow::anyhow!("Invalid typed save command"))};
                        actor_hub.publish(&manager);let _=save.send(result);continue;
                    }
                    let title=crate::action_description(&work.action);
                    let shutdown=matches!(work.action,Action::Shutdown);
                    actor_hub.log.write("manager",&format!("started {title}"));
                    work.progress.send_replace(Some(progress::Progress::new("Running",title.clone())));
                    let result=progress::scope(work.progress,async {
                        if !work.allow_runtime_uac {
                            let Action::Runtime{name,command,expected_pid}=work.action else {
                                anyhow::bail!("Non-interactive runtime policy requires a runtime command");
                            };
                            manager.reap()?;
                            manager.action_runtime_with_policy(name,command,expected_pid,false).await
                        } else {
                            manager.action(work.action).await
                        }
                    }).await;
                    let shutdown_error=if shutdown {result.as_ref().err().map(|error| format!("{error:#}")).or_else(|| result.as_ref().ok().and_then(|reply| reply.error.clone()))} else {None};
                    let failed=result.as_ref().map_or(true, |reply| reply.error.is_some());
                    actor_hub.log.write("manager",&format!("{} {title}",if failed{"failed"}else{"completed"}));
                    actor_hub.publish(&manager);
                    let _=work.reply.send(result);
                    if shutdown{if let Some(error)=shutdown_error {return Err(anyhow::anyhow!(error));} break;}
                }
                _=tick.tick()=>{
                    let _=manager.reap();
                    if manager.updates.status.operation.as_ref().is_none_or(|operation|operation.phase.terminal()) {
                        manager.updates.status.rule_sync_error = match manager.refresh_rule_generations().await {
                            Ok(()) => None,
                            Err(error) => { let message=format!("rule generation refresh deferred: {error:#}"); actor_hub.log.write("manager",&message); Some(message) }
                        };
                    }
                    actor_hub.publish(&manager);
                }
            }
        }
        Ok::<(), anyhow::Error>(())
    });
    owned_tasks.0.push(actor.abort_handle());
    let poll_hub = hub.clone();
    let mut polling = tokio::spawn(async move {
        let mut tick = tokio::time::interval(crate::UI_REFRESH_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let base = poll_hub.base.borrow().clone();
            let rows = base.reply.rows;
            let routes = &base.routes;
            let rows: Vec<Row> = stream::iter(rows)
                .map(|mut row| async move {
                    if let Some(pid) = row.pid {
                        match control::request(&routes[&row.instance.name].socket, Command::Status)
                            .await
                        {
                            Ok(status) if status.pid == pid => {
                                row.uptime = status.uptime_secs;
                                row.state = "Running".into();
                            }
                            _ => row.state = "Unresponsive".into(),
                        }
                    }
                    row
                })
                .buffered(8)
                .collect()
                .await;
            if poll_hub.base.borrow().generation == base.generation {
                let mut reply = Reply::text(base.reply.text);
                reply.rows = rows;
                poll_hub.list.send_replace(reply);
            }
        }
    });
    let permits = Arc::new(Semaphore::new(64));
    owned_tasks.0.push(polling.abort_handle());
    let update_hub = hub.clone();
    let mut update_polling = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let routes: Vec<_> = update_hub
                .base
                .borrow()
                .routes
                .iter()
                .filter(|(_, route)| route.instance.kind == Kind::Proxy && route.pid.is_some())
                .map(|(name, route)| (name.clone(), route.clone()))
                .collect();
            let observed: Vec<_> = stream::iter(routes)
                .map(|(name, route)| async move {
                    let endpoint = astra_management::host_control::endpoint(&route.socket);
                    let result = timeout(
                        Duration::from_millis(750),
                        astra_management::host_control::request(
                            &endpoint,
                            astra_management::host_control::Command::Status,
                        ),
                    )
                    .await;
                    let instance = match result {
                        Ok(Ok(value)) => crate::protocol::updates::UpdateInstance {
                            pid: route.pid,
                            reachable: true,
                            status: Some(value),
                            error: None,
                        },
                        Ok(Err(error)) => crate::protocol::updates::UpdateInstance {
                            pid: route.pid,
                            error: Some(format!("Legacy host or unavailable: {error:#}")),
                            ..Default::default()
                        },
                        Err(_) => crate::protocol::updates::UpdateInstance {
                            pid: route.pid,
                            error: Some("Host status timed out".into()),
                            ..Default::default()
                        },
                    };
                    (name, route.pid, instance)
                })
                .buffer_unordered(4)
                .collect()
                .await;
            let instances = observed
                .into_iter()
                .filter(|(name, pid, _)| {
                    update_hub.route(name).is_ok_and(|route| route.pid == *pid)
                })
                .map(|(name, _, instance)| (name, instance))
                .collect();
            update_hub.updates.send_modify(|status| {
                status.instances = instances;
                status.observed_at_ms = progress::now_ms();
                apply_instance_gate(status);
            });
        }
    });
    let diagnostics = Arc::new(Semaphore::new(2));
    owned_tasks.0.push(update_polling.abort_handle());
    if let Some(ready) = ready {
        let _ = ready.send(NativeClient {
            hub: hub.clone(),
            sender: sender.clone(),
            stopped: stopped.clone(),
            diagnostics: diagnostics.clone(),
            probes: Default::default(),
        });
    }
    let followers = Arc::new(Semaphore::new(16));
    let mut clients = tokio::task::JoinSet::new();
    let signal = astra_ipc::shutdown_signal();
    tokio::pin!(signal);
    let mut actor_joined = false;
    let outcome = loop {
        tokio::select! {
            result=&mut actor=>{
                actor_joined=true;
                let result=result.context("native manager actor failed").and_then(|result|result);
                if result.is_ok() {
                    // Shutdown's caller must flush its acknowledgement before
                    // the supervisor closes the connection tasks.
                    let _=timeout(Duration::from_secs(3),stop.recv()).await;
                }
                break result;
            },
            result=&mut polling=>break Err(anyhow::anyhow!("manager status observer stopped unexpectedly: {result:?}")),
            result=&mut update_polling=>break Err(anyhow::anyhow!("manager update observer stopped unexpectedly: {result:?}")),
            result=&mut signal=>break result.map_err(Into::into),
            _=stop.recv()=>break Ok(()),
            result=async{match &listener{Some(listener)=>listener.accept().await,None=>std::future::pending().await}}=>{
                let (stream,_)=match result {Ok(value)=>value,Err(error) if astra_platform::supervision::transient_io(&error)=>{sleep(Duration::from_millis(100)).await;continue;},Err(error)=>break Err(error.into())};
                if let Ok(permit)=permits.clone().try_acquire_owned(){let (hub,sender,stopped,diagnostics,followers)=(hub.clone(),sender.clone(),stopped.clone(),diagnostics.clone(),followers.clone());
                    clients.spawn(async move {let _permit=permit;let _=serve(stream,hub,sender,stopped,diagnostics,followers).await;});}
            }
            Some(_)=clients.join_next(),if !clients.is_empty()=>{},
        }
    };
    polling.abort();
    update_polling.abort();
    clients.abort_all();
    while clients.join_next().await.is_some() {}
    if outcome.is_err() {
        // Aborting management detaches committed services; candidates retain RAII cleanup.
        actor.abort();
    } else if !actor.is_finished() {
        let (reply, _) = oneshot::channel();
        let (progress, _) = watch::channel(None);
        let _ = timeout(
            Duration::from_secs(3),
            sender.send(Work {
                action: Action::Shutdown,
                reply,
                progress,
                save: None,
                allow_runtime_uac: true,
            }),
        )
        .await;
    }
    drop(sender);
    if !actor_joined {
        match timeout(Duration::from_secs(35), &mut actor).await {
            Ok(Err(error)) if outcome.is_ok() => {
                return Err(error).context("manager shutdown actor failed")
            }
            Ok(Ok(Err(error))) if outcome.is_ok() => {
                return Err(error).context("manager shutdown incomplete")
            }
            Err(_) => {
                actor.abort();
                return Err(anyhow::anyhow!(
                    "manager shutdown timed out; inspect retained core identities"
                ));
            }
            _ => {}
        }
    }
    outcome
}
fn reply(result: Result<Reply>) -> Reply {
    result.unwrap_or_else(|error| {
        let mut reply = Reply::text("");
        reply.error = Some(format!("{error:#}"));
        reply
    })
}
async fn send(
    stream: &mut astra_platform::platform::LocalStream,
    value: &(impl Serialize + Sync),
) -> Result<()> {
    timeout(
        Duration::from_secs(3),
        astra_ipc::send_management(stream, value),
    )
    .await??;
    Ok(())
}

/// Closing a subscription can race with its next pushed snapshot. A closed
/// peer is a normal end of subscription, including Windows ERROR_NO_DATA.
async fn send_subscription(
    stream: &mut astra_platform::platform::LocalStream,
    value: &(impl Serialize + Sync),
) -> Result<bool> {
    match send(stream, value).await {
        Ok(()) => Ok(true),
        Err(error)
            if error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                matches!(
                    error.kind(),
                    std::io::ErrorKind::BrokenPipe
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::NotConnected
                        | std::io::ErrorKind::UnexpectedEof
                )
            }) =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}
async fn serve(
    mut stream: astra_platform::platform::LocalStream,
    hub: Arc<Hub>,
    sender: mpsc::Sender<Work>,
    stopped: mpsc::Sender<()>,
    diagnostics: Arc<Semaphore>,
    followers: Arc<Semaphore>,
) -> Result<()> {
    use tokio::io::AsyncReadExt;
    let first: Request = timeout(
        Duration::from_secs(3),
        astra_ipc::receive_management(&mut stream),
    )
    .await??;
    if first.version != astra_ipc::API_VERSION {
        send(
            &mut stream,
            &reply(Err(anyhow::anyhow!("Unsupported manager version"))),
        )
        .await?;
        return Ok(());
    }
    let session = matches!(first.action, Action::Session);
    let mut next = Some(first);
    if session {
        send(&mut stream, &Event::<Reply>::Ready(astra_ipc::API_VERSION)).await?;
        next = None;
    }
    loop {
        let request = match next.take() {
            Some(request) => request,
            None => astra_ipc::receive_management::<Request>(&mut stream).await?,
        };
        anyhow::ensure!(
            request.version == astra_ipc::API_VERSION,
            "Unsupported manager version"
        );
        match &request.action {
            Action::WatchUpdates => {
                let mut updates = hub.updates.subscribe();
                let mut heartbeat = tokio::time::interval_at(
                    tokio::time::Instant::now() + Duration::from_secs(5),
                    Duration::from_secs(5),
                );
                loop {
                    let status = updates.borrow_and_update().clone();
                    if !send_subscription(
                        &mut stream,
                        &Reply::text(serde_json::to_string(&status)?),
                    )
                    .await?
                    {
                        return Ok(());
                    }
                    tokio::select! {result=updates.changed()=>{result?;},_=heartbeat.tick()=>{},_=stream.read_u8()=>return Ok(())}
                }
            }
            Action::WatchList => {
                let mut updates = hub.list.subscribe();
                let mut heartbeat = tokio::time::interval_at(
                    tokio::time::Instant::now() + Duration::from_secs(5),
                    Duration::from_secs(5),
                );
                loop {
                    let snapshot = updates.borrow_and_update().clone();
                    if !send_subscription(&mut stream, &snapshot).await? {
                        return Ok(());
                    }
                    tokio::select! {_=updates.changed()=>{},_=heartbeat.tick()=>{},_=stream.read_u8()=>return Ok(())}
                }
            }
            Action::WatchRuntime {
                name,
                expected_pid,
                interval_ms,
            } => {
                let route = hub.route(name)?;
                anyhow::ensure!(route.pid == Some(*expected_pid), "Core PID changed");
                let endpoint = astra_management::host_control::endpoint(&route.socket);
                let enhanced = timeout(
                    Duration::from_millis(750),
                    astra_management::host_control::request(
                        &endpoint,
                        astra_management::host_control::Command::Status,
                    ),
                )
                .await
                .ok()
                .and_then(Result::ok)
                .is_some_and(|status| {
                    status
                        .get("capabilities")
                        .and_then(|v| v.as_array())
                        .is_some_and(|items| {
                            items
                                .iter()
                                .any(|v| v.as_str() == Some("runtime_telemetry_v1"))
                        })
                });
                if enhanced {
                    let mut tick = tokio::time::interval(Duration::from_millis(
                        (*interval_ms).clamp(100, 5000),
                    ));
                    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    loop {
                        tokio::select! { _=tick.tick()=>{}, _=stream.read_u8()=>return Ok(()) }
                        let value = timeout(
                            Duration::from_secs(2),
                            astra_management::host_control::request(
                                &endpoint,
                                astra_management::host_control::Command::Telemetry,
                            ),
                        )
                        .await??;
                        anyhow::ensure!(
                            value.get("pid").and_then(|v| v.as_u64()) == Some(*expected_pid as u64)
                                && hub.route(name)?.pid == Some(*expected_pid),
                            "Core PID changed"
                        );
                        // One atomic host sample: never splice metadata onto a separately sampled Snapshot.
                        send(&mut stream, &Reply::text(serde_json::to_string(&value)?)).await?;
                    }
                }
                let mut core = control::watch_runtime(&route.socket, *interval_ms).await?;
                loop {
                    let snapshot: std::result::Result<astra_model::runtime::Snapshot, String> = tokio::select! {
                        result=timeout(Duration::from_secs(5),astra_ipc::receive_management(&mut core))=>result??,
                        _=stream.read_u8()=>return Ok(()),
                    };
                    let snapshot = snapshot.map_err(anyhow::Error::msg)?;
                    anyhow::ensure!(
                        snapshot.pid == *expected_pid
                            && hub.route(name)?.pid == Some(*expected_pid),
                        "Core PID changed"
                    );
                    send(&mut stream, &Reply::text(serde_json::to_string(&snapshot)?)).await?;
                }
            }
            Action::FollowLogs { name, cursor } => {
                let _permit = followers
                    .try_acquire()
                    .context("Too many log subscribers")?;
                let path = if let Some(file) = crate::tui_log::source_file(name) {
                    hub.home.join(file)
                } else {
                    hub.route(name)?;
                    hub.home.join(format!("{name}.log"))
                };
                send(&mut stream, &Reply::text("Streaming logs")).await?;
                crate::log_stream::serve(stream, path, *cursor).await;
                return Ok(());
            }
            Action::DiagnosticsStream { name, targets } => {
                let _permit = diagnostics
                    .try_acquire()
                    .context("Diagnostics are busy; retry shortly")?;
                let plan = hub.plan(name)?;
                let targets = targets.clone();
                return hub
                    .spawn_legacy(async move {
                        let mut output = stream;
                        if let Err(error) = plan.stream(targets, &mut output).await {
                            let cleanup = error
                                .downcast_ref::<crate::network::CleanupUnconfirmed>()
                                .is_some();
                            let text = format!("{error:#}");
                            let _ =
                                astra_ipc::send_management(&mut output, &reply(Err(error))).await;
                            if cleanup {
                                return Err(crate::network::CleanupUnconfirmed(text).into());
                            }
                        }
                        Ok(())
                    })
                    .await
                    .context("Legacy stream owner failed")?;
            }
            Action::NetTool { name, request } => {
                let _permit = diagnostics
                    .try_acquire()
                    .context("Diagnostics are busy; retry shortly")?;
                let plan = hub.plan(name)?;
                let request = request.clone();
                return hub
                    .spawn_legacy(async move {
                        let mut output = stream;
                        if let Err(error) = plan.nettool(request, &mut output).await {
                            let cleanup = error
                                .downcast_ref::<crate::network::CleanupUnconfirmed>()
                                .is_some();
                            let text = format!("{error:#}");
                            let _ =
                                astra_ipc::send_management(&mut output, &reply(Err(error))).await;
                            if cleanup {
                                return Err(crate::network::CleanupUnconfirmed(text).into());
                            }
                        }
                        Ok(())
                    })
                    .await
                    .context("Legacy stream owner failed")?;
            }
            _ => {}
        }
        let shutdown = matches!(request.action, Action::Shutdown);
        let response = match fast_progress(&mut stream, &hub, &request.action, session).await {
            Some(result) => reply(result),
            None => {
                if matches!(
                    &request.action,
                    Action::Diagnostics(_)
                        | Action::DiagnosticTargets(_)
                        | Action::DiagnosticsSelected { .. }
                ) || matches!(&request.action,Action::UpdateSubscriptions(name)if hub.route(name).is_ok_and(|route|route.pid.is_none()))
                {
                    let title = crate::action_description(&request.action);
                    hub.log.write("manager", &format!("started {title}"));
                    let (progress, mut updates) = watch::channel(None);
                    let work_hub = hub.clone();
                    let work_diagnostics = diagnostics.clone();
                    let action = request.action.clone();
                    let future = hub.spawn_legacy(progress::scope(progress, async move {
                        let _permit = work_diagnostics
                            .try_acquire()
                            .context("Diagnostics are busy; retry shortly")?;
                        let name = match &action {
                            Action::Diagnostics(n)
                            | Action::DiagnosticTargets(n)
                            | Action::UpdateSubscriptions(n)
                            | Action::DiagnosticsSelected { name: n, .. } => n,
                            _ => unreachable!(),
                        };
                        let plan = work_hub.plan(name)?;
                        progress::report(
                            if matches!(&action, Action::UpdateSubscriptions(_)) {
                                "Refresh subscriptions"
                            } else {
                                "Run diagnostics"
                            },
                            name.clone(),
                        );
                        match action {
                            Action::UpdateSubscriptions(_) => plan.run_refresh().await,
                            Action::DiagnosticTargets(_) => plan.run_selection(None, true).await,
                            Action::DiagnosticsSelected { targets, .. } => {
                                plan.run_selection(Some(targets), false).await
                            }
                            _ => plan.run().await,
                        }
                    }));
                    tokio::pin!(future);
                    let mut open = session;
                    let result = loop {
                        tokio::select! {
                            result=&mut future=>break result.context("Legacy diagnostic owner failed")?,
                            _=stream.read_u8()=>return Ok(()),
                            changed=updates.changed(),if open=>{
                                if changed.is_err(){open=false;continue;}
                                let event=updates.borrow_and_update().clone();
                                if let Some(event)=event {send(&mut stream,&Event::<Reply>::Progress(event)).await?;}
                            }
                        }
                    };
                    hub.log.write(
                        "manager",
                        &format!(
                            "{} {title}",
                            if result.is_ok() {
                                "completed"
                            } else {
                                "failed"
                            }
                        ),
                    );
                    reply(result)
                } else {
                    let (response, mut receiver) = oneshot::channel();
                    let (progress, mut updates) = watch::channel(None);
                    let queued = progress::Progress::new(
                        "Queued",
                        crate::action_description(&request.action),
                    );
                    if session {
                        send(&mut stream, &Event::<Reply>::Progress(queued)).await?;
                    }
                    sender
                        .send(Work {
                            action: request.action,
                            reply: response,
                            progress,
                            save: None,
                            allow_runtime_uac: true,
                        })
                        .await?;
                    loop {
                        tokio::select! {
                            result=&mut receiver=>break reply(result?),
                            changed=updates.changed(),if session=>{
                                if changed.is_err(){break reply(receiver.await?);}
                                let event=updates.borrow_and_update().clone();if let Some(event)=event{hub.log.write("progress",&format!("{} {}",event.phase,event.detail));send(&mut stream,&Event::<Reply>::Progress(event)).await?;}
                            }
                        }
                    }
                }
            }
        };
        if session {
            send(&mut stream, &Event::Reply(response)).await?;
        } else {
            send(&mut stream, &response).await?;
        }
        if shutdown {
            let _ = stopped.send(()).await;
            return Ok(());
        }
        if !session {
            return Ok(());
        }
    }
}
fn refresh_startup_details(mut reply: Reply) -> Result<Reply> {
    let mut details: HashMap<String, InstanceDetails> = serde_json::from_str(&reply.text)?;
    for row in &reply.rows {
        let entry = details.entry(row.instance.name.clone()).or_default();
        let auto_start = read_config_auto_start(&row.instance.config);
        entry.config_unavailable = auto_start.is_err();
        entry.auto_start = auto_start.unwrap_or(false);
    }
    reply.text = serde_json::to_string(&details)?;
    Ok(reply)
}

async fn fast(hub: &Arc<Hub>, action: &Action) -> Option<Result<Reply>> {
    Some(match action {
        Action::Monitor { name } => {
            async {
                let route = hub.route(name)?;
                let snapshot =
                    crate::monitor::core(name, route.instance.kind, route.pid, &route.socket)
                        .await?;
                anyhow::ensure!(
                    hub.route(name)?.pid == route.pid,
                    "Core PID changed during monitor sample"
                );
                Ok(Reply::text(serde_json::to_string(&snapshot)?))
            }
            .await
        }
        Action::List => {
            // Runtime observations can use the pushed snapshot, but startup
            // opt-in must reflect the saved file at this explicit refresh.
            // Keep filesystem reads independent of the mutation actor and
            // off the async worker, including while a long action is running.
            let reply = hub.list.borrow().clone();
            tokio::task::spawn_blocking(move || refresh_startup_details(reply))
                .await
                .unwrap_or_else(|error| Err(error.into()))
        }
        Action::UpdateStatus => serde_json::to_string(&*hub.updates.borrow())
            .map(Reply::text)
            .map_err(Into::into),
        Action::UpdateOperationStatus { operation_id } => {
            let home = hub.home.clone();
            let status = hub.updates.borrow().clone();
            let id = operation_id.clone();
            tokio::task::spawn_blocking(move || {
                Manager::lookup_update_operation(&home, status, &id)
            })
            .await
            .unwrap_or_else(|error| Err(error.into()))
        }
        Action::StoragePaths => serde_json::to_string(&StoragePaths {
            config: hub.config.clone(),
            state: hub.home.clone(),
        })
        .map(Reply::text)
        .map_err(Into::into),
        Action::WaitElevation { name, ticket } => {
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            {
                async {
                    let mut updates = hub.wait_elevation(name, ticket)?;
                    let outcome = timeout(Duration::from_secs(150), async {
                        loop {
                            if let Some(outcome) = updates.borrow().clone() {
                                return outcome;
                            }
                            if updates.changed().await.is_err() {
                                return super::elevation::Outcome::Failed(
                                    "Elevation request was replaced".into(),
                                );
                            }
                        }
                    })
                    .await
                    .map_err(|_| anyhow::anyhow!("Timed out waiting for the elevated core"))?;
                    outcome.into_reply()
                }
                .await
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            {
                let _ = (name, ticket);
                Err(anyhow::anyhow!("TUN elevation is supported on Linux only"))
            }
        }
        Action::Runtime {
            name,
            command,
            expected_pid,
        } => {
            if let astra_model::runtime::Command::TestGroup(group) = command {
                return Some(
                    hub.manual_group_test(name.clone(), group.clone(), *expected_pid)
                        .await,
                );
            }
            #[cfg(windows)]
            if matches!(command, astra_model::runtime::Command::Tun(true)) {
                return None;
            }
            async {
                let route = hub.route(name)?;
                anyhow::ensure!(
                    route.instance.kind == Kind::Proxy && route.pid.is_some(),
                    "Select a running proxy"
                );
                anyhow::ensure!(
                    expected_pid.is_none() || *expected_pid == route.pid,
                    "Core PID changed; refresh before changing runtime state"
                );
                let snapshot = control::runtime(&route.socket, command.clone()).await?;
                anyhow::ensure!(
                    Some(snapshot.pid) == route.pid && hub.route(name)?.pid == route.pid,
                    "Core PID changed"
                );
                Ok(Reply::text(serde_json::to_string(&snapshot)?))
            }
            .await
        }
        Action::GetLogLevel(name) => {
            if crate::tui_log::source_file(name).is_some() {
                Ok(Reply::text("info"))
            } else {
                hub.route(name).map(|_| {
                    Reply::text(
                        hub.base
                            .borrow()
                            .levels
                            .get(name)
                            .copied()
                            .unwrap_or_default()
                            .as_str(),
                    )
                })
            }
        }
        Action::Read(name) if hub.route(name).is_ok_and(|r| r.instance.config.is_file()) => {
            let reader = hub.reader();
            let name = name.clone();
            tokio::task::spawn_blocking(move || {
                let instance = reader.instance(&name)?;
                Ok(Reply::text(read_config(&instance.config)?))
            })
            .await
            .unwrap_or_else(|e| Err(e.into()))
        }
        Action::NodeTags(name) => {
            let reader = hub.reader();
            let name = name.clone();
            tokio::task::spawn_blocking(move || {
                Ok(Reply::text(serde_json::to_string(
                    &reader.node_tags(&name)?,
                )?))
            })
            .await
            .unwrap_or_else(|e| Err(e.into()))
        }
        Action::SubscriptionNodes(name) => {
            let reader = hub.reader();
            let name = name.clone();
            tokio::task::spawn_blocking(move || {
                let instance = reader.instance(&name)?;
                Ok(Reply::text(serde_json::to_string(
                    &crate::subscription_view::read_in(&reader.runtime_dir, &instance.config)?,
                )?))
            })
            .await
            .unwrap_or_else(|e| Err(e.into()))
        }
        _ => return None,
    })
}

async fn fast_progress(
    stream: &mut astra_platform::platform::LocalStream,
    hub: &Arc<Hub>,
    action: &Action,
    session: bool,
) -> Option<Result<Reply>> {
    let (sender, mut updates) = watch::channel(None);
    let future = progress::scope(sender, fast(hub, action));
    tokio::pin!(future);
    let mut open = session;
    loop {
        tokio::select! {
            result=&mut future=>return result,
            changed=updates.changed(),if open=>{
                if changed.is_err(){open=false;continue;}
                let event=updates.borrow_and_update().clone();
                if let Some(event)=event{hub.log.write("progress",&format!("{} {}",event.phase,event.detail));if send(stream,&Event::<Reply>::Progress(event)).await.is_err(){return Some(Err(anyhow::anyhow!("UI disconnected; the core operation may continue")));}}
            }
        }
    }
}

#[cfg(test)]
#[path = "../test/manager_server_update_status_test.rs"]
mod update_status_tests;

#[cfg(test)]
#[path = "../test/manager_server_runtime_policy_test.rs"]
mod runtime_policy_tests;

#[cfg(all(test, unix))]
#[path = "../test/manager_server_network_gate_test.rs"]
mod network_gate_tests;
