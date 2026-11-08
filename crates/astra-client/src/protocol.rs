//! Adapter from page actions to typed management v2. No frontend opens the internal actor protocol.
use anyhow::{Context, Result};
pub use astra_management::{
    updates, Action, Instance, InstanceDetails, Kind, LogCursor, LogEvent, LogLevel, Reply, Row,
    StoragePaths, SubscriptionNodes,
};
use astra_management::{v2 as v, ManagerClient};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex, OnceLock,
    },
    time::Duration,
};
type Target = (PathBuf, Option<String>);
static TARGET: OnceLock<Mutex<Option<Target>>> = OnceLock::new();
pub fn configure(path: PathBuf, agent: Option<String>) {
    *TARGET.get_or_init(Default::default).lock().unwrap() = Some((path, agent));
}
pub fn target() -> Option<(PathBuf, Option<String>)> {
    TARGET.get_or_init(Default::default).lock().unwrap().clone()
}
pub fn session_home(home: &Path) -> Result<PathBuf> {
    if let Some((endpoint, agent)) = target() {
        use sha2::Digest;
        let identity = format!("{}\0{}", endpoint.display(), agent.unwrap_or_default());
        let key = format!("{:x}", sha2::Sha256::digest(identity.as_bytes()));
        let path = home.join("remote-sessions").join(key);
        astra_platform::storage::private_dir(&path)?;
        Ok(path)
    } else {
        Ok(home.into())
    }
}
fn id() -> String {
    static N: AtomicU64 = AtomicU64::new(1);
    format!(
        "tui-{}-{}-{}",
        std::process::id(),
        astra_ipc::progress::now_ms(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}
pub async fn request(socket: &Path, body: v::RequestBody) -> Result<v::ResponseBody> {
    let (endpoint, agent_id) = TARGET
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| {
            (
                if socket.file_name().is_some_and(|n| n == "manager.sock") {
                    socket.with_file_name("management.sock")
                } else {
                    socket.into()
                },
                None,
            )
        });
    let mut request = v::Request::new(id(), body);
    request.agent_id = agent_id;
    ManagerClient::new(endpoint)
        .request_v2(request)
        .await?
        .result
        .map_err(|e| anyhow::anyhow!("{:?}: {}", e.code, e.message))
}
async fn query(p: &Path, q: v::Query) -> Result<v::ResponseBody> {
    request(p, v::RequestBody::Query(q)).await
}
/// One header exit probe through the manager's probe core for `instance`.
pub async fn exit_probe(
    p: &Path,
    instance: String,
    outbound: String,
    ip: bool,
    keep_alive: Option<Duration>,
) -> Result<astra_model::diagnostics::Probe> {
    let keep_alive_secs = keep_alive.map(|d| d.as_secs().min(u64::from(u32::MAX)) as u32);
    match query(
        p,
        v::Query::ExitProbe {
            instance,
            outbound,
            ip,
            keep_alive_secs,
        },
    )
    .await?
    {
        v::ResponseBody::ExitProbe(value) => {
            Ok(serde_json::from_value(native_numbers(value, None))?)
        }
        other => anyhow::bail!("Expected exit probe, received {other:?}"),
    }
}
pub async fn execute(p: &Path, c: v::Command) -> Result<v::ResponseBody> {
    execute_id(p, id(), c).await
}
async fn execute_id(p: &Path, operation_id: String, c: v::Command) -> Result<v::ResponseBody> {
    let operation = request(
        p,
        v::RequestBody::Execute {
            operation_id: operation_id.clone(),
            command: c,
        },
    );
    tokio::pin!(operation);
    let mut timer = tokio::time::interval(Duration::from_millis(500));
    let mut cursor = None;
    loop {
        tokio::select! {result=&mut operation=>return result,_=timer.tick()=>{
         if let Ok(v::ResponseBody::Events(batch))=query(p,v::Query::Events{after:cursor.clone(),limit:Some(128)}).await{
          cursor=Some(batch.cursor);for event in batch.events{if event.operation_id.as_deref()==Some(&operation_id){astra_ipc::progress::report(event.kind,event.message);}}
         }
        }}
    }
}
fn kind(k: Kind) -> v::InstanceKind {
    match k {
        Kind::Proxy => v::InstanceKind::Proxy,
        Kind::Rule => v::InstanceKind::Rule,
        Kind::Server => v::InstanceKind::Server,
    }
}
fn runtime(c: astra_model::runtime::Command) -> v::RuntimeCommand {
    use astra_model::runtime::Command as C;
    use v::RuntimeCommand as R;
    match c {
        C::Snapshot => R::Snapshot,
        C::SetMode(x) => R::SetMode(x),
        C::Select { group, member } => R::Select { group, member },
        C::TestGroup(x) => R::TestGroup(x),
        C::Disconnect(x) => R::Disconnect(x.map(|x| x.to_string())),
        C::SystemProxy(x) => R::SystemProxy(x),
        C::Tun(x) => R::Tun(x),
        C::Reset => R::Reset,
        C::AllowLan(x) => R::AllowLan(x),
    }
}
// Translate actions before entering the async dispatcher: an await in every
// arm made the debug poll frame exceed 300 KiB and exhaust the Windows UI stack.
enum CallPlan {
    Request(v::RequestBody),
    Execute {
        operation_id: String,
        command: v::Command,
    },
    Save {
        name: String,
        original: String,
        text: String,
    },
    Rollback,
}
impl CallPlan {
    fn query(query: v::Query) -> Self {
        Self::Request(v::RequestBody::Query(query))
    }
    fn execute(command: v::Command) -> Self {
        Self::Execute {
            operation_id: id(),
            command,
        }
    }
}
fn call_plan(action: Action, local: bool) -> Result<CallPlan> {
    use v::{Command as C, Query as Q};
    Ok(match action {
        Action::MaintenanceRestore(_) => {
            anyhow::bail!("Maintenance restore is private to the local agent")
        }
        Action::List | Action::WatchList => CallPlan::query(Q::Instances),
        Action::Add(i) => CallPlan::execute(C::Add {
            name: i.name,
            kind: kind(i.kind),
            config: i.config,
        }),
        Action::Create { name, kind: k } => CallPlan::execute(C::Create {
            name,
            kind: kind(k),
        }),
        Action::Remove(instance) => CallPlan::execute(C::Remove { instance }),
        Action::Start(instance) => CallPlan::execute(C::Start { instance }),
        Action::StartSilent(instance) => CallPlan::execute(C::StartSilent { instance }),
        Action::ForceStart(instance) => CallPlan::execute(C::ForceStart { instance }),
        Action::Stop(instance) => CallPlan::execute(C::Stop { instance }),
        Action::Restart(instance) => CallPlan::execute(C::Restart { instance }),
        Action::Reload(instance) => CallPlan::execute(C::Reload { instance }),
        Action::SetConfig(instance, path) => CallPlan::execute(C::SetConfig { instance, path }),
        Action::Read(instance) => CallPlan::query(Q::Document { instance }),
        Action::Save {
            name,
            original,
            text,
        } => CallPlan::Save {
            name,
            original,
            text,
        },
        Action::CloneConfig(instance, destination) => CallPlan::execute(C::CloneConfig {
            instance,
            destination,
        }),
        Action::Validate(instance) => CallPlan::execute(C::Validate { instance }),
        Action::PrepareConfig(instance) => CallPlan::execute(C::PrepareConfig { instance }),
        Action::Logs(instance) => CallPlan::query(Q::Logs { instance }),
        Action::Shutdown => CallPlan::execute(C::Shutdown),
        Action::GetLogLevel(instance) => CallPlan::query(Q::LogLevel { instance }),
        Action::SetLogLevel {
            name: instance,
            level,
        } => CallPlan::execute(C::SetLogLevel { instance, level }),
        Action::BindStart {
            name: instance,
            targets,
        } => CallPlan::execute(C::BindStart { instance, targets }),
        Action::Diagnostics(instance) => CallPlan::execute(C::Diagnostics {
            instance,
            targets: None,
        }),
        Action::DiagnosticsSelected {
            name: instance,
            targets,
        }
        | Action::DiagnosticsStream {
            name: instance,
            targets,
        } => CallPlan::execute(C::Diagnostics {
            instance,
            targets: Some(targets),
        }),
        Action::NodeTags(instance) => CallPlan::query(Q::NodeTags { instance }),
        Action::SubscriptionNodes(instance) => CallPlan::query(Q::SubscriptionNodes { instance }),
        Action::DiagnosticTargets(instance) => CallPlan::query(Q::DiagnosticTargets { instance }),
        Action::ExplainRoute {
            name: instance,
            request: r,
        } => CallPlan::execute(C::ExplainRoute {
            instance,
            request: v::RouteQuery {
                host: r.target.host,
                port: r.target.port,
                network: r.network,
                inbound: r.inbound,
                destination_ip: r.destination_ip,
                process: r.process,
            },
        }),
        Action::UpdateSubscriptions(instance) => {
            CallPlan::execute(C::UpdateSubscriptions { instance })
        }
        Action::NetTool {
            name: instance,
            request,
        } => CallPlan::execute(C::NetTool {
            instance,
            request: request.into(),
        }),
        Action::StoragePaths => CallPlan::query(Q::StoragePaths),
        Action::Monitor { name: instance } => CallPlan::query(Q::Monitor { instance }),
        Action::Runtime {
            name: instance,
            command: astra_model::runtime::Command::Snapshot,
            ..
        }
        | Action::WatchRuntime { name: instance, .. } => CallPlan::query(Q::Runtime { instance }),
        Action::Runtime {
            name: instance,
            command: astra_model::runtime::Command::Tun(true),
            expected_pid,
        } if local => CallPlan::Request(v::RequestBody::Local(v::LocalCommand::EnableTun {
            instance,
            expected_pid,
        })),
        Action::Runtime {
            name: instance,
            command,
            expected_pid,
        } => CallPlan::execute(C::Runtime {
            instance,
            command: runtime(command),
            expected_pid,
        }),
        Action::UpdateStatus | Action::WatchUpdates => CallPlan::query(Q::UpdateStatus),
        Action::UpdateOperationStatus { operation_id } => {
            CallPlan::query(Q::UpdateOperationStatus { operation_id })
        }
        Action::UpdateTransaction { request: r } => CallPlan::Execute {
            operation_id: r.operation_id.clone(),
            command: C::Update { request: r },
        },
        Action::UpdatePackage(package) => CallPlan::execute(C::Update {
            request: updates::UpdateRequest::new(updates::UpdateIntent::Stage { package }),
        }),
        Action::Rollback => CallPlan::Rollback,
        Action::RecoverUpdate { operation_id } => {
            CallPlan::execute(C::RecoverUpdate { operation_id })
        }
        Action::PrepareElevation {
            name: instance,
            expected_pid,
        } => CallPlan::Request(v::RequestBody::Local(v::LocalCommand::PrepareElevation {
            instance,
            expected_pid,
        })),
        Action::WaitElevation {
            name: instance,
            ticket,
        } => CallPlan::Request(v::RequestBody::Local(v::LocalCommand::WaitElevation {
            instance,
            ticket,
        })),
        Action::CancelElevation {
            name: instance,
            ticket,
        } => CallPlan::Request(v::RequestBody::Local(v::LocalCommand::CancelElevation {
            instance,
            ticket,
        })),
        Action::FollowLogs { .. } | Action::Session => {
            anyhow::bail!("Subscription action requires a v2 stream adapter")
        }
    })
}
pub async fn call(p: &Path, action: Action) -> Result<Reply> {
    // TUI rows retain names, while v2 addresses and replies use durable IDs.
    let monitor_name = match &action {
        Action::Monitor { name } => Some(name.clone()),
        _ => None,
    };
    let local = TARGET
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .as_ref()
        .is_none_or(|(_, agent)| agent.is_none());
    let result = match call_plan(action, local)? {
        CallPlan::Request(body) => request(p, body).await?,
        CallPlan::Execute {
            operation_id,
            command,
        } => execute_id(p, operation_id, command).await?,
        CallPlan::Save {
            name,
            original,
            text,
        } => save(p, name, original, text).await?,
        CallPlan::Rollback => rollback(p).await?,
    };
    reply_for_action(result, monitor_name.as_deref())
}
fn reply_for_action(mut result: v::ResponseBody, monitor_name: Option<&str>) -> Result<Reply> {
    if let (v::ResponseBody::Monitor(snapshot), Some(name)) = (&mut result, monitor_name) {
        snapshot.instance = name.into();
    }
    reply(result)
}
async fn save(p: &Path, name: String, original: String, text: String) -> Result<v::ResponseBody> {
    let draft_id = id();
    let v::ResponseBody::Draft(d) = request(
        p,
        v::RequestBody::Drafts(v::DraftCommand::Open {
            instance: name,
            draft_id: draft_id.clone(),
        }),
    )
    .await?
    else {
        anyhow::bail!("Expected draft")
    };
    anyhow::ensure!(
        d.original == original,
        "File changed on disk; reopen before editing"
    );
    let v::ResponseBody::Draft(d) = request(
        p,
        v::RequestBody::Drafts(v::DraftCommand::Edit {
            draft_id: draft_id.clone(),
            expected_revision: d.revision,
            text,
        }),
    )
    .await?
    else {
        anyhow::bail!("Expected draft")
    };
    request(
        p,
        v::RequestBody::Drafts(v::DraftCommand::Save {
            draft_id,
            expected_revision: d.revision,
            expected_source_revision: d.source_revision,
            operation_id: id(),
        }),
    )
    .await
}
async fn rollback(p: &Path) -> Result<v::ResponseBody> {
    use v::{Command as C, Query as Q};
    let v::ResponseBody::UpdateStatus(status) = query(p, Q::UpdateStatus).await? else {
        anyhow::bail!("Expected update status")
    };
    let target_directory = status
        .capabilities
        .rollback_target
        .context("No rollback target")?;
    execute(
        p,
        C::Update {
            request: updates::UpdateRequest::new(updates::UpdateIntent::Rollback {
                target_directory,
            }),
        },
    )
    .await
}

fn reply(r: v::ResponseBody) -> Result<Reply> {
    use v::ResponseBody as R;
    Ok(match r {
        R::Instances(items) => {
            let mut details = std::collections::HashMap::new();
            let rows = items
                .into_iter()
                .map(|i| {
                    details.insert(i.name.clone(), i.details.unwrap_or_default());
                    astra_management::Row {
                        instance: Instance {
                            name: i.name,
                            kind: match i.kind {
                                v::InstanceKind::Proxy => Kind::Proxy,
                                v::InstanceKind::Rule => Kind::Rule,
                                v::InstanceKind::Server => Kind::Server,
                            },
                            config: i.config,
                        },
                        pid: i.pid,
                        state: i.state,
                        uptime: i.uptime_secs.parse().unwrap_or(0),
                        changed: i.changed,
                    }
                })
                .collect();
            Reply {
                version: astra_ipc::API_VERSION,
                error: None,
                rows,
                text: serde_json::to_string(&details)?,
            }
        }
        R::Local(x) => Reply::text(if let serde_json::Value::String(s) = x {
            s
        } else {
            serde_json::to_string(&x)?
        }),
        R::Document(d) => Reply::text(d.text),
        R::Draft(_) => Reply::text("Validated and saved; configuration applied where supported"),
        R::Logs { text } | R::Done { message: text } => Reply::text(text),
        R::Runtime(v::RuntimeSnapshot::Client(x))
        | R::Diagnostics(x)
        | R::DiagnosticTargets(x)
        | R::RouteExplanation(x) => Reply::text(serde_json::to_string(&native_numbers(x, None))?),
        R::Runtime(v::RuntimeSnapshot::Server(x)) => Reply::text(serde_json::to_string(&x)?),
        R::NodeTags(x) => Reply::text(serde_json::to_string(&x)?),
        R::SubscriptionNodes(x) => Reply::text(serde_json::to_string(&x)?),
        R::StoragePaths(x) => Reply::text(serde_json::to_string(&x)?),
        R::Monitor(x) => Reply::text(serde_json::to_string(&x)?),
        R::LogLevel(x) => Reply::text(x.as_str()),
        R::UpdateStatus(x) => Reply::text(serde_json::to_string(&x)?),
        R::Task(t) => {
            anyhow::ensure!(matches!(t.state, v::TaskState::Complete), "{}", t.message);
            Reply::text(t.message)
        }
        other => Reply::text(serde_json::to_string(&other)?),
    })
}
fn native_numbers(value: serde_json::Value, key: Option<&str>) -> serde_json::Value {
    match value {
        serde_json::Value::String(s)
            if key.is_some_and(|k| {
                matches!(
                    k,
                    "pid"
                        | "id"
                        | "port"
                        | "upload"
                        | "download"
                        | "proxy_upload"
                        | "proxy_download"
                        | "active"
                        | "total"
                        | "elapsed_ms"
                        | "delay_ms"
                        | "connect_ms"
                        | "sample_sequence"
                        | "sample_elapsed_ms"
                        | "sample_age_ms"
                        | "generation"
                        | "active_generation"
                        | "revision"
                        | "api_version"
                        | "index"
                        | "seq"
                        | "sent"
                        | "received"
                )
            }) =>
        {
            s.parse::<u64>()
                .map(|n| serde_json::Value::Number(n.into()))
                .unwrap_or(serde_json::Value::String(s))
        }
        serde_json::Value::Array(a) => a.into_iter().map(|v| native_numbers(v, key)).collect(),
        serde_json::Value::Object(o) => o
            .into_iter()
            .map(|(k, v)| {
                let v = native_numbers(
                    v,
                    Some(if key == Some("connection_generations") {
                        "generation"
                    } else {
                        &k
                    }),
                );
                (k, v)
            })
            .collect(),
        other => other,
    }
}
pub async fn subscribe(
    socket: &Path,
    action: Action,
) -> Result<astra_platform::platform::LocalStream> {
    let (client, mut writer) = astra_platform::platform::stream_pair().await?;
    let p = socket.to_owned();
    let log_stream = matches!(&action, Action::FollowLogs { .. });
    tokio::spawn(async move {
        let result: Result<()> = async {
            if let Action::FollowLogs { name, mut cursor } = action {
                astra_ipc::send_management(
                    &mut writer,
                    &Reply::text("Streaming logs over management v2"),
                )
                .await?;
                loop {
                    let v::ResponseBody::LogChunk(c) = query(
                        &p,
                        v::Query::LogChunk {
                            instance: name.clone(),
                            cursor: cursor.map(|c| v::LogCursor {
                                device: c.device.to_string(),
                                inode: c.inode.to_string(),
                                offset: c.offset.to_string(),
                            }),
                        },
                    )
                    .await?
                    else {
                        anyhow::bail!("Expected log chunk")
                    };
                    let event = match c {
                        v::LogChunk::Data {
                            bytes,
                            cursor: c,
                            reset,
                            skip_prefix,
                        } => {
                            let c = LogCursor {
                                device: c.device.parse()?,
                                inode: c.inode.parse()?,
                                offset: c.offset.parse()?,
                            };
                            cursor = Some(c);
                            LogEvent::Data {
                                bytes,
                                cursor: c,
                                reset,
                                skip_prefix,
                            }
                        }
                        v::LogChunk::Waiting => LogEvent::Waiting,
                        v::LogChunk::Error(e) => LogEvent::Error(e),
                    };
                    tokio::time::timeout(
                        Duration::from_secs(3),
                        astra_ipc::send_management(&mut writer, &event),
                    )
                    .await??;
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
            if let Action::NetTool {
                name: instance,
                request: r,
            } = action.clone()
            {
                return stream_job(
                    &p,
                    v::Command::NetTool {
                        instance,
                        request: r.into(),
                    },
                    "nettool_event",
                    &mut writer,
                )
                .await;
            }
            if let Action::DiagnosticsStream {
                name: instance,
                targets,
            } = action.clone()
            {
                return stream_job(
                    &p,
                    v::Command::Diagnostics {
                        instance,
                        targets: Some(targets),
                    },
                    "diagnostics_event",
                    &mut writer,
                )
                .await;
            }
            loop {
                let r = call(&p, action.clone()).await?;
                tokio::time::timeout(
                    Duration::from_secs(3),
                    astra_ipc::send_management(&mut writer, &r),
                )
                .await??;
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
        .await;
        if let Err(e) = result {
            if log_stream {
                let _ = astra_ipc::send_management(&mut writer, &LogEvent::Error(format!("{e:#}")))
                    .await;
            } else {
                let mut r = Reply::text("");
                r.error = Some(format!("{e:#}"));
                let _ = astra_ipc::send_management(&mut writer, &r).await;
            }
        }
    });
    Ok(client)
}
async fn stream_job(
    path: &Path,
    command: v::Command,
    event_kind: &str,
    writer: &mut astra_platform::platform::LocalStream,
) -> Result<()> {
    let operation_id = id();
    let operation = request(
        path,
        v::RequestBody::Execute {
            operation_id: operation_id.clone(),
            command,
        },
    );
    tokio::pin!(operation);
    let mut timer = tokio::time::interval(Duration::from_millis(100));
    let mut cursor = None;
    let mut seen = std::collections::HashSet::new();
    let result = loop {
        tokio::select! {result=&mut operation=>break result?,_=timer.tick()=>{
         if let Ok(v::ResponseBody::Events(batch))=query(path,v::Query::Events{after:cursor.clone(),limit:Some(256)}).await{cursor=Some(batch.cursor);
          for event in batch.events{if event.operation_id.as_deref()==Some(&operation_id)&&event.kind==event_kind{let value=match event.payload {Some(v::EventPayload::Diagnostics(value)|v::EventPayload::NetTool(value))=>value,None=>continue};let text=serde_json::to_string(&native_numbers(value,None))?;if seen.insert(text.clone()){tokio::time::timeout(Duration::from_secs(3),astra_ipc::send_management(writer,&Reply::text(text))).await??;}}}
         }
        }}
    };
    let events = match result {
        v::ResponseBody::NetTool(events) => events
            .into_iter()
            .map(|v| native_numbers(v, None))
            .collect::<Vec<_>>(),
        v::ResponseBody::Diagnostics(value) => {
            use astra_model::diagnostics::{Event, Report};
            let report: Report = serde_json::from_value(native_numbers(value, None))?;
            let mut events = vec![Event::Started {
                targets: report.probes.iter().map(|p| p.outbound.clone()).collect(),
                url: report.url,
                running_config: report.running_config,
            }];
            for probe in report.probes {
                if let Some(connect_ms) = probe.connect_ms {
                    events.push(Event::Connected {
                        outbound: probe.outbound.clone(),
                        connect_ms,
                    });
                }
                events.push(Event::Finished { probe });
            }
            events.push(Event::Done);
            events
                .into_iter()
                .map(serde_json::to_value)
                .collect::<std::result::Result<Vec<_>, _>>()?
        }
        other => anyhow::bail!("Expected stream result, received {other:?}"),
    };
    for event in events {
        let text = serde_json::to_string(&event)?;
        if seen.insert(text.clone()) {
            tokio::time::timeout(
                Duration::from_secs(3),
                astra_ipc::send_management(writer, &Reply::text(text)),
            )
            .await??;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "test/protocol_test.rs"]
mod tests;
