use crate::manager::validate_name;
use crate::{
    protocol as old,
    store::{self},
};
use anyhow::{Context, Result};
use astra_management::{config::ManagerConfig, v2::*};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};
use tokio::{io::AsyncWriteExt, sync::Mutex as AsyncMutex};
#[cfg(test)]
#[path = "test/agent_test.rs"]
mod tests;

fn program_info() -> ProgramInfo {
    use astra_platform::program_info::FeatureSupport as Feature;
    ProgramInfo::collect(
        astra_platform::build_info!(),
        vec![
            Feature::new(
                "instances",
                "Instance management",
                true,
                "Manage proxy, rule and server instances; core binaries are installed separately.",
            ),
            Feature::new(
                "documents",
                "Configuration editor",
                true,
                "Typed configuration schemas, drafts and validation.",
            ),
            Feature::new(
                "monitor",
                "Monitoring",
                true,
                "Process metrics, connections and logs; live data requires a running core.",
            ),
            Feature::new(
                "diagnostics",
                "Network diagnostics",
                true,
                "Core-provided diagnostics and routing explanations.",
            ),
            Feature::new(
                "subscriptions",
                "Subscriptions",
                true,
                "Subscription and rule data management.",
            ),
            Feature::new(
                "updates",
                "Update management",
                true,
                "Update planning and status; execution depends on update policy and installation.",
            ),
            Feature::new(
                "remote_management",
                "Remote management",
                true,
                "Controller/agent transport; requires configured credentials and connectivity.",
            ),
        ],
    )
}

