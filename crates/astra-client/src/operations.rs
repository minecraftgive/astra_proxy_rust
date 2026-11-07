//! Background requests carry explicit input and typed results, never another UI.
use crate::{drafts, editor::Editor, i18n::t, logs, protocol::*, state::*, tr, update};
use anyhow::Result;
use std::collections::BTreeMap;
use std::{collections::HashMap, path::PathBuf};

pub struct OperationInput {
    waker: crate::waker::Waker,
    kind: Kind,
    rows: Vec<Row>,
    details: HashMap<String, InstanceDetails>,
    drafts: HashMap<String, drafts::Draft>,
    metadata_name: Option<String>,
    log_settings: HashMap<String, logs::Settings>,
    vim_keys: bool,
}
impl OperationInput {
    pub fn with_waker(mut self, waker: crate::waker::Waker) -> Self {
        self.waker = waker;
        self
    }
    pub fn capture(ui: &Client) -> Self {
        let mut drafts = ui.drafts.clone();
        if let Some(editor) = &ui.editor {
            let (key, path) = ui.draft_key(&editor.name);
            let old = drafts.get(&key);
            let draft = drafts::Draft {
                path,
                kind: ui.kind,
                editor: editor.clone(),
                conflict: old.is_some_and(|d| d.conflict),
                observed_disk: old.and_then(|d| d.observed_disk.clone()),
            };
            drafts.insert(key, draft);
        }
        Self {
            waker: ui.waker.clone(),
            kind: ui.kind,
            rows: ui.rows.clone(),
            details: ui.details.clone(),
            drafts,
            metadata_name: ui.editor.as_ref().map(|e| e.name.clone()),
            log_settings: ui.log_settings.clone(),
            vim_keys: ui.vim_keys,
        }
    }
    fn target(&self, name: &str) -> MetadataTarget {
        let row = self.rows.iter().find(|row| row.instance.name == name);
        MetadataTarget {
            name: name.into(),
            kind: row.map_or(self.kind, |r| r.instance.kind),
            path: row.map(|r| r.instance.config.clone()).unwrap_or_default(),
            details: self.details.get(name).cloned().unwrap_or_default(),
        }
    }
    fn update_rows(&mut self, reply: &Reply) {
        self.rows = reply.rows.clone();
        self.details = serde_json::from_str(&reply.text).unwrap_or_default();
    }
    pub fn prepare(&self, name: String, disk: String) -> DocumentReady {
        let target = self.target(&name);
        let key = format!("{}\n{}", name, target.path.to_string_lossy());
        let draft = self
            .drafts
            .get(&key)
            .or_else(|| self.drafts.get(&format!("{name}\n")));
        if let Some(saved) = draft {
            let mut saved = saved.clone();
            saved.path = target.path;
            saved.conflict = saved.editor.original != disk;
            saved.observed_disk = saved.conflict.then_some(disk);
            saved.editor.name = name;
            let message = if saved.conflict {
                t("Configuration changed on disk; draft retained. Saving will not overwrite external changes.")
            } else { t("Restored configuration draft") }.into();
            DocumentReady {
                editor: saved.editor.clone(),
                kind: saved.kind,
                observation: Some((key, saved)),
                message,
            }
        } else {
            DocumentReady {
                editor: Editor::new(name, disk).with_kind(target.kind),
                observation: None,
                kind: target.kind,
                message: String::new(),
            }
        }
    }
}

pub struct DocumentReady {
    pub editor: Editor,
    pub observation: Option<(String, drafts::Draft)>,
    kind: Kind,
    message: String,
}

