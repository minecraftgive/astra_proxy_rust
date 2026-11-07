use super::*;
use astra_client::state::Input;
impl AppWindow {
    pub(super) fn leave_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.panel == Some(Panel::Editor)
            && self
                .model
                .read(cx)
                .owns_overlay(crate::model::Overlay::Editor, self.id)
        {
            if let Some(editor) = &self.editor {
                return editor.update(cx, |editor, cx| editor.prepare_leave(window, cx));
            }
        }
        true
    }
    pub(super) fn navigate(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= PAGES.len()
            || (self.model.read(cx).server_mode && index < 4)
            || !self.leave_editor(window, cx)
        {
            return;
        }
        window.focus(&self.focus, cx);
        let owned_editor = self.panel == Some(Panel::Editor)
            && self
                .model
                .read(cx)
                .owns_overlay(crate::model::Overlay::Editor, self.id);
        self.page = Some(index);
        self.panel = None;
        self.palette = false;
        self.profile_chooser = false;
        self.model.update(cx, |model, cx| {
            if owned_editor {
                model.client.suspend_editor();
            }
            model.client.page = index;
            model.client.overlay_epoch += 1;
            model.window_context(self.id, index, true);
            model.route_window_effects(self.id);
            cx.notify();
        });
        self.record_page(cx);
        cx.notify();
    }
    pub(super) fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.leave_editor(window, cx) {
            return;
        }
        self.palette = false;
        self.profile_chooser = false;
        self.panel = None;
        self.model.update(cx, |model, _| {
            model.window_context(self.id, self.page.unwrap_or(model.client.page), true)
        });
        window.focus(&self.focus, cx);
        cx.notify();
    }
    pub(super) fn show_panel(&mut self, panel: Panel, window: &mut Window, cx: &mut Context<Self>) {
        if panel != Panel::Editor && !self.leave_editor(window, cx) {
            return;
        }
        self.panel = Some(panel);
        self.model.update(cx, |model, _| {
            model.window_context(self.id, self.page.unwrap_or(model.client.page), true)
        });
        self.palette = false;
        self.profile_chooser = false;
        window.focus(&self.focus, cx);
        if panel == Panel::Monitor {
            self.model.update(cx, |model, cx| {
                model.tokio(|client| {
                    client.page = self.page.unwrap_or(client.page);
                    client.open_monitor();
                });
                cx.notify();
            });
        }
        if panel == Panel::Updates {
            self.model.update(cx, |model, cx| {
                model.start(astra_client::protocol::Action::UpdateStatus, cx)
            });
        }
        cx.notify();
    }
    pub(super) fn execute(&mut self, id: Id, window: &mut Window, cx: &mut Context<Self>) {
        if astra_client::commands::SPECS
            .iter()
            .any(|spec| spec.id == id && spec.scope == astra_client::commands::Scope::Editor)
            && !(self.panel == Some(Panel::Editor) && self.model.read(cx).editor_owned_by(self.id))
        {
            return;
        }
        if matches!(
            id,
            Id::Search | Id::NextMatch | Id::PreviousMatch | Id::Pause
        ) {
            if let Some(view) = self
                .mounted
                .as_ref()
                .and_then(|mounted| mounted.view.clone().downcast::<views::logs::View>().ok())
            {
                view.update(cx, |view, cx| view.registered_command(id, window, cx));
                return;
            }
        }
        if self.panel == Some(Panel::Editor)
            && self
                .model
                .read(cx)
                .owns_overlay(crate::model::Overlay::Editor, self.id)
            && matches!(
                id,
                Id::Save
                    | Id::Search
                    | Id::NextMatch
                    | Id::PreviousMatch
                    | Id::Import
                    | Id::ExternalEditor
                    | Id::ReadExternal
                    | Id::CompareDraft
                    | Id::Undo
                    | Id::Redo
                    | Id::Discard
            )
        {
            if let Some(editor) = &self.editor {
                editor.update(cx, |editor, cx| editor.command(id, window, cx));
                return;
            }
        }
        if matches!(
            id,
            Id::Quit
                | Id::Window
                | Id::Settings
                | Id::Palette
                | Id::Profiles
                | Id::Help
                | Id::Tasks
                | Id::Logs
                | Id::Monitor
        ) && !self.leave_editor(window, cx)
        {
            return;
        }
        window.focus(&self.focus, cx);
        self.palette = false;
        self.profile_chooser = false;
        let page = self.page;
        let input = self.model.update(cx, |model, cx| {
            let input = model.tokio(|client| {
                if let Some(page) = page {
                    client.page = page;
                }
                client.run_page_command(client.page, id)
            });
            cx.notify();
            input
        });
        self.page = Some(self.model.read(cx).client.page);
        self.record_page(cx);
        self.handle_input(input, window, cx);
    }
    pub(super) fn handle_input(
        &mut self,
        input: Input,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.model.update(cx, |model, cx| {
            model.route_window_effects(self.id);
            cx.notify();
        });
        self.apply_window_effects(window, cx);
        if self
            .elevation
            .update(cx, |host, cx| host.start(input.clone(), window, cx))
        {
            cx.notify();
            return;
        }
        match input {
            Input::Idle => {}
            Input::Run(action, prompt) => self.model.update(cx, |model, cx| {
                model.start_window_prompt(
                    self.id,
                    self.page.unwrap_or(model.client.page),
                    action,
                    prompt.map(|prompt| *prompt),
                    cx,
                )
            }),
            Input::Quit => {
                if self.leave_editor(window, cx) {
                    self.model.update(cx, |model, cx| model.request_quit(cx));
                }
            }
            Input::OpenWindow(launch) => {
                let _ = open_at(
                    self.model.clone(),
                    launch.view.and_then(|view| view.page()).or(self.page),
                    cx,
                );
            }
            Input::ExternalEditor => {
                self.panel = Some(Panel::Editor);
                let _ = self.content(window, cx);
                if let Some(editor) = &self.editor {
                    editor.update(cx, |editor, cx| editor.open_external(window, cx));
                }
            }
            Input::Install(request) => self.model.update(cx, |model, cx| {
                let socket = model.socket.clone();
                model.tokio(|client| client.start_installation(&socket, request));
                cx.notify();
            }),
            Input::Frontend(id) => match id {
                Id::Palette | Id::Profiles => {
                    self.palette = id == Id::Palette;
                    self.profile_chooser = id == Id::Profiles;
                    self.mounted = None;
                    self.search.update(cx, |input, cx| {
                        input.set_value("", window, cx);
                        input.focus(window, cx);
                    });
                }
                Id::Help => self.show_panel(Panel::Help, window, cx),
                Id::Tasks => self.show_panel(Panel::Tasks, window, cx),
                Id::Logs => self.show_panel(Panel::GuiLog, window, cx),
                Id::Monitor => self.show_panel(Panel::Monitor, window, cx),
                _ => {}
            },
        }
        self.model.update(cx, |model, cx| {
            model.route_window_effects(self.id);
            cx.notify();
        });
        self.apply_window_effects(window, cx);
        cx.notify();
    }
    pub(super) fn apply_window_effects(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let effects = self
            .model
            .update(cx, |model, _| model.take_window_effects(self.id));
        for effect in effects {
            match effect {
                crate::model::WindowEffect::Editor => self.panel = Some(Panel::Editor),
                crate::model::WindowEffect::Diagnostics => self.panel = Some(Panel::Diagnostics),
                crate::model::WindowEffect::Rules => {
                    self.page = Some(3);
                    self.panel = None;
                }
                crate::model::WindowEffect::Profiles => {
                    self.page = Some(5);
                    self.panel = None;
                }
                crate::model::WindowEffect::Logs => {
                    self.page = Some(4);
                    self.panel = None;
                }
            }
            self.palette = false;
            self.profile_chooser = false;
            window.focus(&self.focus, cx);
        }
    }
}