pub struct AgentService {
    pub config: ManagerConfig,
    pub identity: Agent,
    store: crate::database::Database,
    mutation: AsyncMutex<()>,
    capacity: tokio::sync::Semaphore,
    native: std::sync::OnceLock<crate::manager::NativeClient>,
    uncertain: std::sync::Mutex<std::collections::HashMap<String, Task>>,
}
// A started server remains a transaction-owned candidate until readiness is
// committed. Cancellation must not leave it alive while this manager survives.
struct ServerCandidate(Option<std::process::Child>);
impl std::ops::Deref for ServerCandidate {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        self.0.as_ref().unwrap()
    }
}
impl std::ops::DerefMut for ServerCandidate {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.as_mut().unwrap()
    }
}
impl Drop for ServerCandidate {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

impl AgentService {
    pub(crate) async fn database_closed(&self) -> anyhow::Error {
        self.store.closed().await
    }
    pub fn new(config: ManagerConfig) -> Result<Arc<Self>> {
        crate::private_dir(&config.state_dir)?;
        crate::private_dir(&config.config_dir)?;
        let identity = Agent {
            id: config.agent.id.clone(),
            name: config.agent.name.clone(),
            boot_id: uuid::Uuid::new_v4().to_string(),
            online: true,
            last_seen_ms: store::now(),
        };
        let db = config.state_dir.join("management.sqlite");
        let marker = config.state_dir.join("management-state-v2");
        if marker.exists() {
            anyhow::ensure!(
                astra_platform::storage::read_bounded(&marker, 64)? == b"2\n",
                "Unsupported management state marker; retain state for recovery"
            );
            anyhow::ensure!(db.is_file(),"Management identity database is missing; restore management.sqlite and its WAL before restarting. Stable instance identities are never silently rebuilt.");
        }
        let store = crate::database::Database::open(db.clone(), config.limits.event_buffer)?;
        astra_platform::platform::set_private(&db, 0o600)?;
        if !marker.exists() {
            crate::atomic_write(&marker, b"2\n")?;
        }
        let capacity = tokio::sync::Semaphore::new(config.limits.concurrent_tasks.max(1));
        Ok(Arc::new(Self {
            config,
            identity,
            store,
            mutation: AsyncMutex::new(()),
            capacity,
            native: std::sync::OnceLock::new(),
            uncertain: Default::default(),
        }))
    }
    pub async fn start_native(&self) -> Result<tokio::task::JoinHandle<Result<()>>> {
        let legacy = self.config.state_dir.join("manager.sock");
        if legacy.exists() {
            astra_platform::platform::recover_stale_manager_endpoint(&legacy).context("An older manager endpoint still exists; stop its owner before starting management v2")?;
        }
        anyhow::ensure!(!legacy.exists(),"An older manager endpoint still exists; stop its owner and inspect retained cores before starting management v2");
        anyhow::ensure!(self.native.get().is_none(), "Native actor already started");
        let (client, task) = crate::manager::start_native(
            self.config.state_dir.clone(),
            self.config.config_dir.clone(),
            self.config.bin_dir.clone(),
            self.config.runtime_dir.clone(),
        )
        .await?;
        self.native
            .set(client)
            .map_err(|_| anyhow::anyhow!("Native actor already started"))?;
        Ok(task)
    }
    async fn call(&self, action: old::Action) -> Result<old::Reply> {
        self.native
            .get()
            .context("Native actor not initialized")?
            .call(action)
            .await
    }
    async fn servers(&self) -> Result<Vec<(String, PathBuf)>> {
        self.store.servers().await
    }
    async fn server_path(&self, name: &str) -> Result<Option<PathBuf>> {
        Ok(self
            .servers()
            .await?
            .into_iter()
            .find(|(n, _)| n == name)
            .map(|(_, p)| p))
    }
    fn server_endpoint(&self, name: &str) -> PathBuf {
        self.config.state_dir.join(format!("server-{name}.sock"))
    }
    fn server_binary(&self) -> PathBuf {
        self.config
            .bin_dir
            .join(astra_platform::platform::executable("astra-server-core"))
    }
    pub async fn handle(&self, request: Request) -> Response {
        let id = request.id.clone();
        if request.version != VERSION {
            return Response::error(id, ErrorCode::VersionMismatch, "Management v2 is required");
        }
        if request
            .agent_id
            .as_ref()
            .is_some_and(|v| v != &self.identity.id)
        {
            return Response::error(id, ErrorCode::NotFound, "Request targets another agent");
        }
        let result = match request.body {
            RequestBody::Local(command) => self.local(command).await,
            RequestBody::Query(query) => self.query(query).await,
            RequestBody::Execute {
                operation_id,
                command,
            } => {
                if matches!(command, Command::CancelTask { .. }) {
                    return match self.execute(operation_id, command).await {
                        Ok(value) => Response::success(id, value),
                        Err(error) => {
                            Response::error(id, ErrorCode::InvalidRequest, error.to_string())
                        }
                    };
                }
                let _capacity = self
                    .capacity
                    .acquire()
                    .await
                    .expect("agent capacity never closed");
                let _serial = self.mutation.lock().await;
                self.execute(operation_id, command).await
            }
            RequestBody::Drafts(command) => {
                let _serial = if matches!(
                    command,
                    DraftCommand::Read { .. } | DraftCommand::Schema { .. }
                ) {
                    None
                } else {
                    Some(self.mutation.lock().await)
                };
                self.draft(command).await
            }
            RequestBody::Artifacts(command) => {
                let _serial = self.mutation.lock().await;
                let root = self.config.state_dir.join("artifacts");
                let limit = self.config.limits.upload_bytes;
                match tokio::task::spawn_blocking(move || {
                    crate::artifacts::handle(&root, limit, command)
                })
                .await
                {
                    Ok(result) => result,
                    Err(error) => Err(error.into()),
                }
            }
        };
        match result {
            Ok(value) => Response::success(id, value),
            Err(error) => {
                let message = format!("{error:#}");
                let code = if message.starts_with("Conflict:") {
                    ErrorCode::Conflict
                } else if message.starts_with("RequiresLocalAction:") {
                    ErrorCode::RequiresLocalAction
                } else if message.starts_with("Unsupported:") {
                    ErrorCode::Unsupported
                } else if message.contains("outcome unknown") || message.contains("not replayed") {
                    ErrorCode::OutcomeUnknown
                } else if message.contains("not found") {
                    ErrorCode::NotFound
                } else {
                    ErrorCode::Internal
                };
                let details = error
                    .downcast_ref::<crate::save::SaveFailure>()
                    .map(|failure| failure.outcome.clone());
                let mut response = Response::error(id, code, message);
                if let Err(error) = &mut response.result {
                    error.details = details;
                }
                response
            }
        }
    }
    pub async fn handle_remote(&self, request: Request) -> Response {
        if matches!(request.body, RequestBody::Local(_)) {
            return Response::error(
                request.id,
                ErrorCode::RequiresLocalAction,
                "Interactive elevation is available only through the target host's private IPC",
            );
        }
        self.handle(request).await
    }
    async fn local(&self, command: LocalCommand) -> Result<ResponseBody> {
        let action = match command {
            LocalCommand::PrepareInstallation {
                package_id,
                expected_release_id,
                scope,
            } => {
                let config = self.config.clone();
                return Ok(ResponseBody::InstallationLaunch(
                    tokio::task::spawn_blocking(move || {
                        crate::install::prepare(&config, &package_id, &expected_release_id, scope)
                    })
                    .await??,
                ));
            }
            LocalCommand::PrepareElevation {
                instance,
                expected_pid,
            } => old::Action::PrepareElevation {
                name: self.store.resolve(&instance).await?,
                expected_pid,
            },
            LocalCommand::WaitElevation { instance, ticket } => old::Action::WaitElevation {
                name: self.store.resolve(&instance).await?,
                ticket,
            },
            LocalCommand::CancelElevation { instance, ticket } => old::Action::CancelElevation {
                name: self.store.resolve(&instance).await?,
                ticket,
            },
            LocalCommand::EnableTun {
                instance,
                expected_pid,
            } => old::Action::Runtime {
                name: self.store.resolve(&instance).await?,
                command: astra_model::runtime::Command::Tun(true),
                expected_pid,
            },
        };
        let reply = self.call(action).await?;
        Ok(ResponseBody::Local(
            serde_json::from_str(&reply.text).unwrap_or(serde_json::Value::String(reply.text)),
        ))
    }
    async fn query(&self, mut query: Query) -> Result<ResponseBody> {
        match &mut query {
            Query::Runtime { instance }
            | Query::Monitor { instance }
            | Query::Document { instance }
            | Query::Capabilities { instance }
            | Query::NodeTags { instance }
            | Query::SubscriptionNodes { instance }
            | Query::DiagnosticTargets { instance }
            | Query::LogLevel { instance }
            | Query::Logs { instance }
            | Query::LogChunk { instance, .. } => {
                *instance = self.store.resolve(instance).await?;
            }
            _ => {}
        }
        Ok(match query {
            Query::Hello => ResponseBody::Hello(Hello {
                version: VERSION,
                role: "agent".into(),
                epoch: self.identity.boot_id.clone(),
                build_id: env!("CARGO_PKG_VERSION").into(),
                capabilities: vec![
                    "instances".into(),
                    "client".into(),
                    "server".into(),
                    "documents".into(),
                    "artifacts".into(),
                    "tasks".into(),
                    "monitor".into(),
                    "program_info".into(),
                    "installation".into(),
                ],
            }),
            Query::ProgramInfo => ResponseBody::ProgramInfo(program_info()),
            Query::InstallationStatus => {
                let config = self.config.clone();
                let target = format!("{} ({})", self.identity.name, self.identity.id);
                ResponseBody::InstallationStatus(
                    tokio::task::spawn_blocking(move || crate::install::status(&config, target))
                        .await??,
                )
            }
            Query::Agents => ResponseBody::Agents(vec![Agent {
                last_seen_ms: store::now(),
                ..self.identity.clone()
            }]),
            Query::Instances => ResponseBody::Instances(self.instances().await?),
            Query::Monitor { instance } => {
                let mut sample: MonitorSnapshot = if self.server_path(&instance).await?.is_some() {
                    crate::monitor::server(&instance, &self.server_endpoint(&instance)).await?
                } else {
                    serde_json::from_str(
                        &self
                            .call(old::Action::Monitor {
                                name: instance.clone(),
                            })
                            .await?
                            .text,
                    )?
                };
                // Legacy management addresses instances by name. The v2 DTO
                // must use the same durable identity returned by Instances.
                sample.instance = self.store.identity(&instance).await?;
                ResponseBody::Monitor(sample)
            }
            Query::Document { instance } => ResponseBody::Document(self.document(&instance).await?),
            Query::Schema { kind, path } => {
                let kind = model_kind(kind);
                let document =
                    astra_app::Document::parse(kind, &astra_app::Document::template(kind))?;
                let path = path.unwrap_or_default();
                ResponseBody::Schema(
                    document.schema(&path, astra_model::document::Platform::current())?,
                )
            }
            Query::Capabilities { instance } => ResponseBody::Capabilities(
                self.instances()
                    .await?
                    .into_iter()
                    .find(|i| i.name == instance || i.id == instance)
                    .context("instance not found")?
                    .capabilities,
            ),
            Query::NodeTags { instance } => ResponseBody::NodeTags(serde_json::from_str(
                &self.call(old::Action::NodeTags(instance)).await?.text,
            )?),
            Query::SubscriptionNodes { instance } => {
                ResponseBody::SubscriptionNodes(serde_json::from_str(
                    &self
                        .call(old::Action::SubscriptionNodes(instance))
                        .await?
                        .text,
                )?)
            }
            Query::DiagnosticTargets { instance } => {
                ResponseBody::DiagnosticTargets(browser_numbers(serde_json::from_str(
                    &self
                        .call(old::Action::DiagnosticTargets(instance))
                        .await?
                        .text,
                )?))
            }
            Query::LogLevel { instance } => {
                anyhow::ensure!(
                    self.server_path(&instance).await?.is_none(),
                    "Unsupported: server cores do not expose a configurable log capture level"
                );
                ResponseBody::LogLevel(
                    self.call(old::Action::GetLogLevel(instance))
                        .await?
                        .text
                        .parse()?,
                )
            }
            Query::StoragePaths => ResponseBody::StoragePaths(old::StoragePaths {
                config: self.config.config_dir.clone(),
                state: self.config.state_dir.clone(),
            }),
            Query::Runtime { instance } => {
                if self.server_path(&instance).await?.is_some() {
                    let r = astra_management::server::call(
                        &self.server_endpoint(&instance),
                        astra_management::server::Action::Status,
                    )
                    .await?;
                    ResponseBody::Runtime(RuntimeSnapshot::Server(ServerSnapshot {
                        pid: r.pid,
                        uptime_secs: r.uptime_secs.to_string(),
                        active: r.active.to_string(),
                        accepted: r.accepted.to_string(),
                        rejected: r.rejected.to_string(),
                        upload: r.upload.to_string(),
                        download: r.download.to_string(),
                        listeners: r.listeners,
                    }))
                } else {
                    let row = self
                        .call(old::Action::List)
                        .await?
                        .rows
                        .into_iter()
                        .find(|r| r.instance.name == instance)
                        .context("instance not found")?;
                    anyhow::ensure!(
                        row.instance.kind == old::Kind::Proxy,
                        "Unsupported: rule instances do not expose a proxy runtime snapshot"
                    );
                    ResponseBody::Runtime(RuntimeSnapshot::Client(browser_numbers(
                        self.native
                            .get()
                            .context("Native actor not initialized")?
                            .runtime_sample(&instance)
                            .await?,
                    )))
                }
            }
            Query::Logs { instance } => {
                let text = if self.server_path(&instance).await?.is_some() {
                    astra_management::server::call(
                        &self.server_endpoint(&instance),
                        astra_management::server::Action::Logs,
                    )
                    .await?
                    .logs
                } else {
                    self.call(old::Action::Logs(instance)).await?.text
                };
                ResponseBody::Logs { text }
            }
            Query::LogChunk { instance, cursor } => {
                let cursor = cursor
                    .map(|c| {
                        Ok::<_, anyhow::Error>(old::LogCursor {
                            device: c.device.parse()?,
                            inode: c.inode.parse()?,
                            offset: c.offset.parse()?,
                        })
                    })
                    .transpose()?;
                let mut stream = if self.server_path(&instance).await?.is_some() {
                    let (reader, writer) = astra_platform::platform::stream_pair().await?;
                    tokio::spawn(crate::log_stream::serve(
                        writer,
                        self.config.state_dir.join(format!("server-{instance}.log")),
                        cursor,
                    ));
                    reader
                } else {
                    anyhow::ensure!(
                        instance == "@tui"
                            || self
                                .call(old::Action::List)
                                .await?
                                .rows
                                .iter()
                                .any(|r| r.instance.name == instance),
                        "instance not found"
                    );
                    let (reader, writer) = astra_platform::platform::stream_pair().await?;
                    tokio::spawn(crate::log_stream::serve(
                        writer,
                        self.config.state_dir.join(if instance == "@tui" {
                            "tui.log".into()
                        } else {
                            format!("{instance}.log")
                        }),
                        cursor,
                    ));
                    reader
                };
                let event: old::LogEvent = match tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    astra_ipc::receive_management(&mut stream),
                )
                .await
                {
                    Ok(result) => result?,
                    Err(_) => old::LogEvent::Waiting,
                };
                ResponseBody::LogChunk(match event {
                    old::LogEvent::Data {
                        bytes,
                        cursor,
                        reset,
                        skip_prefix,
                    } => LogChunk::Data {
                        bytes,
                        cursor: LogCursor {
                            device: cursor.device.to_string(),
                            inode: cursor.inode.to_string(),
                            offset: cursor.offset.to_string(),
                        },
                        reset,
                        skip_prefix,
                    },
                    old::LogEvent::Waiting => LogChunk::Waiting,
                    old::LogEvent::Error(e) => LogChunk::Error(e),
                })
            }
            Query::Tasks { limit } => {
                let mut tasks = self.store.tasks(limit.unwrap_or(32)).await?;
                for task in self.uncertain.lock().unwrap().values() {
                    tasks.retain(|existing| existing.id != task.id);
                    tasks.push(task.clone());
                }
                ResponseBody::Tasks(tasks)
            }
            Query::UpdateStatus => ResponseBody::UpdateStatus(serde_json::from_str(
                &self.call(old::Action::UpdateStatus).await?.text,
            )?),
            Query::UpdateOperationStatus { operation_id } => {
                ResponseBody::UpdateStatus(serde_json::from_str(
                    &self
                        .call(old::Action::UpdateOperationStatus { operation_id })
                        .await?
                        .text,
                )?)
            }
            Query::Task { operation_id } => {
                let uncertain = self.uncertain.lock().unwrap().get(&operation_id).cloned();
                ResponseBody::Task(match uncertain {
                    Some(task) => task,
                    None => self.store.task(&operation_id).await?,
                })
            }
            Query::Events { after, limit } => ResponseBody::Events(
                self.store
                    .events(after.as_deref(), limit.unwrap_or(128))
                    .await?,
            ),
        })
    }
    async fn instances(&self) -> Result<Vec<Instance>> {
        let reply = self.call(old::Action::List).await?;
        let details: std::collections::HashMap<String, old::InstanceDetails> =
            serde_json::from_str(&reply.text).unwrap_or_default();
        let mut rows: Vec<Instance> = reply
            .rows
            .into_iter()
            .map(|r| {
                let mut capabilities = vec![
                    "document".into(),
                    "monitor".into(),
                    "start".into(),
                    "stop".into(),
                    "reload".into(),
                    "logs".into(),
                    "restart".into(),
                    "validate".into(),
                    "remove".into(),
                    "set_config".into(),
                    "clone_config".into(),
                    "log_level".into(),
                    "update".into(),
                ];
                if let Some(d) = details.get(&r.instance.name) {
                    if d.runtime_supported && r.instance.kind == old::Kind::Proxy {
                        capabilities.push("runtime".into());
                    }
                    if d.diagnostics_supported && r.instance.kind == old::Kind::Proxy {
                        capabilities.push("diagnostics".into());
                    }
                    if r.instance.kind == old::Kind::Proxy {
                        capabilities.extend([
                            "subscriptions".into(),
                            "bind_start".into(),
                            "nettool".into(),
                        ]);
                    }
                    if r.instance.kind == old::Kind::Rule && d.explanation_supported {
                        capabilities.push("explain_route".into());
                    }
                }
                Instance {
                    details: details.get(&r.instance.name).cloned(),
                    changed: r.changed,
                    id: r.instance.name.clone(),
                    name: r.instance.name,
                    kind: if r.instance.kind == old::Kind::Proxy {
                        InstanceKind::Proxy
                    } else {
                        InstanceKind::Rule
                    },
                    config: r.instance.config,
                    pid: r.pid,
                    state: r.state,
                    uptime_secs: r.uptime.to_string(),
                    capabilities,
                }
            })
            .collect();
        for (name, config) in self.servers().await? {
            let status = astra_management::server::call(
                &self.server_endpoint(&name),
                astra_management::server::Action::Status,
            )
            .await
            .ok();
            let state = if status.is_some() {
                "Running"
            } else if self.server_endpoint(&name).exists() {
                "Unreachable"
            } else {
                "Stopped"
            };
            let config_path = config.clone();
            let auto_start = tokio::task::spawn_blocking(move || {
                crate::manager::read_config_auto_start(&config_path)
            })
            .await
            .unwrap_or_else(|error| Err(error.into()));
            rows.push(Instance {
                details: Some(old::InstanceDetails {
                    config_unavailable: auto_start.is_err(),
                    auto_start: auto_start.unwrap_or(false),
                    listeners: status
                        .as_ref()
                        .map(|r| r.listeners.clone())
                        .unwrap_or_default(),
                    server_status: status.as_ref().map(|r| ServerSnapshot {
                        pid: r.pid,
                        uptime_secs: r.uptime_secs.to_string(),
                        active: r.active.to_string(),
                        accepted: r.accepted.to_string(),
                        rejected: r.rejected.to_string(),
                        upload: r.upload.to_string(),
                        download: r.download.to_string(),
                        listeners: r.listeners.clone(),
                    }),
                    ..Default::default()
                }),
                changed: false,
                id: name.clone(),
                name,
                kind: InstanceKind::Server,
                config,
                pid: status.as_ref().map(|s| s.pid),
                state: state.into(),
                uptime_secs: status.map_or(0, |s| s.uptime_secs).to_string(),
                capabilities: vec![
                    "document".into(),
                    "monitor".into(),
                    "start".into(),
                    "stop".into(),
                    "reload".into(),
                    "logs".into(),
                    "server_status".into(),
                    "restart".into(),
                    "validate".into(),
                    "remove".into(),
                    "set_config".into(),
                    "clone_config".into(),
                ],
            });
        }
        for row in &mut rows {
            row.id = self.store.identity(&row.name).await?;
        }
        Ok(rows)
    }
    async fn document(&self, name: &str) -> Result<Document> {
        let resolved = self.store.resolve(name).await?;
        let name = resolved.as_str();
        let (path, kind) = if let Some(path) = self.server_path(name).await? {
            (path, InstanceKind::Server)
        } else {
            let row = self
                .call(old::Action::List)
                .await?
                .rows
                .into_iter()
                .find(|r| r.instance.name == name)
                .context("instance not found")?;
            (
                row.instance.config,
                if row.instance.kind == old::Kind::Proxy {
                    InstanceKind::Proxy
                } else {
                    InstanceKind::Rule
                },
            )
        };
        let path = std::fs::canonicalize(path)?;
        let text = bounded_text(&path)?;
        Ok(Document {
            instance: self.store.identity(name).await?,
            path,
            revision: store::revision(&text),
            text,
            kind,
        })
    }
    async fn execute(&self, id: String, command: Command) -> Result<ResponseBody> {
        let fingerprint = store::revision(&serde_json::to_string(&command)?);
        if let Some(result) = self.store.begin(&id, &fingerprint).await? {
            return Ok(self
                .uncertain
                .lock()
                .unwrap()
                .get(&id)
                .cloned()
                .map(ResponseBody::Task)
                .unwrap_or(result));
        }
        let (progress, mut updates) = tokio::sync::watch::channel(None);
        let operation = astra_ipc::progress::scope(progress, self.command(command, &id));
        tokio::pin!(operation);
        let result = loop {
            tokio::select! {
                result=&mut operation=>break result,
                changed=updates.changed()=>{
                    if changed.is_ok(){
                        let event=updates.borrow_and_update().clone();
                        if let Some(event)=event{if let Err(error)=self.store.event(Some(&id),&event.phase,&event.detail).await {eprintln!("Operation {id} progress persistence failed; execution continues: {error:#}");}}
                    }
                }
            }
        };
        self.finish_operation(&id, &result).await?;
        result
    }
    async fn finish_operation(&self, id: &str, result: &Result<ResponseBody>) -> Result<()> {
        let error = result.as_ref().err().map(|e| format!("{e:#}"));
        if let Err(error) = self
            .store
            .finish(id, result.as_ref().ok(), error.as_deref())
            .await
        {
            let message = format!("outcome unknown for operation {id}: final result could not be persisted ({error:#}); query this operation before retrying");
            self.uncertain.lock().unwrap().insert(
                id.into(),
                Task {
                    id: id.into(),
                    state: TaskState::OutcomeUnknown,
                    message: message.clone(),
                    started_ms: store::now(),
                    finished_ms: None,
                },
            );
            anyhow::bail!(message);
        }
        Ok(())
    }
    async fn command(&self, mut command: Command, operation_id: &str) -> Result<ResponseBody> {
        match &mut command {
            Command::Remove { instance }
            | Command::Start { instance }
            | Command::StartSilent { instance }
            | Command::ForceStart { instance }
            | Command::Stop { instance }
            | Command::Restart { instance }
            | Command::Reload { instance }
            | Command::Validate { instance }
            | Command::PrepareConfig { instance }
            | Command::Runtime { instance, .. }
            | Command::UpdateSubscriptions { instance }
            | Command::Diagnostics { instance, .. }
            | Command::ExplainRoute { instance, .. }
            | Command::SetLogLevel { instance, .. }
            | Command::BindStart { instance, .. }
            | Command::CloneConfig { instance, .. }
            | Command::SetConfig { instance, .. }
            | Command::NetTool { instance, .. } => {
                *instance = self.store.resolve(instance).await?;
            }
            _ => {}
        }
        let target = match &command {
            Command::Remove { instance }
            | Command::Start { instance }
            | Command::StartSilent { instance }
            | Command::ForceStart { instance }
            | Command::Stop { instance }
            | Command::Restart { instance }
            | Command::Reload { instance }
            | Command::Validate { instance }
            | Command::CloneConfig { instance, .. }
            | Command::SetConfig { instance, .. }
            | Command::SetLogLevel { instance, .. }
            | Command::PrepareConfig { instance } => Some(instance.clone()),
            _ => None,
        };
        if let Some(name) = target {
            if let Some(path) = self.server_path(&name).await? {
                return self.server_command(&name, &path, command).await;
            }
        }
        let action = match command {
            Command::InstallCurrentPackage {
                package_id,
                expected_release_id,
                scope,
            } => {
                let config = self.config.clone();
                let progress = astra_ipc::progress::sender();
                let result = tokio::task::spawn_blocking(move || {
                    crate::install::execute(
                        &config,
                        &package_id,
                        &expected_release_id,
                        scope,
                        |message| {
                            if let Some(progress) = &progress {
                                progress.send_replace(Some(astra_ipc::progress::Progress::new(
                                    "Installing package",
                                    message,
                                )));
                            }
                        },
                    )
                })
                .await??;
                return Ok(ResponseBody::InstallationResult(result));
            }
            Command::CancelTask { operation_id } => {
                let task = self.store.task(&operation_id).await?;
                return Ok(ResponseBody::Cancellation(
                    if matches!(task.state, TaskState::Complete | TaskState::Failed) {
                        Cancellation::AlreadyFinished(task.message)
                    } else {
                        Cancellation::UnsafeToCancel("This operation may already have external effects. Observe its durable task outcome; stopping observation does not stop any core.".into())
                    },
                ));
            }
            Command::Create {
                name,
                kind: InstanceKind::Server,
            } => {
                validate_name(&name)?;
                self.ensure_unused(&name).await?;
                let path = self.config.config_dir.join(format!("{name}.server.toml"));
                let output = tokio::process::Command::new(self.server_binary())
                    .envs(
                        self.config
                            .runtime_dir
                            .as_ref()
                            .map(|path| ("ASTRA_RUNTIME_DIR", path)),
                    )
                    .arg("--init")
                    .arg(&path)
                    .output()
                    .await?;
                check_output(output)?;
                self.store.add_server(&name, &path).await?;
                return Ok(ResponseBody::Done {
                    message: "Server configuration created".into(),
                });
            }
            Command::Add {
                name,
                kind: InstanceKind::Server,
                config,
            } => {
                validate_name(&name)?;
                self.ensure_unused(&name).await?;
                let path = std::fs::canonicalize(config)?;
                self.validate_server(&path, &bounded_text(&path)?).await?;
                self.store.add_server(&name, &path).await?;
                return Ok(ResponseBody::Done {
                    message: "Server registered".into(),
                });
            }
            Command::Create { name, kind } => {
                self.ensure_unused(&name).await?;
                old::Action::Create {
                    name,
                    kind: old_kind(kind)?,
                }
            }
            Command::Add { name, kind, config } => {
                self.ensure_unused(&name).await?;
                old::Action::Add(old::Instance {
                    name,
                    kind: old_kind(kind)?,
                    config,
                })
            }
            Command::Remove { instance } => old::Action::Remove(instance),
            Command::Start { instance } => old::Action::Start(instance),
            Command::StartSilent { instance } => old::Action::StartSilent(instance),
            Command::ForceStart { instance } => old::Action::ForceStart(instance),
            Command::Stop { instance } => old::Action::Stop(instance),
            Command::Restart { instance } => old::Action::Restart(instance),
            Command::Reload { instance } => old::Action::Reload(instance),
            Command::Validate { instance } => old::Action::Validate(instance),
            Command::PrepareConfig { instance } => old::Action::PrepareConfig(instance),
            Command::Runtime {
                instance,
                command,
                expected_pid,
            } => {
                let command = command.into_core()?;
                // Execute requests may use an already-authorized local service.
                // The actor carries a no-UAC policy through the actual spawn,
                // including a service disappearing during the request.
                if matches!(command, astra_model::runtime::Command::Tun(true)) {
                    let reply = self
                        .native
                        .get()
                        .context("Native actor not initialized")?
                        .enable_tun_without_uac(instance, expected_pid)
                        .await
                        .map_err(|error| anyhow::anyhow!("RequiresLocalAction: target service could not enable TUN without interactive elevation: {error:#}"))?;
                    // A successful service handover returns a new PID. The
                    // actor verified expected_pid before replacing the core.
                    return Ok(ResponseBody::Runtime(RuntimeSnapshot::Client(
                        browser_numbers(serde_json::from_str(&reply.text)?),
                    )));
                }
                old::Action::Runtime {
                    name: instance,
                    command,
                    expected_pid,
                }
            }
            Command::UpdateSubscriptions { instance } => old::Action::UpdateSubscriptions(instance),
            Command::Diagnostics { instance, targets } => {
                if let Some(targets) = targets {
                    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
                    let future = self
                        .native
                        .get()
                        .context("Native actor not initialized")?
                        .diagnostics(&instance, targets, tx);
                    tokio::pin!(future);
                    let report = loop {
                        tokio::select! {result=&mut future=>break result?,Some(event)=rx.recv()=>{if let Err(error)=self.store.event(Some(operation_id),"diagnostics_event",&serde_json::to_string(&browser_numbers(serde_json::to_value(event)?))?).await {eprintln!("Diagnostic progress persistence failed; operation continues: {error:#}");}}}
                    };
                    while let Ok(event) = rx.try_recv() {
                        self.store
                            .event(
                                Some(operation_id),
                                "diagnostics_event",
                                &serde_json::to_string(&browser_numbers(serde_json::to_value(
                                    event,
                                )?))?,
                            )
                            .await?;
                    }
                    return Ok(ResponseBody::Diagnostics(browser_numbers(
                        serde_json::to_value(report)?,
                    )));
                }
                let action = match targets {
                    Some(targets) => old::Action::DiagnosticsSelected {
                        name: instance,
                        targets,
                    },
                    None => old::Action::Diagnostics(instance),
                };
                return Ok(ResponseBody::Diagnostics(browser_numbers(
                    serde_json::from_str(&self.call(action).await?.text)?,
                )));
            }
            Command::ExplainRoute { instance, request } => {
                let request = astra_model::RouteRequest {
                    api_version: astra_ipc::API_VERSION,
                    id: 1,
                    target: astra_model::Target {
                        host: request.host,
                        port: request.port,
                    },
                    network: request.network,
                    inbound: request.inbound,
                    destination_ip: request.destination_ip,
                    process: request.process,
                };
                return Ok(ResponseBody::RouteExplanation(browser_numbers(
                    serde_json::from_str(
                        &self
                            .call(old::Action::ExplainRoute {
                                name: instance,
                                request,
                            })
                            .await?
                            .text,
                    )?,
                )));
            }
            Command::SetLogLevel { instance, level } => old::Action::SetLogLevel {
                name: instance,
                level,
            },
            Command::BindStart {
                instance,
                mut targets,
            } => {
                for target in &mut targets {
                    *target = self.store.resolve(target).await?;
                }
                old::Action::BindStart {
                    name: instance,
                    targets,
                }
            }
            Command::CloneConfig {
                instance,
                destination,
            } => old::Action::CloneConfig(instance, destination),
            Command::SetConfig { instance, path } => old::Action::SetConfig(instance, path),
            Command::NetTool { instance, request } => {
                let request = request.into_core()?;
                anyhow::ensure!(
                    request.count <= 100
                        && request.timeout_ms <= 30000
                        && request.interval_ms <= 30000,
                    "Network probe limits exceeded"
                );
                let (tx, mut rx) = tokio::sync::mpsc::channel(64);
                let future = self
                    .native
                    .get()
                    .context("Native actor not initialized")?
                    .nettool(&instance, request, tx);
                tokio::pin!(future);
                let events = loop {
                    tokio::select! {result=&mut future=>break result?,Some(event)=rx.recv()=>{if let Err(error)=self.store.event(Some(operation_id),"nettool_event",&serde_json::to_string(&browser_numbers(serde_json::to_value(event)?))?).await {eprintln!("Network tool progress persistence failed; operation continues: {error:#}");}}}
                };
                while let Ok(event) = rx.try_recv() {
                    self.store
                        .event(
                            Some(operation_id),
                            "nettool_event",
                            &serde_json::to_string(&browser_numbers(serde_json::to_value(event)?))?,
                        )
                        .await?;
                }
                let events = events
                    .into_iter()
                    .map(|event| Ok(browser_numbers(serde_json::to_value(event)?)))
                    .collect::<Result<Vec<_>>>()?;
                return Ok(ResponseBody::NetTool(events));
            }
            Command::Update { request } => old::Action::UpdateTransaction { request },
            Command::UpdateArtifact { artifact_id } => {
                let root = self.config.state_dir.join("artifacts");
                let package = tokio::task::spawn_blocking(move || {
                    crate::artifacts::package_view(&root, &artifact_id)
                })
                .await??;
                let reply = self
                    .call(old::Action::UpdateTransaction {
                        request: old::updates::UpdateRequest::new(
                            old::updates::UpdateIntent::Stage {
                                package: package.path.clone(),
                            },
                        ),
                    })
                    .await?;
                return Ok(ResponseBody::UpdateStatus(serde_json::from_str(
                    &reply.text,
                )?));
            }
            Command::RecoverUpdate { operation_id } => old::Action::RecoverUpdate { operation_id },
            Command::Shutdown => {
                let mut failures = Vec::new();
                if let Err(error) = self.shutdown_servers().await {
                    failures.push(format!("servers: {error:#}"));
                }
                let native = self.call(old::Action::Shutdown).await;
                if let Err(error) = native {
                    failures.push(format!("native instances: {error:#}"));
                }
                anyhow::ensure!(
                    failures.is_empty(),
                    "Shutdown incomplete: {}",
                    failures.join("; ")
                );
                return Ok(ResponseBody::Done {
                    message: "All instances stopped".into(),
                });
            }
        };
        let removed = if let old::Action::Remove(name) = &action {
            Some(name.clone())
        } else {
            None
        };
        let update = matches!(
            &action,
            old::Action::UpdateTransaction { .. } | old::Action::RecoverUpdate { .. }
        );
        let runtime = matches!(&action, old::Action::Runtime { .. });
        let message = self.call(action).await?.text;
        if update {
            return Ok(ResponseBody::UpdateStatus(serde_json::from_str(&message)?));
        }
        if runtime {
            return Ok(ResponseBody::Runtime(RuntimeSnapshot::Client(
                browser_numbers(serde_json::from_str(&message)?),
            )));
        }
        if let Some(name) = removed {
            self.store.forget_identity(&name).await?;
        }
        Ok(ResponseBody::Done { message })
    }
    async fn ensure_unused(&self, name: &str) -> Result<()> {
        validate_name(name)?;
        anyhow::ensure!(
            !self
                .instances()
                .await?
                .iter()
                .any(|i| i.name.eq_ignore_ascii_case(name)),
            "Conflict: instance name already exists"
        );
        Ok(())
    }
    async fn server_command(
        &self,
        name: &str,
        path: &Path,
        command: Command,
    ) -> Result<ResponseBody> {
        let endpoint = self.server_endpoint(name);
        if matches!(&command, Command::Remove { .. } | Command::SetConfig { .. })
            && endpoint.exists()
        {
            astra_platform::platform::recover_stale_endpoint(&endpoint)?;
        }
        match command {
            Command::Start { .. } | Command::StartSilent { .. } => {
                self.start_server(name, path).await?
            }
            Command::Stop { .. } => {
                self.stop_server(name).await?;
            }
            Command::Reload { .. } => {
                self.reload_server(name).await?;
            }
            Command::Restart { .. } => {
                self.stop_server(name).await?;
                self.start_server(name, path).await?;
            }
            Command::Validate { .. } => self.validate_server(path, &bounded_text(path)?).await?,
            Command::PrepareConfig { .. } => {
                self.validate_server(path, &bounded_text(path)?).await?;
                return Ok(ResponseBody::Done {
                    // The frontend opens a nonempty prepare result as a new
                    // document. An existing, valid profile is ready to start.
                    message: String::new(),
                });
            }
            Command::SetLogLevel { .. } => {
                anyhow::bail!(
                    "Unsupported: server cores do not expose a configurable log capture level"
                );
            }
            Command::SetConfig { path: new_path, .. } => {
                let new_path = std::fs::canonicalize(new_path)?;
                self.validate_server(&new_path, &bounded_text(&new_path)?)
                    .await?;
                anyhow::ensure!(
                    !endpoint.exists(),
                    "Stop server before changing its configuration binding"
                );
                self.store.set_server_config(name, &new_path).await?;
            }
            Command::CloneConfig { destination, .. } => {
                use astra_platform::platform::OpenOptionsExt;
                use std::io::Write;
                let text = bounded_text(path)?;
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&destination)?;
                file.write_all(text.as_bytes())?;
                file.sync_all()?;
                return Ok(ResponseBody::Done {
                    message: destination.display().to_string(),
                });
            }
            Command::Remove { .. } => {
                anyhow::ensure!(
                    !endpoint.exists(),
                    "Stop server before removing its registration"
                );
                self.store.remove_server(name).await?;
                self.store.forget_identity(name).await?;
            }
            _ => anyhow::bail!("Unsupported server operation"),
        }
        Ok(ResponseBody::Done {
            message: "Server operation completed".into(),
        })
    }
    async fn start_server(&self, name: &str, path: &Path) -> Result<()> {
        let endpoint = self.server_endpoint(name);
        if endpoint.exists() {
            astra_platform::platform::recover_stale_endpoint(&endpoint)?;
        }
        anyhow::ensure!(
            !endpoint.exists(),
            "Server endpoint exists; inspect the running instance before starting"
        );
        match astra_platform::storage::read_bounded(&self.server_record(name), 64 * 1024) {
            Ok(bytes) => {
                let identity: astra_model::control::Identity = serde_json::from_slice(&bytes)?;
                anyhow::ensure!(!astra_platform::lifecycle::matches_live(&identity.process)?,"Recorded server is still alive; an unresponsive endpoint does not authorize a duplicate start");
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        self.validate_server(path, &bounded_text(path)?).await?;
        use astra_platform::platform::OpenOptionsExt;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(self.config.state_dir.join(format!("server-{name}.log")))?;
        let mut command = std::process::Command::new(self.server_binary());
        command.envs(
            self.config
                .runtime_dir
                .as_ref()
                .map(|path| ("ASTRA_RUNTIME_DIR", path)),
        );
        command
            .arg("--config")
            .arg(path)
            .arg("--control")
            .arg(&endpoint)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        command.env(
            "ASTRA_SERVICE_OWNER",
            serde_json::to_string(&astra_platform::lifecycle::capture(std::process::id())?)?,
        );
        astra_platform::platform::background_command(&mut command, true);
        let mut child = ServerCandidate(Some(command.spawn()?));
        for _ in 0..100 {
            if let Ok(status) =
                astra_management::server::call(&endpoint, astra_management::server::Action::Status)
                    .await
            {
                let ready = (|| -> Result<_> {
                    let identity = status
                        .identity
                        .context("Server does not support lifecycle identity")?;
                    anyhow::ensure!(
                        identity.process.pid == child.id()
                            && astra_platform::lifecycle::matches_live(&identity.process)?,
                        "Server identity changed during readiness"
                    );
                    astra_platform::storage::write_json(&self.server_record(name), &identity)?;
                    Ok(identity)
                })();
                let identity = match ready {
                    Ok(identity) => identity,
                    Err(error) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(error);
                    }
                };
                if let Err(error) = astra_management::server::call(
                    &endpoint,
                    astra_management::server::Action::CommitService {
                        boot_id: identity.boot_id.clone(),
                    },
                )
                .await
                {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error);
                }
                let exited = self
                    .config
                    .state_dir
                    .join(format!("server-{name}.exit.json"));
                let mut child = child.0.take().unwrap();
                std::thread::spawn(move || {
                    let result = child.wait();
                    let _ = astra_platform::storage::write_json(
                        &exited,
                        &serde_json::json!({"boot_id":identity.boot_id,"exit":result.map(|status|status.to_string()).unwrap_or_else(|e|e.to_string()),"timestamp_ms":store::now()}),
                    );
                });
                return Ok(());
            }
            if let Some(status) = child.try_wait()? {
                anyhow::bail!("Server exited during startup: {status}");
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let _ = child.kill();
        let _ = child.wait();
        anyhow::bail!(
            "Server did not confirm readiness; candidate stopped, inspect its retained log"
        )
    }
    fn server_record(&self, name: &str) -> PathBuf {
        self.config
            .state_dir
            .join(format!("server-{name}.process.json"))
    }
    async fn reload_server(&self, name: &str) -> Result<()> {
        let previous = self.verify_server(name).await?;
        let reply = astra_management::server::call(
            &self.server_endpoint(name),
            astra_management::server::Action::Reload { path: None },
        )
        .await?;
        let identity = reply
            .identity
            .context("Reload response has no lifecycle identity")?;
        anyhow::ensure!(
            identity.boot_id == previous.boot_id && identity.process == previous.process,
            "Outcome unknown: server identity changed during reload"
        );
        astra_platform::storage::write_json(&self.server_record(name), &identity)
            .context("Outcome unknown: reload completed but process record checkpoint failed")?;
        Ok(())
    }
    async fn verify_server(&self, name: &str) -> Result<astra_model::control::Identity> {
        let recorded: astra_model::control::Identity = serde_json::from_slice(
            &astra_platform::storage::read_bounded(&self.server_record(name), 64 * 1024)?,
        )?;
        anyhow::ensure!(
            astra_platform::lifecycle::matches_live(&recorded.process)?,
            "Recorded server process has exited"
        );
        let status = astra_management::server::call(
            &self.server_endpoint(name),
            astra_management::server::Action::Status,
        )
        .await?;
        let actual = status
            .identity
            .context("Server lifecycle identity unavailable")?;
        anyhow::ensure!(
            actual.boot_id == recorded.boot_id && actual.process == recorded.process,
            "Server boot identity mismatch; operation rejected"
        );
        let path = self
            .server_path(name)
            .await?
            .context("Server registration missing")?;
        anyhow::ensure!(
            actual.applied.as_ref().is_some_and(
                |a| std::fs::canonicalize(&a.path).ok() == std::fs::canonicalize(&path).ok()
            ),
            "Server configuration binding mismatch"
        );
        astra_platform::storage::write_json(&self.server_record(name), &actual)?;
        Ok(actual)
    }
    async fn stop_server(&self, name: &str) -> Result<()> {
        let endpoint = self.server_endpoint(name);
        let record = self.server_record(name);
        match astra_platform::storage::read_bounded(&record, 64 * 1024) {
            Ok(bytes) => {
                let identity: astra_model::control::Identity = serde_json::from_slice(&bytes)?;
                if !astra_platform::lifecycle::matches_live(&identity.process)? {
                    astra_platform::platform::recover_stale_endpoint(&endpoint)?;
                    std::fs::remove_file(record)?;
                    return Ok(());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !endpoint.exists() => {
                return Ok(())
            }
            Err(error) => return Err(error.into()),
        }
        let identity = self.verify_server(name).await?;
        astra_management::server::call(
            &endpoint,
            astra_management::server::Action::StopIdentified {
                boot_id: identity.boot_id.clone(),
                force: false,
            },
        )
        .await?;
        let wait = async {
            while astra_platform::lifecycle::matches_live(&identity.process)? {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Ok::<(), std::io::Error>(())
        };
        let stopped = tokio::time::timeout(std::time::Duration::from_secs(30), wait).await;
        if let Ok(result) = &stopped {
            result
                .as_ref()
                .map_err(|error| anyhow::anyhow!("Could not observe server exit: {error}"))?;
        }
        if stopped.is_err() {
            astra_management::server::call(
                &endpoint,
                astra_management::server::Action::StopIdentified {
                    boot_id: identity.boot_id,
                    force: true,
                },
            )
            .await?;
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while astra_platform::lifecycle::matches_live(&identity.process)? {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Ok::<(), std::io::Error>(())
            })
            .await??;
        }
        astra_platform::platform::recover_stale_endpoint(&endpoint)?;
        match std::fs::remove_file(record) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }
    pub(crate) async fn shutdown_servers(&self) -> Result<()> {
        let names = self.servers().await?;
        let results = futures_util::future::join_all(
            names
                .iter()
                .map(|(name, _)| async move { (name, self.stop_server(name).await) }),
        )
        .await;
        let errors: Vec<_> = results
            .into_iter()
            .filter_map(|(name, result)| result.err().map(|error| format!("{name}: {error:#}")))
            .collect();
        anyhow::ensure!(
            errors.is_empty(),
            "Server shutdown incomplete: {}",
            errors.join("; ")
        );
        Ok(())
    }
    async fn validate_server(&self, path: &Path, text: &str) -> Result<()> {
        let mut child = tokio::process::Command::new(self.server_binary())
            .envs(
                self.config
                    .runtime_dir
                    .as_ref()
                    .map(|path| ("ASTRA_RUNTIME_DIR", path)),
            )
            .arg("--check-stdin")
            .arg(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        child
            .stdin
            .take()
            .context("validation stdin")?
            .write_all(text.as_bytes())
            .await?;
        let output =
            tokio::time::timeout(std::time::Duration::from_secs(30), child.wait_with_output())
                .await??;
        check_output(output)
    }
    async fn draft(&self, command: DraftCommand) -> Result<ResponseBody> {
        match command {
            DraftCommand::Schema { draft_id, path } => {
                let draft = self.store.draft(&draft_id).await?;
                let document = astra_app::Document::parse(model_kind(draft.kind), &draft.text)?;
                Ok(ResponseBody::Schema(document.schema(
                    &path,
                    astra_model::document::Platform::current(),
                )?))
            }
            DraftCommand::Apply {
                draft_id,
                expected_revision,
                command,
            } => {
                let mut draft = self.store.draft(&draft_id).await?;
                anyhow::ensure!(
                    draft.revision == expected_revision,
                    "Conflict: draft changed in another frontend"
                );
                let mut document = astra_app::Document::parse(model_kind(draft.kind), &draft.text)?;
                document.apply(command)?;
                draft.text = document.text().into();
                draft.last_save = None;
                Ok(ResponseBody::Draft(
                    self.store.replace_draft(draft, &expected_revision).await?,
                ))
            }
            DraftCommand::Open { instance, draft_id } => {
                let doc = self.document(&instance).await?;
                Ok(ResponseBody::Draft(
                    self.store.open_draft(draft_id, doc).await?,
                ))
            }
            DraftCommand::Read { draft_id } => {
                Ok(ResponseBody::Draft(self.store.draft(&draft_id).await?))
            }
            DraftCommand::Edit {
                draft_id,
                expected_revision,
                text,
            } => {
                anyhow::ensure!(text.len() <= 480 * 1024, "Configuration exceeds 480 KiB");
                let mut draft = self.store.draft(&draft_id).await?;
                draft.text = text;
                draft.last_save = None;
                Ok(ResponseBody::Draft(
                    self.store.replace_draft(draft, &expected_revision).await?,
                ))
            }
            DraftCommand::Discard {
                draft_id,
                expected_revision,
            } => {
                self.store.discard(&draft_id, &expected_revision).await?;
                Ok(ResponseBody::Done {
                    message: "Draft discarded; source configuration unchanged".into(),
                })
            }
            DraftCommand::Save {
                draft_id,
                expected_revision,
                expected_source_revision,
                operation_id,
            } => {
                let fingerprint = store::revision(&serde_json::to_string(&(
                    &draft_id,
                    &expected_revision,
                    &expected_source_revision,
                ))?);
                if let Some(previous) = self.store.begin(&operation_id, &fingerprint).await? {
                    return Ok(previous);
                }
                let mut result = self
                    .save_draft(
                        &draft_id,
                        &expected_revision,
                        &expected_source_revision,
                        &operation_id,
                    )
                    .await;
                if let Err(error) = &mut result {
                    if let Some(failure) = error.downcast_mut::<crate::save::SaveFailure>() {
                        failure.outcome.operation_id = operation_id.clone();
                        if matches!(failure.outcome.state, SaveState::OutcomeUnknown) {
                            failure.outcome.message =
                                format!("Save outcome unknown: {}", failure.outcome.message);
                        }
                    } else {
                        let mut outcome = crate::save::outcome(
                            SaveState::Rejected,
                            Some(false),
                            Some(false),
                            format!("{error:#}"),
                        );
                        outcome.operation_id = operation_id.clone();
                        *error = crate::save::SaveFailure { outcome }.into();
                    }
                }
                self.finish_operation(&operation_id, &result).await?;
                result
            }
        }
    }
    async fn save_draft(
        &self,
        id: &str,
        expected: &str,
        source: &str,
        operation_id: &str,
    ) -> Result<ResponseBody> {
        let mut draft = self.store.draft(id).await?;
        let instance_name = self.store.resolve(&draft.instance).await?;
        anyhow::ensure!(
            draft.revision == expected && draft.source_revision == source,
            "Conflict: draft revision changed"
        );
        let current = self.document(&draft.instance).await?;
        astra_app::Document::parse(model_kind(draft.kind), &draft.text)?.validate_structure()?;
        anyhow::ensure!(
            current.path == draft.path && current.kind == draft.kind,
            "Conflict: instance configuration binding changed"
        );
        anyhow::ensure!(
            current.revision == source,
            "Conflict: source configuration changed; draft retained"
        );
        let mut outcome = if draft.kind == InstanceKind::Server {
            self.validate_server(&draft.path, &draft.text).await?;
            anyhow::ensure!(
                store::revision(&bounded_text(&draft.path)?) == source,
                "Conflict: source changed during validation"
            );
            if self.server_endpoint(&instance_name).exists() {
                self.verify_server(&instance_name).await?;
            }
            let backups = self.config.state_dir.join("backups");
            crate::private_dir(&backups)?;
            crate::atomic_write(
                &backups.join(format!("{}.server.toml.bak", draft.instance)),
                draft.original.as_bytes(),
            )?;
            crate::atomic_write(&draft.path, draft.text.as_bytes())?;
            let endpoint = self.server_endpoint(&instance_name);
            if endpoint.exists() {
                if let Err(error) = self.reload_server(&instance_name).await {
                    if let Err(restore) =
                        crate::atomic_write(&draft.path, draft.original.as_bytes())
                    {
                        return Err(crate::save::failed(SaveState::RecoveryFailed,None,None,format!("Server reload failed: {error:#}; restoring source failed: {restore:#}; draft and backup retained")));
                    }
                    if let Err(restore) = self.reload_server(&instance_name).await {
                        return Err(crate::save::failed(SaveState::OutcomeUnknown,Some(false),None,format!("Server reload failed: {error:#}; source restored but runtime could not be confirmed: {restore:#}; draft retained")));
                    }
                    return Err(crate::save::failed(SaveState::Restored,Some(false),Some(false),format!("Server reload failed: {error:#}; previous source and runtime restored; draft retained")));
                }
            }
            let running = endpoint.exists();
            crate::save::outcome(
                if running {
                    SaveState::Applied
                } else {
                    SaveState::NextStart
                },
                Some(true),
                Some(running),
                if running {
                    "Server configuration saved and applied"
                } else {
                    "Server configuration saved; applies on next start"
                },
            )
        } else {
            self.native
                .get()
                .context("Native actor not initialized")?
                .save(instance_name, draft.original.clone(), draft.text.clone())
                .await?
        };
        outcome.operation_id = operation_id.into();
        draft.last_save = Some(outcome.clone());
        draft.original = draft.text.clone();
        draft.source_revision = store::revision(&draft.text);
        Ok(ResponseBody::Draft(
            self.store.replace_draft(draft, expected).await.map_err(|error|{outcome.state=SaveState::OutcomeUnknown;outcome.message=format!("Configuration save completed but draft checkpoint failed: {error:#}. Read source and task before retrying");crate::save::SaveFailure{outcome}})?,
        ))
    }
}
fn old_kind(kind: InstanceKind) -> Result<old::Kind> {
    match kind {
        InstanceKind::Proxy => Ok(old::Kind::Proxy),
        InstanceKind::Rule => Ok(old::Kind::Rule),
        InstanceKind::Server => anyhow::bail!("server requires server adapter"),
    }
}
fn model_kind(kind: InstanceKind) -> astra_model::CoreKind {
    match kind {
        InstanceKind::Proxy => astra_model::CoreKind::Proxy,
        InstanceKind::Rule => astra_model::CoreKind::Rule,
        InstanceKind::Server => astra_model::CoreKind::Server,
    }
}
fn bounded_text(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut text = String::new();
    std::fs::File::open(path)?
        .take(480 * 1024 + 1)
        .read_to_string(&mut text)?;
    anyhow::ensure!(text.len() <= 480 * 1024, "Configuration exceeds 480 KiB");
    Ok(text)
}
fn check_output(output: std::process::Output) -> Result<()> {
    anyhow::ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}
