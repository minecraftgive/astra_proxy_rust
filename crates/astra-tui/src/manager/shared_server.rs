//! A serialized mutation actor with independent read, push and log connections.
use super::*;
use astra_ipc::{progress, session::Event};
use futures_util::{stream, StreamExt};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use tokio::sync::{mpsc, oneshot, watch, Semaphore};

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
            updates: Default::default(),
            home: self.home.clone(),
            config_home: self.config.clone(),
            bin: self.bin.clone(),
            registry: base.registry.clone(),
            runtime: RuntimeState {
                log_levels: base.levels.clone(),
            },
            running: HashMap::new(),
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
            if astra_ipc::unix_handoff::ticket_eq(&slot.ticket, ticket) {
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
            astra_ipc::unix_handoff::ticket_eq(&slot.ticket, ticket),
            "Elevation request changed"
        );
        Ok(slot.updates.subscribe())
    }
    fn route(&self, name: &str) -> Result<Route> {
        self.base
            .borrow()
            .routes
            .get(name)
            .cloned()
            .context("Unknown instance")
    }
    fn plan(&self, name: &str) -> Result<DiagnosticPlan> {
        let route = self.route(name)?;
        anyhow::ensure!(
            route.instance.kind == Kind::Proxy,
            "Select a proxy instance for diagnostics"
        );
        Ok(DiagnosticPlan {
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
    reply: oneshot::Sender<Reply>,
    progress: progress::Sender,
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

pub(super) async fn run(mut manager: Manager, listener: UnixListener) -> Result<()> {
    let hub = Hub::new(&manager)?;
    manager.publisher = Some(hub.clone());
    let (sender, mut requests) = mpsc::channel::<Work>(32);
    let (stopped, mut stop) = mpsc::channel::<()>(1);
    let (elevation_sender, mut elevation_events) = mpsc::unbounded_channel::<ElevationEvent>();
    manager.elevation_events = Some(elevation_sender);
    let actor_hub = hub.clone();
    let actor = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        loop {
            tokio::select! {
                Some(event)=elevation_events.recv()=>{
                    apply_elevation(&mut manager,event).await;
                    actor_hub.publish(&manager);
                }
                work=requests.recv()=>{
                    let Some(work)=work else {break;};
                    let title=crate::ui::action_description(&work.action);
                    actor_hub.log.write("manager",&format!("started {title}"));
                    work.progress.send_replace(Some(progress::Progress::new("Running",title.clone())));
                    let result=progress::scope(work.progress,manager.action(work.action)).await;
                    let reply=reply(result);
                    actor_hub.log.write("manager",&format!("{} {title}",if reply.error.is_some(){"failed"}else{"completed"}));
                    actor_hub.publish(&manager);
                    let _=work.reply.send(reply);
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
        manager.stop_all().await;
    });
    let poll_hub = hub.clone();
    let polling = tokio::spawn(async move {
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
    let update_hub = hub.clone();
    let update_polling = tokio::spawn(async move {
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
                    let endpoint = astra_proxy::hosted::endpoint(&route.socket);
                    let result = timeout(
                        Duration::from_millis(750),
                        astra_proxy::hosted::request(
                            &endpoint,
                            astra_proxy::hosted::Command::Status,
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
    let followers = Arc::new(Semaphore::new(16));
    let mut clients = tokio::task::JoinSet::new();
    let signal = astra_ipc::shutdown_signal();
    tokio::pin!(signal);
    loop {
        tokio::select! {
            _=&mut signal=>break,
            _=stop.recv()=>break,
            result=listener.accept()=>{
                let (stream,_)=result?;
                if let Ok(permit)=permits.clone().try_acquire_owned(){let (hub,sender,stopped,diagnostics,followers)=(hub.clone(),sender.clone(),stopped.clone(),diagnostics.clone(),followers.clone());
                    clients.spawn(async move {let _permit=permit;let _=serve(stream,hub,sender,stopped,diagnostics,followers).await;});}
            }
            Some(_)=clients.join_next(),if !clients.is_empty()=>{},
        }
    }
    polling.abort();
    update_polling.abort();
    clients.abort_all();
    while clients.join_next().await.is_some() {}
    drop(sender);
    let _ = actor.await;
    Ok(())
}
fn reply(result: Result<Reply>) -> Reply {
    result.unwrap_or_else(|error| {
        let mut reply = Reply::text("");
        reply.error = Some(format!("{error:#}"));
        reply
    })
}
async fn send(
    stream: &mut astra_ipc::platform::LocalStream,
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
    stream: &mut astra_ipc::platform::LocalStream,
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
    mut stream: astra_ipc::platform::LocalStream,
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
                let endpoint = astra_proxy::hosted::endpoint(&route.socket);
                let enhanced = timeout(
                    Duration::from_millis(750),
                    astra_proxy::hosted::request(&endpoint, astra_proxy::hosted::Command::Status),
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
                            astra_proxy::hosted::request(
                                &endpoint,
                                astra_proxy::hosted::Command::Telemetry,
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
                    let snapshot: std::result::Result<astra_ipc::runtime::Snapshot, String> = tokio::select! {
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
                let path = if name == "@tui" {
                    hub.home.join("tui.log")
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
                let (mut input, mut output) = tokio::io::split(stream);
                let result = tokio::select! {result=plan.stream(targets,&mut output)=>result,_=input.read_u8()=>return Ok(())};
                if let Err(error) = result {
                    let _ = astra_ipc::send_management(&mut output, &reply(Err(error))).await;
                }
                return Ok(());
            }
            Action::NetTool { name, request } => {
                let _permit = diagnostics
                    .try_acquire()
                    .context("Diagnostics are busy; retry shortly")?;
                let plan = hub.plan(name)?;
                let request = request.clone();
                let (mut input, mut output) = tokio::io::split(stream);
                let result = tokio::select! {result=plan.nettool(request,&mut output)=>result,_=input.read_u8()=>return Ok(())};
                if let Err(error) = result {
                    let _ = astra_ipc::send_management(&mut output, &reply(Err(error))).await;
                }
                return Ok(());
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
                    let title = crate::ui::action_description(&request.action);
                    hub.log.write("manager", &format!("started {title}"));
                    let (progress, mut updates) = watch::channel(None);
                    let future = progress::scope(progress, async {
                        let _permit = diagnostics
                            .try_acquire()
                            .context("Diagnostics are busy; retry shortly")?;
                        let name = match &request.action {
                            Action::Diagnostics(n)
                            | Action::DiagnosticTargets(n)
                            | Action::UpdateSubscriptions(n)
                            | Action::DiagnosticsSelected { name: n, .. } => n,
                            _ => unreachable!(),
                        };
                        let plan = hub.plan(name)?;
                        progress::report(
                            if matches!(&request.action, Action::UpdateSubscriptions(_)) {
                                "Refresh subscriptions"
                            } else {
                                "Run diagnostics"
                            },
                            name.clone(),
                        );
                        match request.action {
                            Action::UpdateSubscriptions(_) => plan.run_refresh().await,
                            Action::DiagnosticTargets(_) => plan.run_selection(None, true).await,
                            Action::DiagnosticsSelected { targets, .. } => {
                                plan.run_selection(Some(targets), false).await
                            }
                            _ => plan.run().await,
                        }
                    });
                    tokio::pin!(future);
                    let mut open = session;
                    let result = loop {
                        tokio::select! {
                            result=&mut future=>break result,
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
                        crate::ui::action_description(&request.action),
                    );
                    if session {
                        send(&mut stream, &Event::<Reply>::Progress(queued)).await?;
                    }
                    sender
                        .send(Work {
                            action: request.action,
                            reply: response,
                            progress,
                        })
                        .await?;
                    loop {
                        tokio::select! {
                            result=&mut receiver=>break result?,
                            changed=updates.changed(),if session=>{
                                if changed.is_err(){break receiver.await?;}
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
async fn fast(hub: &Arc<Hub>, action: &Action) -> Option<Result<Reply>> {
    Some(match action {
        Action::List => Ok(hub.list.borrow().clone()),
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
            #[cfg(windows)]
            if matches!(command, astra_ipc::runtime::Command::Tun(true)) {
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
            if name == "@tui" {
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
                    &crate::subscription_view::read(&instance.config)?,
                )?))
            })
            .await
            .unwrap_or_else(|e| Err(e.into()))
        }
        _ => return None,
    })
}

async fn fast_progress(
    stream: &mut astra_ipc::platform::LocalStream,
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
mod update_status_tests {
    use super::*;
    use crate::protocol::updates::{UpdateIntent, UpdateRequest, UpdateStatus};

    #[tokio::test]
    async fn a_subscriber_close_before_the_next_push_is_a_normal_disconnect() {
        use tokio::io::AsyncReadExt;
        let (client, mut server) = astra_ipc::platform::stream_pair().await.unwrap();
        drop(client);
        // Observe the close first, then exercise a write that raced with it.
        assert!(timeout(Duration::from_millis(500), server.read_u8())
            .await
            .unwrap()
            .is_err());
        assert!(
            !send_subscription(&mut server, &Reply::text("next snapshot"))
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn read_and_watch_update_status_do_not_enter_the_mutation_queue() {
        let id = UpdateRequest::new(UpdateIntent::Stage {
            package: PathBuf::new(),
        })
        .operation_id;
        let root = std::env::temp_dir().join(format!("astra-update-watch-{id}"));
        let manager = Manager::load(root.join("state"), root.join("bundle")).unwrap();
        let hub = Hub::new(&manager).unwrap();
        // Keep the receiver alive but deliberately never service it, as during a long update.
        let (sender, mut queued) = mpsc::channel(1);
        let (stopped, _stop) = mpsc::channel(1);
        for action in [Action::UpdateStatus, Action::WatchUpdates] {
            let watching = matches!(action, Action::WatchUpdates);
            let (mut client, server) = astra_ipc::platform::stream_pair().await.unwrap();
            let task = tokio::spawn(serve(
                server,
                hub.clone(),
                sender.clone(),
                stopped.clone(),
                Arc::new(Semaphore::new(1)),
                Arc::new(Semaphore::new(1)),
            ));
            astra_ipc::send_management(
                &mut client,
                &Request {
                    version: astra_ipc::API_VERSION,
                    action,
                },
            )
            .await
            .unwrap();
            let first: Reply = timeout(
                Duration::from_millis(500),
                astra_ipc::receive_management(&mut client),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(
                serde_json::from_str::<UpdateStatus>(&first.text)
                    .unwrap()
                    .schema_version,
                1
            );
            if watching {
                let mut changed = manager.cached_update_status();
                changed.staged_release_id = Some("observed-during-work".into());
                hub.publish_updates(changed);
                timeout(Duration::from_millis(500), async {
                    loop {
                        let next: Reply = astra_ipc::receive_management(&mut client).await.unwrap();
                        let status: UpdateStatus = serde_json::from_str(&next.text).unwrap();
                        if status.staged_release_id.as_deref() == Some("observed-during-work") {
                            break;
                        }
                    }
                })
                .await
                .unwrap();
            }
            drop(client);
            timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        }
        assert!(queued.try_recv().is_err());
        assert!(
            !manager.home.join("updates").exists(),
            "reads and watches must not open/recover release-store journals"
        );
        drop(hub);
        drop(manager);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn gate_requires_observed_identity_and_idle_host() {
        let mut status = UpdateStatus::default();
        status.capabilities.experimental_enabled = true;
        status.capabilities.ordinary_user = true;
        status.instances.insert(
            "legacy".into(),
            crate::protocol::updates::UpdateInstance {
                reachable: true,
                status: Some(serde_json::json!({"active":1})),
                error: None,
                ..Default::default()
            },
        );
        apply_instance_gate(&mut status);
        assert!(!status.capabilities.activation_allowed);
        status.instances.get_mut("legacy").unwrap().status =
            Some(serde_json::json!({"boot_id":"one","paused":true}));
        apply_instance_gate(&mut status);
        assert!(!status.capabilities.activation_allowed);
        status.instances.get_mut("legacy").unwrap().status =
            Some(serde_json::json!({"boot_id":"one","paused":false,"busy":false}));
        apply_instance_gate(&mut status);
        assert!(status.capabilities.activation_allowed);
    }
}