#[derive(Clone)]
pub struct MetadataTarget {
    name: String,
    kind: Kind,
    path: PathBuf,
    details: InstanceDetails,
}
pub struct Metadata {
    target: MetadataTarget,
    subscriptions: Option<BTreeMap<String, SubscriptionNodes>>,
    tags: Option<Vec<String>>,
    unavailable: bool,
}
impl Metadata {
    fn apply_editor(&self, editor: &mut Editor) {
        if let Some(nodes) = &self.subscriptions {
            editor.set_subscription_nodes(nodes.clone());
        }
        if let Some(tags) = &self.tags {
            editor.set_node_tags(tags.clone());
        }
    }
    pub fn apply(self, ui: &mut Client) {
        let current = ui
            .rows
            .iter()
            .find(|row| row.instance.name == self.target.name);
        if current
            .map(|row| &row.instance.config)
            .unwrap_or(&PathBuf::new())
            != &self.target.path
        {
            return;
        }
        if let Some(editor) = ui.editor.as_mut().filter(|e| e.name == self.target.name) {
            self.apply_editor(editor);
            if self.unavailable {
                ui.message = "Node suggestions unavailable; manual entry is supported".into();
            }
        }
    }
}
pub async fn metadata(socket: &std::path::Path, target: MetadataTarget) -> Metadata {
    let mut result = Metadata {
        target,
        subscriptions: None,
        tags: None,
        unavailable: false,
    };
    let target = &result.target;
    if target.kind == Kind::Proxy {
        result.subscriptions = if target.details.subscription_nodes_supported {
            call(socket, Action::SubscriptionNodes(target.name.clone()))
                .await
                .ok()
                .and_then(|reply| serde_json::from_str(&reply.text).ok())
        } else {
            let path = target.path.clone();
            tokio::task::spawn_blocking(move || crate::subscription_view::read(&path))
                .await
                .ok()
                .and_then(Result::ok)
        };
    } else if target.kind == Kind::Rule && target.details.node_tags_supported {
        result.tags = call(socket, Action::NodeTags(target.name.clone()))
            .await
            .ok()
            .and_then(|reply| serde_json::from_str(&reply.text).ok());
        result.unavailable = result.tags.is_none();
    }
    result
}
pub fn metadata_target(ui: &Client) -> Option<MetadataTarget> {
    let editor = ui.editor.as_ref()?;
    let row = ui.rows.iter().find(|row| row.instance.name == editor.name);
    Some(MetadataTarget {
        name: editor.name.clone(),
        kind: row.map_or(ui.kind, |r| r.instance.kind),
        path: row.map(|r| r.instance.config.clone()).unwrap_or_default(),
        details: ui.details.get(&editor.name).cloned().unwrap_or_default(),
    })
}

pub enum JobResult {
    Done {
        message: String,
        rows: Option<Reply>,
        quit: bool,
    },
    Document {
        prepared: DocumentReady,
        binding: Option<Reply>,
    },
    Metadata {
        value: Metadata,
        message: String,
    },
    Runtime {
        name: String,
        snapshot: astra_model::runtime::Snapshot,
        message: String,
    },
    Update(update::View),
    Logs(logs::Viewer),
    Diagnostics {
        view: crate::diagnostics::View,
    },
    Explanation(crate::explanation::View),
    Failed {
        message: String,
        resume: Option<Resume>,
        observed: Option<DocumentReady>,
    },
}
impl JobResult {
    pub fn failed(&self) -> bool {
        matches!(self, Self::Failed { .. })
            || matches!(self, Self::Update(view) if view.status.operation.as_ref().is_some_and(|o|o.error.is_some()))
    }
    pub fn message(&self) -> String {
        match self {
            Self::Done { message, .. }
            | Self::Metadata { message, .. }
            | Self::Runtime { message, .. }
            | Self::Failed { message, .. } => message.clone(),
            Self::Document { prepared, .. } => prepared.message.clone(),
            Self::Update(view) => view
                .status
                .operation
                .as_ref()
                .map(|o| o.phase.label().to_owned())
                .unwrap_or_else(|| t("Update status received").into()),
            _ => String::new(),
        }
    }
}
fn failure(error: impl std::fmt::Display, resume: Option<Resume>) -> JobResult {
    JobResult::Failed {
        message: error.to_string(),
        resume,
        observed: None,
    }
}
fn progress(message: String) {
    astra_ipc::progress::report("Working", message);
}

