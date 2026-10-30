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
    generation: AtomicU64,
    log: Arc<crate::tui_log::Log>,
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
            generation: AtomicU64::new(0),
            log: Arc::new(crate::tui_log::Log::open(&manager.home.join("tui.log"))?),
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
    }
    fn reader(&self) -> Manager {
        let base = self.base.borrow();
        Manager {
            home: self.home.clone(),
            config_home: self.config.clone(),
            bin: self.bin.clone(),
            registry: base.registry.clone(),
            runtime: RuntimeState {
                log_levels: base.levels.clone(),
            },
            running: HashMap::new(),
            exits: HashMap::new(),
            publisher: None,
        }
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

pub(super) async fn run(mut manager: Manager, listener: UnixListener) -> Result<()> {
    let hub = Hub::new(&manager)?;
    manager.publisher = Some(hub.clone());
    let (sender, mut requests) = mpsc::channel::<Work>(32);
    let (stopped, mut stop) = mpsc::channel::<()>(1);
    let actor_hub = hub.clone();
    let actor = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        loop {
            tokio::select! {
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
                _=tick.tick()=>{let _=manager.reap();actor_hub.publish(&manager);}
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
            Action::WatchList => {
                let mut updates = hub.list.subscribe();
                let mut heartbeat = tokio::time::interval(Duration::from_secs(5));
                loop {
                    let snapshot = updates.borrow_and_update().clone();
                    send(&mut stream, &snapshot).await?;
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
        Action::StoragePaths => serde_json::to_string(&StoragePaths {
            config: hub.config.clone(),
            state: hub.home.clone(),
        })
        .map(Reply::text)
        .map_err(Into::into),
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
