mod routing;
use crate::backend::{BackendCmd, BackendEvent, Origin};
use astra_client::{
    i18n::t,
    operations::OperationInput,
    protocol::{Action, Kind},
    state::Client,
};
use gpui::{App, Context, Entity};
pub(crate) use routing::{Overlay, WindowEffect};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex},
};

pub type RestartPlan = Arc<
    Mutex<
        Option<(
            astra_client::windows::Context,
            astra_client::windows::Launch,
        )>,
    >,
>;
struct PendingRestart {
    id: u64,
    target: astra_client::windows::Context,
    launch: astra_client::windows::Launch,
    snapshot: Vec<u8>,
    release: Option<String>,
    launch_key: Option<(String, String)>,
}

pub struct AppModel {
    pub server_mode: bool,
    routing: routing::Routing,
    pub client: Client,
    pub handle: tokio::runtime::Handle,
    waker: astra_client::waker::Waker,
    pub socket: std::path::PathBuf,
    pub commands: async_channel::Sender<BackendCmd>,
    pub online: bool,
    pub ready: bool,
    pub startup_error: Option<String>,
    pub connection_errors: std::collections::BTreeMap<&'static str, String>,
    pub next_job: u64,
    quit_prepared: bool,
    quit_pending: bool,
    quit_snapshot: Option<Vec<u8>>,
    last_window_page: Option<usize>,
    pub context: Option<astra_client::windows::Context>,
    pub launch: astra_client::windows::Launch,
    pub resume_ui: bool,
    pub elevation_active: Arc<std::sync::atomic::AtomicBool>,
    restart_ready_path: Option<PathBuf>,
    restart: astra_client::update::Restart,
    restart_plan: RestartPlan,
    restart_pending: Option<PendingRestart>,
    restart_sequence: u64,
    maintenance_checkpoint: Option<(String, Vec<u8>)>,
    window_pages: BTreeMap<u64, usize>,
    restored_windows: Vec<usize>,
    cancelled_jobs: BTreeSet<u64>,
}
impl AppModel {
    pub fn new(
        kind: Kind,
        commands: async_channel::Sender<BackendCmd>,
        handle: tokio::runtime::Handle,
        waker: astra_client::waker::Waker,
    ) -> Self {
        let mut client = Client::new(kind).with_waker(waker.clone());
        if kind == Kind::Server {
            client.page = 5;
        }
        Self {
            server_mode: kind == Kind::Server,
            routing: Default::default(),
            client,
            handle,
            waker: waker.clone(),
            socket: Default::default(),
            commands,
            online: false,
            ready: false,
            startup_error: None,
            connection_errors: Default::default(),
            next_job: 0,
            quit_prepared: false,
            quit_pending: false,
            quit_snapshot: None,
            last_window_page: None,
            context: None,
            launch: Default::default(),
            resume_ui: false,
            elevation_active: Default::default(),
            restart_ready_path: None,
            restart: astra_client::update::Restart::new(false).with_waker(waker.clone()),
            restart_plan: Default::default(),
            restart_pending: None,
            restart_sequence: 0,
            maintenance_checkpoint: None,
            window_pages: Default::default(),
            restored_windows: Vec::new(),
            cancelled_jobs: Default::default(),
        }
    }
    pub fn configure_launch(
        &mut self,
        resume_ui: bool,
        restart_ready: Option<PathBuf>,
        plan: RestartPlan,
    ) {
        self.resume_ui = resume_ui;
        self.restart_ready_path = restart_ready;
        self.restart_plan = plan;
        self.restart = astra_client::update::Restart::new(resume_ui).with_waker(self.waker.clone());
    }
    pub fn register_window(&mut self, id: u64, page: usize) {
        self.window_pages.insert(id, page.min(6));
        self.window_context(id, page.min(6), false);
    }
    pub fn update_window(&mut self, id: u64, page: usize) {
        self.window_pages.insert(id, page.min(6));
    }
    pub fn remove_window(&mut self, id: u64) {
        if let Some(page) = self.window_pages.remove(&id) {
            self.last_window_page = Some(page);
        }
        self.remove_window_routing(id);
    }
    pub fn take_restored_windows(&mut self) -> Vec<usize> {
        std::mem::take(&mut self.restored_windows)
    }
    /// Called by the window only after a frame with the restored client has painted.
    pub fn mark_window_ready(&mut self) {
        if !self.ready {
            return;
        }
        if let Some(context) = &self.context {
            if let Err(error) = self.client.maintenance.mark_ready(context) {
                self.client.message = format!("{error:#}");
            }
        }
        if let Some(path) = self.restart_ready_path.take() {
            if let Err(error) = astra_client::self_update::signal_ready(&path) {
                self.client.message = format!("{error:#}");
            }
        }
    }
    fn session_snapshot(&mut self) -> astra_client::state::Session {
        self.client.remember_draft();
        for viewer in [&self.client.viewer, &self.client.tui_viewer]
            .into_iter()
            .flatten()
        {
            self.client
                .session
                .log_settings
                .insert(viewer.name.clone(), viewer.settings.clone());
        }
        self.client.session.gui_windows = if self.window_pages.is_empty() {
            vec![self.last_window_page.unwrap_or(self.client.page)]
        } else {
            self.window_pages.values().copied().collect()
        };
        self.client.session.clone()
    }
    fn restart_idle(&self) -> bool {
        let ready = self.ready
            && !self
                .elevation_active
                .load(std::sync::atomic::Ordering::Acquire)
            && !self.client.tasks.running()
            && !self.client.installation.busy()
            && (!self.client.installation.visible()
                || self.client.update_launch.pending()
                || self.client.maintenance.record.as_ref().is_some_and(|r| {
                    matches!(
                        r.phase,
                        astra_management::maintenance::Phase::Ready
                            | astra_management::maintenance::Phase::Complete
                    )
                }))
            && !self.client.persistence_paused
            && self.client.recovery.is_none()
            && !self
                .client
                .prompt
                .as_ref()
                .is_some_and(astra_client::state::Prompt::confirmation);
        #[cfg(windows)]
        let ready = ready && self.client.service_change.is_none();
        ready
    }
    fn checkpoint_restart(&mut self, mut target: astra_client::windows::Context) {
        let snapshot = self.session_snapshot();
        let bytes = match serde_json::to_vec(&snapshot) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.client.message = error.to_string();
                self.client.tasks.notice_result(&self.client.message, true);
                self.restart.defer(target);
                return;
            }
        };
        self.restart_sequence += 1;
        let id = self.restart_sequence;
        let mut launch = self.launch.clone();
        launch.kind = Some(self.client.kind);
        launch.view = Some(astra_client::windows::View::from_page(
            snapshot
                .gui_windows
                .first()
                .copied()
                .unwrap_or(self.client.page),
        ));
        launch.instance = self.client.active_profile.clone();
        target.language = astra_client::i18n::language();
        match self.commands.try_send(BackendCmd::CheckpointForRestart {
            id,
            session: Box::new(snapshot),
        }) {
            Ok(()) => {
                self.restart_pending = Some(PendingRestart {
                    launch_key: self.client.update_launch.identity(),
                    id,
                    target,
                    launch,
                    snapshot: bytes,
                    release: self
                        .client
                        .update_status
                        .as_ref()
                        .and_then(astra_client::self_update::completed_release)
                        .map(str::to_owned),
                })
            }
            Err(error) => {
                self.restart.defer(target);
                let message = astra_client::tr!("Cannot save session: {}", error.to_string());
                self.client.message = message.clone();
                self.client
                    .fail(message.clone(), astra_client::state::Resume::Session);
                self.client.tasks.notice_result(&message, true);
            }
        }
    }
    fn complete_restart_checkpoint(
        &mut self,
        id: u64,
        result: Result<(), String>,
        cx: &mut Context<Self>,
    ) {
        if self
            .restart_pending
            .as_ref()
            .is_none_or(|pending| pending.id != id)
        {
            return;
        }
        let pending = self.restart_pending.take().unwrap();
        if let Err(error) = result {
            self.restart.defer(pending.target);
            let message = astra_client::tr!("Cannot save session: {}", error);
            self.client.message = message.clone();
            self.client
                .fail(message.clone(), astra_client::state::Resume::Session);
            self.client.tasks.notice_result(&message, true);
            cx.notify();
            return;
        }
        if !self.restart_idle()
            || (pending.launch_key.is_some()
                && (pending.launch_key != self.client.update_launch.identity()
                    || !self.client.update_launch.due(std::time::Instant::now())))
            || self
                .client
                .update_status
                .as_ref()
                .and_then(astra_client::self_update::completed_release)
                .map(str::to_owned)
                != pending.release
        {
            self.restart.defer(pending.target);
            return;
        }
        let current = self.session_snapshot();
        if serde_json::to_vec(&current).ok().as_deref() != Some(pending.snapshot.as_slice()) {
            self.checkpoint_restart(pending.target);
            return;
        }
        if let Some(context) = &self.context {
            let _guard = self.handle.enter();
            match self.client.maintenance.checkpointed(
                context,
                &pending.target,
                self.waker.clone(),
                self.client.preferences.silent_startup,
            ) {
                Ok(false) => {
                    cx.notify();
                    return;
                }
                Err(error) => {
                    self.client.update_launch.clear();
                    self.client.message = format!("{error:#}");
                    cx.notify();
                    return;
                }
                Ok(true) => {}
            }
        }
        *self
            .restart_plan
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some((pending.target, pending.launch));
        self.quit_prepared = true;
        self.commands.close();
        crate::lifecycle::finish(cx);
    }
    pub fn cancel_task(&mut self, id: u64, cx: &mut Context<Self>) {
        if !self
            .client
            .tasks
            .records
            .iter()
            .any(|task| task.id == id && task.finished.is_none())
        {
            return;
        }
        match self.commands.try_send(BackendCmd::Cancel(id)) {
            Ok(()) => {
                self.cancelled_jobs.insert(id);
                self.forget_window_job(id);
                self.cancelled_jobs
                    .retain(|id| self.client.tasks.records.iter().any(|task| task.id == *id));
                if let Some(task) = self
                    .client
                    .tasks
                    .records
                    .iter_mut()
                    .find(|task| task.id == id)
                {
                    task.finished = Some(std::time::Instant::now());
                    push_task_event(task, t("Cancelled").into());
                }
                self.client.message = t("Cancelled").into();
            }
            Err(error) => self.client.message = error.to_string(),
        }
        cx.notify();
    }
    /// Register a debug view and whether its window is on screen; memory
    /// rounds run only while some registered view is visible.
    pub fn watch_memory(&mut self, id: u64, visible: bool) {
        let waker = self.client.waker.clone();
        self.tokio(|client| {
            client
                .memory
                .get_or_insert_with(|| astra_client::memory::View::new(waker))
                .watch(id, visible)
        });
    }
    /// Drop a debug view's registration; the state goes with the last one.
    pub fn unwatch_memory(&mut self, id: u64) {
        if self
            .client
            .memory
            .as_mut()
            .is_some_and(|memory| memory.unwatch(id))
        {
            self.client.memory = None;
        }
    }
    pub fn tokio<R>(&mut self, f: impl FnOnce(&mut Client) -> R) -> R {
        let _guard = self.handle.enter();
        f(&mut self.client)
    }
    #[cfg(test)]
    pub fn new_test(kind: Kind, commands: async_channel::Sender<BackendCmd>) -> Self {
        static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
        let runtime = RUNTIME.get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap()
        });
        Self::new(kind, commands, runtime.handle().clone(), Default::default())
    }
    pub fn observe_notifications(
        model: &Entity<Self>,
        notifications: async_channel::Receiver<()>,
        cx: &mut App,
    ) {
        model.update(cx, |_, cx| {
            cx.spawn(async move |model, cx| {
                while notifications.recv().await.is_ok() {
                    if model
                        .update(cx, |model, cx| {
                            model.poll_views();
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
        });
        model.update(cx, |_, cx| {
            cx.spawn(async move |model, cx| loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(250))
                    .await;
                if model
                    .update(cx, |model, cx| {
                        if model.poll_views() {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            })
            .detach();
        });
    }
    fn poll_views(&mut self) -> bool {
        let socket = self.socket.clone();
        let mut changed = self.tokio(|client| {
            let mut changed = false;
            for viewer in [&mut client.viewer, &mut client.tui_viewer]
                .into_iter()
                .flatten()
            {
                let revision = viewer.revision();
                if let Some(error) = viewer.poll() {
                    client.message = error;
                }
                changed |= revision != viewer.revision();
            }
            if let Some(view) = &mut client.diagnostics {
                changed |= view.poll();
                client.remember_exit_ips();
            }
            if let Some(view) = &mut client.monitor {
                changed |= view.poll_ready(&socket);
            }
            if let Some(view) = &mut client.memory {
                changed |= view.poll_ready(&socket, &client.rows);
            }
            if !socket.as_os_str().is_empty() {
                // The header's exit IP and TCP latency rounds, as in the TUI.
                changed |= client.poll_exit_probe(&socket);
                let overlay_clear = client.prompt.is_none() && client.editor.is_none();
                changed |= client.poll_installation_ready(&socket, overlay_clear);
            }
            changed |= client.poll_terminal_scan_ready();
            #[cfg(windows)]
            {
                changed |= client.poll_service_change_ready();
            }
            changed |= client.expire_connection_highlights();
            changed |= client.expire_notices();
            changed
        });
        if self.ready
            && self.client.program_info.refresh
            && !self.client.program_info.loading
            && self
                .commands
                .try_send(BackendCmd::RefreshProgramInfo)
                .is_ok()
        {
            self.client.program_info.refresh = false;
            self.client.program_info.loading = true;
            changed = true;
        }
        if self.restart_pending.is_none() && !self.quit_prepared && !self.quit_pending {
            if let Some(context) = &self.context {
                let idle = self.restart_idle();
                let _guard = self.handle.enter();
                let previous_message = self.client.message.clone();
                let launch_label = self.client.update_launch.label();
                match self.restart.poll_client(&mut self.client, context, idle) {
                    Ok(Some(target)) => {
                        drop(_guard);
                        self.checkpoint_restart(target);
                        changed = true;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        self.client.message = astra_client::tr!(
                            "Cannot restart updated interface: {}",
                            format!("{error:#}")
                        );
                        changed = true;
                    }
                }
                changed |= self.client.message != previous_message;
                let next_label = self.client.update_launch.label();
                changed |= launch_label != next_label;
                if self.client.update_launch.pending()
                    && self.client.recovery.is_none()
                    && !self.client.persistence_paused
                {
                    changed |= self.client.message != next_label;
                    self.client.message = next_label;
                }
            }
        }
        if self.maintenance_checkpoint.is_none() {
            if let Some(id) = self.client.maintenance.needs_checkpoint() {
                let snapshot = self.session_snapshot();
                if let Ok(bytes) = serde_json::to_vec(&snapshot) {
                    if self
                        .commands
                        .try_send(BackendCmd::CheckpointForMaintenance {
                            id: id.clone(),
                            session: Box::new(snapshot),
                        })
                        .is_ok()
                    {
                        self.maintenance_checkpoint = Some((id, bytes));
                    }
                }
            }
        }
        changed
    }
    pub fn bridge(
        model: &Entity<Self>,
        events: async_channel::Receiver<BackendEvent>,
        cx: &mut App,
    ) {
        model.update(cx, |_, cx| {
            cx.spawn(async move |model, cx| {
                while let Ok(event) = events.recv().await {
                    if model
                        .update(cx, |model, cx| {
                            model.apply(event, cx);
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
        });
    }
    pub(crate) fn observe_quit(model: &Entity<Self>, cx: &App) {
        let model = model.clone();
        cx.on_app_quit(move |cx| {
            model.update(cx, |model, _| model.prepare_quit());
            async {}
        })
        .detach();
    }
    pub fn origin(&self) -> Origin {
        Origin {
            view_context: (
                self.client.page,
                self.client.kind,
                self.client
                    .active_proxy()
                    .map(|row| row.instance.name.clone()),
            ),
            selected: self.client.selected().map(|r| r.instance.name.clone()),
            overlay_epoch: self.client.overlay_epoch,
        }
    }
    pub(crate) fn prepare_quit(&mut self) {
        if self.quit_prepared {
            return;
        }
        self.quit_prepared = true;
        // Before Ready the client still contains defaults. Saving those while
        // bootstrap is pending would overwrite the session being loaded.
        self.restart_pending = None;
        if self.ready && !self.client.persistence_paused {
            let session = self.session_snapshot();
            // The backend inbox is unbounded, so this cannot fail because of a
            // full queue. Closing it afterward still lets the backend drain
            // this final checkpoint before shutdown joins its thread.
            if let Err(error) = self
                .commands
                .try_send(BackendCmd::Checkpoint(Box::new(session)))
            {
                self.client.message =
                    astra_client::tr!("Cannot save session: {}", error.to_string());
                eprintln!("{}", self.client.message);
            }
        }
        self.commands.close();
    }
    pub fn request_quit(&mut self, cx: &mut Context<Self>) {
        if self.quit_prepared || self.quit_pending {
            return;
        }
        if let Some(pending) = self.restart_pending.take() {
            self.restart.defer(pending.target);
        }
        self.restart_plan
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if !self.ready || self.client.persistence_paused {
            self.quit_prepared = true;
            self.commands.close();
            crate::lifecycle::finish(cx);
            return;
        }
        let session = self.session_snapshot();
        self.quit_snapshot = serde_json::to_vec(&session).ok();
        match self.commands.try_send(BackendCmd::CheckpointForQuit {
            session: Box::new(session),
        }) {
            Ok(()) => self.quit_pending = true,
            Err(error) => self.quit_failed(error.to_string(), cx),
        }
    }
    fn quit_failed(&mut self, error: String, cx: &mut Context<Self>) {
        self.quit_pending = false;
        self.quit_snapshot = None;
        let message = astra_client::tr!("Cannot save session: {}", error);
        self.client.message = message.clone();
        self.client
            .fail(message.clone(), astra_client::state::Resume::Session);
        self.client.tasks.notice_result(&message, true);
        if cx.windows().is_empty() {
            let model = cx.entity();
            cx.defer(move |cx| {
                let _ = crate::window::open(model, cx);
            });
        }
        cx.notify();
    }
    fn complete_quit_checkpoint(&mut self, result: Result<(), String>, cx: &mut Context<Self>) {
        if !self.quit_pending {
            return;
        }
        self.quit_pending = false;
        if let Err(error) = result {
            self.quit_failed(error, cx);
            return;
        }
        let current = self.session_snapshot();
        if serde_json::to_vec(&current).ok() != self.quit_snapshot {
            self.request_quit(cx);
            return;
        }
        self.quit_snapshot = None;
        self.quit_prepared = true;
        self.commands.close();
        crate::lifecycle::finish(cx);
    }
    pub fn start(&mut self, action: Action, cx: &mut Context<Self>) {
        self.start_prompt(action, None, cx);
    }
    pub fn start_prompt(
        &mut self,
        action: Action,
        prompt: Option<(astra_client::state::Prompt, String)>,
        cx: &mut Context<Self>,
    ) {
        if self.client.tasks.records.len() >= 32
            && self
                .client
                .tasks
                .records
                .iter()
                .all(|task| task.finished.is_none())
        {
            self.client.message = t("Task queue is full; wait for an operation to finish").into();
            self.client.tasks.notice_result(&self.client.message, true);
            cx.notify();
            return;
        }
        self.next_job += 1;
        let id = self.next_job;
        let command = BackendCmd::Start(Box::new(crate::backend::Start {
            id,
            input: OperationInput::capture(&self.client),
            action: action.clone(),
            prompt,
            origin: self.origin(),
        }));
        match self.commands.try_send(command) {
            Ok(()) => {
                if self.client.tasks.records.len() >= 32 {
                    if let Some(index) = self
                        .client
                        .tasks
                        .records
                        .iter()
                        .position(|task| task.finished.is_some())
                    {
                        self.client.tasks.records.remove(index);
                    }
                }
                self.client
                    .tasks
                    .records
                    .push_back(astra_client::tasks::Record {
                        id,
                        title: t(&action.summary()).to_owned(),
                        events: VecDeque::new(),
                        started: std::time::Instant::now(),
                        finished: None,
                    });
            }
            Err(error) => self.client.message = error.to_string(),
        }
        cx.notify();
    }
    pub fn apply(&mut self, event: BackendEvent, cx: &mut Context<Self>) {
        let handle = self.handle.clone();
        let _guard = handle.enter();
        match event {
            BackendEvent::Terminate => self.request_quit(cx),
            BackendEvent::MaintenanceCheckpoint { id, result } => {
                if self
                    .maintenance_checkpoint
                    .as_ref()
                    .is_some_and(|(pending, _)| pending == &id)
                {
                    let (_, saved) = self.maintenance_checkpoint.take().unwrap();
                    match result {
                        Ok(())
                            if serde_json::to_vec(&self.session_snapshot()).ok().as_ref()
                                == Some(&saved) =>
                        {
                            if let Err(error) = self.client.maintenance.acknowledge(&id) {
                                self.client.message = format!("{error:#}");
                            }
                        }
                        Ok(()) => {}
                        Err(error) => {
                            self.client.message = error;
                        }
                    }
                }
            }
            BackendEvent::QuitCheckpoint(result) => self.complete_quit_checkpoint(result, cx),
            BackendEvent::RestartCheckpoint { id, result } => {
                self.complete_restart_checkpoint(id, result, cx)
            }
            BackendEvent::LocalProgramInfo(info) => {
                self.client.program_info.local = Some(*info);
                self.client.program_info.local_error = None;
            }
            BackendEvent::Ready(prepared) => {
                let (prepared, mut session) = *prepared;
                self.socket = prepared.socket.clone();
                self.context = Some(prepared.context.clone());
                self.launch = prepared.launch.clone();
                self.ready = true;
                session.kind = prepared.launch.kind.unwrap_or(session.kind);
                self.client = Client::from_session(session).with_waker(self.waker.clone());
                if let Err(error) = self.client.maintenance.register(&prepared.context) {
                    self.client.maintenance.error = Some(format!("{error:#}"));
                }
                let preferences_path = prepared.context.home.join(astra_client::preferences::FILE);
                let (preferences, notice) =
                    astra_client::preferences::load(&preferences_path, &self.client.session);
                self.restored_windows = if self.resume_ui {
                    self.client
                        .session
                        .gui_windows
                        .iter()
                        .copied()
                        .filter(|page| {
                            *page < 7 && (self.client.kind != Kind::Server || *page >= 4)
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                self.client.page = prepared
                    .launch
                    .view
                    .and_then(|view| view.page())
                    .unwrap_or_else(|| {
                        if self.client.kind == Kind::Server {
                            5
                        } else {
                            preferences.start_page.page()
                        }
                    });
                if let Some(page) = self.restored_windows.first().copied() {
                    self.client.page = page;
                }
                if !self.restored_windows.is_empty() {
                    self.restored_windows.remove(0);
                }
                if let Some(instance) = prepared.launch.instance.clone() {
                    self.client.active_profile = Some(instance);
                }
                self.client.preferences = preferences;
                self.client.vim_keys = self.client.preferences.vim_keys;
                self.client.preferences_path = Some(preferences_path);
                self.client.apply_preferences(None);
                #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
                self.client.load_startup_context(&prepared.context);
                self.client.initialize_installation(
                    &prepared.context,
                    &prepared.socket,
                    &prepared.launch,
                    self.resume_ui,
                );
                if astra_management::maintenance::pending(&prepared.context.home).unwrap_or(true) {
                    self.client.installation.notice = false;
                    self.client.installation.open = true;
                }
                if !astra_client::i18n::command_line_language() {
                    astra_client::i18n::set_language(self.client.preferences.language.language());
                }
                crate::theme::apply(&self.client.preferences, cx);
                if let Some(notice) = notice {
                    self.client.message = notice;
                }
            }
            BackendEvent::Rows(reply) => {
                self.online = true;
                self.connection_errors.remove("Manager");
                self.client.update_rows(*reply);
                let key = self
                    .client
                    .active_profile
                    .as_ref()
                    .and_then(|name| {
                        self.client
                            .rows
                            .iter()
                            .find(|row| &row.instance.name == name)
                    })
                    .and_then(|row| row.pid.map(|pid| (row.instance.name.clone(), pid)));
                let key = if self.client.kind == Kind::Server {
                    None
                } else {
                    key
                };
                let old = self
                    .client
                    .runtime_snapshot
                    .as_ref()
                    .map(|snapshot| (self.client.runtime_instance.clone(), snapshot.pid));
                if old.is_some() && old != key {
                    self.client.reset_runtime_identity();
                }
                let _ = self.commands.try_send(BackendCmd::Runtime(key));
            }
            BackendEvent::Runtime {
                name,
                pid,
                snapshot,
            } => {
                if snapshot.snapshot.pid == pid
                    && self
                        .client
                        .rows
                        .iter()
                        .any(|row| row.instance.name == name && row.pid == Some(pid))
                    && self.client.active_profile.as_ref() == Some(&name)
                {
                    self.client.accept_runtime_view(name, *snapshot);
                    self.connection_errors.remove("Runtime");
                }
            }
            BackendEvent::Updates(status) => {
                self.client.update_status = Some(*status);
                self.connection_errors.remove("Updates");
            }
            BackendEvent::ProgramInfo(info) => {
                self.client.program_info.data = Some(*info);
                self.client.program_info.error = None;
                self.client.program_info.loading = false;
                self.connection_errors.remove("Program info");
            }
            BackendEvent::Progress { id, value } => {
                if self.cancelled_jobs.contains(&id) {
                    return;
                }
                self.client.tasks.event(id, value);
            }
            BackendEvent::JobDone {
                id,
                action,
                result,
                origin,
            } => {
                if self.cancelled_jobs.contains(&id) {
                    return;
                }
                if let Some(task) = self
                    .client
                    .tasks
                    .records
                    .iter_mut()
                    .find(|task| task.id == id)
                {
                    task.finished = Some(std::time::Instant::now());
                    push_task_event(task, result.message());
                }
                let (context, owner) = self.routed_apply_context(id, &action, &origin);
                let overlay = Self::result_overlay(&result);
                if owner.is_some() && overlay == Some(Overlay::Editor) {
                    self.client.suspend_editor();
                }
                if self.client.apply_operation(*result, &action, context) {
                    self.request_quit(cx);
                }
                self.finish_window_result(owner, overlay, &action);
            }
            BackendEvent::Rejected { id, message } => {
                self.forget_window_job(id);
                if self.cancelled_jobs.contains(&id) {
                    return;
                }
                if let Some(task) = self
                    .client
                    .tasks
                    .records
                    .iter_mut()
                    .find(|task| task.id == id)
                {
                    task.finished = Some(std::time::Instant::now());
                    push_task_event(task, message.clone());
                }
                self.client.tasks.notice_result(&message, true);
                self.client.message = message;
            }
            BackendEvent::Disconnected { source, message } => {
                if source == "Program info" {
                    self.client.program_info.error = Some(message.clone());
                    self.client.program_info.loading = false;
                }
                if source == "Local program info" {
                    self.client.program_info.local_error = Some(message.clone());
                }
                if source == "Manager" {
                    self.online = false;
                }
                if source == "Manager" || source == "Runtime" {
                    self.client.runtime_disconnected(message.clone());
                }
                self.connection_errors.insert(source, message);
            }
            BackendEvent::Failed(message) => {
                self.online = false;
                self.startup_error = Some(message);
            }
        }
    }
}

fn push_task_event(task: &mut astra_client::tasks::Record, message: String) {
    task.events.push_back(message);
    while task.events.len() > 128 {
        task.events.pop_front();
    }
}

#[cfg(test)]
#[path = "test/model_test.rs"]
mod tests;

#[cfg(test)]
#[path = "test/quit_checkpoint_test.rs"]
mod quit_checkpoint_tests;