pub fn perform(
    socket: &std::path::Path,
    input: OperationInput,
    action: Action,
    origin: Option<(Prompt, String)>,
) -> futures_util::future::LocalBoxFuture<'_, JobResult> {
    // Keep each operation's await points in a separate poll function. In debug
    // builds, one large async match reserves stack temporaries for every arm,
    // including the operations that this task will never execute. Dispatch
    // before polling so the progress scope also carries only a pinned pointer.
    if matches!(
        &action,
        Action::UpdateStatus
            | Action::UpdateOperationStatus { .. }
            | Action::UpdateTransaction { .. }
            | Action::RecoverUpdate { .. }
    ) {
        return Box::pin(perform_update(socket, action));
    }
    if let Action::Runtime { name, .. } | Action::WaitElevation { name, .. } = &action {
        return Box::pin(perform_runtime(socket, name.clone(), action));
    }
    if let Action::Diagnostics(name) = action {
        return Box::pin(async move {
            JobResult::Diagnostics {
                view: crate::diagnostics::View::start_with_waker(socket, name, input.waker),
            }
        });
    }
    if let Action::Logs(name) = action {
        return Box::pin(perform_logs(socket, input, name));
    }
    Box::pin(perform_profile(socket, input, action, origin))
}

async fn perform_update(socket: &std::path::Path, action: Action) -> JobResult {
    progress(action_description(&action));
    match call(socket, action.clone())
        .await
        .and_then(|r| update::View::decode(&r.text))
    {
        Ok(view) => JobResult::Update(view),
        Err(error) => {
            let operation_id = match &action {
                Action::UpdateTransaction { request } => Some(request.operation_id.clone()),
                Action::RecoverUpdate { operation_id }
                | Action::UpdateOperationStatus { operation_id } => Some(operation_id.clone()),
                _ => None,
            };
            failure(
                format!(
                    "{error:#} · {}",
                    t("Read update status, then recover the existing operation")
                ),
                operation_id.map(|operation_id| {
                    Resume::Request(Action::UpdateOperationStatus { operation_id })
                }),
            )
        }
    }
}

async fn perform_runtime(socket: &std::path::Path, name: String, action: Action) -> JobResult {
    let message = match &action {
        Action::WaitElevation { .. } => t("TUN enabled by the elevated core"),
        Action::Runtime {
            command: astra_model::runtime::Command::TestGroup(_),
            ..
        } => t("Group test started"),
        _ => t("Runtime updated; profile unchanged"),
    }
    .into();
    match call(socket, action)
        .await
        .and_then(|r| Ok(serde_json::from_str(&r.text)?))
    {
        Ok(snapshot) => JobResult::Runtime {
            name,
            snapshot,
            message,
        },
        Err(error) => failure(format!("{error:#}"), None),
    }
}

async fn perform_logs(
    socket: &std::path::Path,
    mut input: OperationInput,
    name: String,
) -> JobResult {
    // The legacy UI rows retain each instance's kind, while the server adapter
    // deliberately offers log reading without dynamic capture-level control.
    let capture_supported =
        name == "@tui" || (name != "@gui" && input.target(&name).kind != Kind::Server);
    let mut viewer = logs::Viewer::new(
        name.clone(),
        input.log_settings.remove(&name).unwrap_or_default(),
    );
    viewer = viewer.with_waker(input.waker.clone());
    viewer.set_vim_navigation(input.vim_keys);
    viewer.set_capture_supported(capture_supported);
    let result: Result<()> = async {
        if capture_supported {
            viewer.capture_level = call(socket, Action::GetLogLevel(name.clone()))
                .await?
                .text
                .parse()?;
        }
        viewer.connect(socket).await
    }
    .await;
    match result {
        Ok(()) => JobResult::Logs(viewer),
        Err(error) => failure(
            format!("{error:#}"),
            Some(Resume::Request(Action::Logs(name))),
        ),
    }
}

async fn prepare_profile(
    socket: &std::path::Path,
    input: &OperationInput,
    name: &str,
) -> Result<Option<DocumentReady>> {
    progress(t("Checking saved configuration").into());
    let reply = call(socket, Action::PrepareConfig(name.into())).await?;
    if reply.text.is_empty() {
        return Ok(None);
    }
    let mut prepared = input.prepare(name.into(), reply.text);
    metadata(socket, input.target(name))
        .await
        .apply_editor(&mut prepared.editor);
    prepared.message = "Review configuration · Ctrl-S save".into();
    Ok(Some(prepared))
}

