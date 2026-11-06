use crate::backend::{BackendCmd, BackendEvent, Origin};
use astra_client::{
    i18n::t,
    operations::OperationInput,
    protocol::{Action, Kind},
    state::Client,
};
use gpui::{App, Context, Entity};
use std::collections::VecDeque;

pub struct AppModel {
    pub client: Client,
    pub commands: async_channel::Sender<BackendCmd>,
    pub online: bool,
    pub ready: bool,
    pub startup_error: Option<String>,
    pub connection_errors: std::collections::BTreeMap<&'static str, String>,
    pub next_job: u64,
    quit_prepared: bool,
}
impl AppModel {
    pub fn new(kind: Kind, commands: async_channel::Sender<BackendCmd>) -> Self {
        let mut client = Client::new(kind);
        if kind == Kind::Server {
            client.page = 5;
        }
        Self {
            client,
            commands,
            online: false,
            ready: false,
            startup_error: None,
            connection_errors: Default::default(),
            next_job: 0,
            quit_prepared: false,
        }
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
        if self.ready {
            self.client.remember_draft();
            // The backend inbox is unbounded, so this cannot fail because of a
            // full queue. Closing it afterward still lets the backend drain
            // this final checkpoint before shutdown joins its thread.
            if let Err(error) = self.commands.try_send(BackendCmd::Checkpoint(Box::new(
                self.client.session.clone(),
            ))) {
                self.client.message =
                    astra_client::tr!("Cannot save session: {}", error.to_string());
                eprintln!("{}", self.client.message);
            }
        }
        self.commands.close();
    }
    pub fn request_quit(&mut self, cx: &mut Context<Self>) {
        if self.quit_prepared {
            return;
        }
        self.prepare_quit();
        cx.quit();
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
        if matches!(
            action,
            Action::Diagnostics(_)
                | Action::DiagnosticsSelected { .. }
                | Action::DiagnosticsStream { .. }
        ) {
            self.client.message =
                t("Diagnostics is planned for the next implementation phase").into();
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
        match event {
            BackendEvent::Ready(prepared) => {
                let (prepared, mut session) = *prepared;
                self.ready = true;
                session.kind = prepared.launch.kind.unwrap_or(session.kind);
                self.client = Client::from_session(session);
                let preferences_path = prepared.context.home.join(astra_client::preferences::FILE);
                let (preferences, notice) =
                    astra_client::preferences::load(&preferences_path, &self.client.session);
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
                if let Some(instance) = prepared.launch.instance {
                    self.client.active_profile = Some(instance);
                }
                self.client.preferences = preferences;
                self.client.vim_keys = self.client.preferences.vim_keys;
                self.client.preferences_path = Some(preferences_path);
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
                self.connection_errors.remove("Program info");
            }
            BackendEvent::Progress { id, value } => {
                self.client.tasks.event(id, value);
            }
            BackendEvent::JobDone {
                id,
                action,
                result,
                origin,
            } => {
                if let Some(task) = self
                    .client
                    .tasks
                    .records
                    .iter_mut()
                    .find(|task| task.id == id)
                {
                    task.finished = Some(std::time::Instant::now());
                    task.events.push_back(result.message());
                }
                let context =
                    origin.apply_context(&action, &self.origin(), self.client.active_log.is_some());
                if self.client.apply_operation(*result, &action, context) {
                    self.request_quit(cx);
                }
            }
            BackendEvent::Rejected { id, message } => {
                if let Some(task) = self
                    .client
                    .tasks
                    .records
                    .iter_mut()
                    .find(|task| task.id == id)
                {
                    task.finished = Some(std::time::Instant::now());
                    task.events.push_back(message.clone());
                }
                self.client.message = message;
            }
            BackendEvent::Disconnected { source, message } => {
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

#[cfg(test)]
#[path = "test/model_test.rs"]
mod tests;