async fn profile_failure(
    socket: &std::path::Path,
    input: &OperationInput,
    action: Action,
    origin: Option<(Prompt, String)>,
    error: anyhow::Error,
) -> JobResult {
    let mut message = format!("{error:#}");
    let mut observed = None;
    if let Action::Save { name, .. } = &action {
        if message.contains("File changed on disk")
            || message.contains("File changed during validation")
        {
            if let Ok(reply) = call(socket, Action::Read(name.clone())).await {
                observed = Some(input.prepare(name.clone(), reply.text));
                message.push_str("\nDraft retained. Cancel this error, then F2 → Compare draft with observed disk.");
            }
        }
    }
    let resume = origin
        .map(|(p, text)| Resume::Prompt(p, text))
        .unwrap_or(Resume::Request(action));
    JobResult::Failed {
        message,
        resume: Some(resume),
        observed,
    }
}

async fn read_profile(
    socket: &std::path::Path,
    input: &OperationInput,
    name: String,
    text: String,
    binding: Option<Reply>,
) -> JobResult {
    let mut prepared = input.prepare(name.clone(), text);
    let metadata = metadata(socket, input.target(&name)).await;
    metadata.apply_editor(&mut prepared.editor);
    if metadata.unavailable {
        prepared.message = "Node suggestions unavailable; manual entry is supported".into();
    }
    JobResult::Document { prepared, binding }
}

async fn perform_profile(
    socket: &std::path::Path,
    mut input: OperationInput,
    mut action: Action,
    mut origin: Option<(Prompt, String)>,
) -> JobResult {
    let mut binding = None;
    loop {
        if let Action::Start(name) | Action::Restart(name) | Action::Validate(name) = &action {
            match prepare_profile(socket, &input, name).await {
                Ok(Some(prepared)) => {
                    return JobResult::Document { prepared, binding };
                }
                Ok(None) => {}
                Err(error) => return failure(format!("{error:#}"), Some(Resume::Request(action))),
            }
        }
        progress(format!(
            "{} · {}",
            action_description(&action),
            t("Waiting for manager confirmation")
        ));
        let reply = match call(socket, action.clone()).await {
            Ok(reply) => reply,
            Err(error) => {
                return profile_failure(socket, &input, action, origin, error).await;
            }
        };
        match action {
            Action::Create { name, kind } => {
                input.kind = kind;
                if let Ok(reply) = call(socket, Action::List).await {
                    input.update_rows(&reply);
                    binding = Some(reply);
                }
                action = Action::Read(name);
                origin = None;
            }
            Action::Add(instance) => {
                input.kind = instance.kind;
                if let Ok(reply) = call(socket, Action::List).await {
                    input.update_rows(&reply);
                    binding = Some(reply);
                }
                action = Action::Read(instance.name);
                origin = None;
            }
            Action::SetConfig(name, _) => {
                if let Ok(reply) = call(socket, Action::List).await {
                    input.update_rows(&reply);
                    binding = Some(reply);
                }
                action = Action::Read(name);
                origin = None;
            }
            Action::Read(name) => {
                return read_profile(socket, &input, name, reply.text, binding).await;
            }
            Action::ExplainRoute { .. } => {
                return match serde_json::from_str(&reply.text) {
                    Ok(report) => {
                        JobResult::Explanation(crate::explanation::View { report, scroll: 0 })
                    }
                    Err(error) => failure(error, Some(Resume::Request(action))),
                }
            }
            Action::UpdateSubscriptions(_) => {
                if let Some(name) = input.metadata_name.as_ref() {
                    return JobResult::Metadata {
                        value: metadata(socket, input.target(name)).await,
                        message: reply.text,
                    };
                }
                return JobResult::Done {
                    message: reply.text,
                    rows: None,
                    quit: false,
                };
            }
            Action::List => {
                return JobResult::Done {
                    message: String::new(),
                    rows: Some(reply),
                    quit: false,
                }
            }
            Action::BindStart { .. } => {
                return JobResult::Done {
                    message: reply.text,
                    rows: call(socket, Action::List).await.ok(),
                    quit: false,
                }
            }
            Action::Shutdown => {
                return JobResult::Done {
                    message: reply.text,
                    rows: None,
                    quit: true,
                }
            }
            Action::SetLogLevel { level, .. } => {
                return JobResult::Done {
                    message: tr!("记录级别已改为 {}，无需重启", level.as_str()),
                    rows: None,
                    quit: false,
                }
            }
            _ => {
                return JobResult::Done {
                    message: reply.text,
                    rows: None,
                    quit: false,
                }
            }
        }
    }
}

pub fn merge_observation(ui: &mut Client, prepared: &DocumentReady) {
    let Some((key, observed)) = &prepared.observation else {
        return;
    };
    if ui
        .editor
        .as_ref()
        .is_some_and(|e| e.name == observed.editor.name && e.original != observed.editor.original)
    {
        return;
    }
    if let Some(current) = ui.drafts.get_mut(key) {
        if current.editor.original == observed.editor.original {
            current.conflict = observed.conflict;
            current.observed_disk = observed.observed_disk.clone();
        }
    } else {
        ui.drafts.insert(key.clone(), observed.clone());
    }
}

pub struct ApplyContext {
    pub same: bool,
    pub log_target_current: bool,
    pub background: bool,
}
pub trait OperationHost {
    fn client_mut(&mut self) -> &mut Client;
    fn accept_runtime(&mut self, name: String, snapshot: astra_model::runtime::Snapshot);
}
impl OperationHost for Client {
    fn client_mut(&mut self) -> &mut Client {
        self
    }
    fn accept_runtime(&mut self, name: String, snapshot: astra_model::runtime::Snapshot) {
        Client::accept_runtime(self, name, snapshot);
    }
}
impl Client {
    pub fn apply_operation(
        &mut self,
        result: JobResult,
        action: &Action,
        context: ApplyContext,
    ) -> bool {
        apply_operation(self, result, action, context)
    }
}
pub fn apply_operation<H: OperationHost>(
    host: &mut H,
    result: JobResult,
    action: &Action,
    context: ApplyContext,
) -> bool {
    if let JobResult::Runtime {
        name,
        snapshot,
        message,
    } = result
    {
        host.client_mut().operation_failed = false;
        host.client_mut().refresh_paused = false;
        if context.same {
            host.accept_runtime(name, snapshot);
        }
        let client = host.client_mut();
        client.message = message;
        client.tasks.notice_result(&client.message, false);
        return false;
    }
    let ui = host.client_mut();
    let failed = result.failed();
    if failed {
        ui.reject_proxy_selection(action);
    }
    let mut message = result.message();
    let mut quit = false;
    ui.operation_failed = failed;
    if failed {
        ui.search_on_open = false;
    } else {
        ui.refresh_paused = false;
    }
    if let Action::Save {
        name,
        original,
        text,
    } = action
    {
        if !failed {
            for draft in ui
                .drafts
                .values_mut()
                .filter(|d| d.editor.name == *name && d.editor.original == *original)
            {
                draft.editor.original = text.clone();
                draft.conflict = false;
                draft.observed_disk = None;
            }
            if let Some(editor) = ui
                .editor
                .as_mut()
                .filter(|e| e.name == *name && (e.original == *original || e.original == *text))
            {
                editor.original = text.clone();
            }
        }
    }
    match result {
        JobResult::Done {
            rows, quit: exit, ..
        } => {
            if let Some(reply) = rows {
                // The subscription owns live rows while jobs run; an older
                // command reply must not rewind a newer PID/state snapshot.
                if !context.background {
                    ui.update_rows(reply);
                }
            }
            quit = exit;
            if let Action::SetLogLevel { name, level } = action {
                if let Some(viewer) = ui.viewer.as_mut().filter(|v| v.name == *name) {
                    viewer.capture_changed(*level);
                }
            }
        }
        JobResult::Document { prepared, binding } => {
            merge_observation(ui, &prepared);
            let name = prepared.editor.name.clone();
            if context.same && (ui.editor.is_none() || !context.background) {
                if let Some(reply) = binding {
                    if !context.background {
                        ui.update_rows(reply);
                    }
                }
                ui.kind = prepared.kind;
                if let Some(index) = ui.rows().iter().position(|r| r.instance.name == name) {
                    let kind = ui.index();
                    ui.selections[kind] = index;
                }
                ui.editor = Some(prepared.editor);
                ui.apply_pending_search();
            } else {
                ui.ready_editors.insert(name, prepared.editor);
            }
        }
        JobResult::Metadata { value, .. } => value.apply(ui),
        JobResult::Runtime { .. } => unreachable!(),
        JobResult::Update(view) => {
            ui.update_status = Some(view.status.clone());
            ui.update_view = Some(view);
        }
        JobResult::Logs(viewer) => {
            if crate::tui_log::source_file(&viewer.name).is_some() {
                if context.log_target_current {
                    ui.tui_viewer = Some(viewer);
                }
            } else if context.same {
                ui.active_log = Some(viewer.name.clone());
                ui.viewer = Some(viewer);
            }
        }
        JobResult::Diagnostics { mut view } => {
            if context.same {
                if std::mem::take(&mut ui.open_nettools) {
                    view.open_tools();
                }
                ui.diagnostics = Some(view);
            }
        }
        JobResult::Explanation(view) => {
            if context.same {
                ui.explanation = Some(view);
            }
        }
        JobResult::Failed {
            resume, observed, ..
        } => {
            if let Some(observed) = observed {
                merge_observation(ui, &observed);
            }
            if matches!(action,Action::Logs(name) if crate::tui_log::source_file(name).is_some())
                && context.log_target_current
            {
                ui.tui_viewer = None;
            }
            if context.same
                && context.log_target_current
                && (!context.background || (ui.prompt.is_none() && ui.recovery.is_none()))
            {
                if let Some(resume) = resume {
                    ui.fail(message.clone(), resume);
                }
            }
        }
    }
    if !context.background && failed {
        if let Some(recovery) = &ui.recovery {
            message = recovery.message.clone();
        }
    }
    ui.message = message;
    ui.tasks.notice_result(&ui.message, failed);
    quit
}

pub fn action_description(action: &Action) -> String {
    let (label, name) = match action {
        Action::Logs(name) if name == "@tui" => ("TUI log", ""),
        Action::Logs(name) if name == "@gui" => ("GUI log", ""),
        Action::Logs(name) => ("Logs", name.as_str()),
        Action::Diagnostics(name) => ("Connectivity / Exit IP", name.as_str()),
        Action::NetTool { name, .. } => ("Network tools", name.as_str()),
        Action::Runtime { name, .. } => ("Applying runtime change", name.as_str()),
        Action::PrepareElevation { name, .. } => {
            ("Enable TUN with administrator permission", name.as_str())
        }
        Action::CancelElevation { name, .. } => ("Cancel TUN elevation", name.as_str()),
        Action::WaitElevation { name, .. } => ("Wait for the elevated core", name.as_str()),
        Action::ExplainRoute { name, .. } => ("Route explanation", name.as_str()),
        Action::Create { name, .. } => ("Create", name.as_str()),
        Action::Add(instance) => ("Add", instance.name.as_str()),
        Action::Start(name) => ("Start", name.as_str()),
        Action::ForceStart(name) => ("Force start", name.as_str()),
        Action::Stop(name) => ("Stop", name.as_str()),
        Action::Restart(name) => ("Restart", name.as_str()),
        Action::Reload(name) => ("Reload", name.as_str()),
        Action::Validate(name) => ("Validate", name.as_str()),
        Action::Read(name) => ("Edit", name.as_str()),
        Action::Save { name, .. } => ("Save", name.as_str()),
        Action::UpdateSubscriptions(name) => ("Update subscriptions", name.as_str()),
        Action::UpdatePackage(_) => ("Import local update package", ""),
        Action::UpdateStatus => ("Update status", ""),
        Action::Rollback => ("Roll back release", ""),
        Action::BindStart { name, .. } => ("Startup bindings", name.as_str()),
        Action::Remove(name) => ("Remove", name.as_str()),
        Action::SetConfig(name, _) => ("Config path", name.as_str()),
        Action::CloneConfig(name, _) => ("Copy config", name.as_str()),
        Action::Shutdown => ("Stop all", ""),
        _ => ("Processing request", ""),
    };
    format!("{} {}", t(label), name).trim().to_owned()
}

#[cfg(test)]
#[path = "test/operations_thread_test.rs"]
mod thread_tests;
