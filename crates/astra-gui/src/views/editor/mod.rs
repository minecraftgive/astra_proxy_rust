//! Native editor presentation. Documents and every edit remain in astra-client.
mod form;
mod members;
mod raw;
#[cfg(test)]
mod tests;
mod transfer;
mod tree;

use crate::{
    model::AppModel,
    views::ViewEvent,
    widgets::{Card, Empty},
};
use astra_client::{
    commands::Id,
    editor::{Editor, Mode, SourceError},
    i18n::t,
    intents::EditorAction,
    state::{Client, Input as ClientInput},
};
use gpui::{
    div, prelude::*, px, App, Context, Entity, EventEmitter, FocusHandle, Focusable, KeyBinding,
    KeyDownEvent, Render, Subscription, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants},
    input::{
        Editor as CodeEditor, EditorState, Input, InputEvent, InputState, RangeDecorationCollection,
    },
    ActiveTheme, WindowExt,
};

gpui::actions!(
    astra_editor,
    [
        SaveSource,
        ToggleSource,
        ImportNodes,
        ExportNodes,
        ExportLinks
    ]
);
struct Installed;
impl gpui::Global for Installed {}
pub struct View {
    model: Entity<AppModel>,
    document_name: Option<String>,
    focus: FocusHandle,
    source: Entity<EditorState>,
    source_marks: RangeDecorationCollection,
    source_error: Option<SourceError>,
    source_visible: bool,
    source_colors: Vec<gpui::Hsla>,
    field_error: Option<(String, String)>,
    error: Option<String>,
    fields: Vec<form::Field>,
    field_subscriptions: Vec<Subscription>,
    field_signature: String,
    mode_input: Entity<InputState>,
    mode_signature: String,
    member_search: Entity<InputState>,
    compare_visible: bool,
    external_busy: bool,
    external_returned: bool,
    observations: Vec<Subscription>,
}
impl Focusable for View {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl EventEmitter<ViewEvent> for View {}
impl View {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        if !cx.has_global::<Installed>() {
            cx.set_global(Installed);
            cx.bind_keys([
                KeyBinding::new("ctrl-s", SaveSource, Some("AstraEditor > Input")),
                KeyBinding::new(
                    "ctrl-z",
                    gpui_component::input::Undo,
                    Some("AstraEditor > Input"),
                ),
                KeyBinding::new(
                    "ctrl-y",
                    gpui_component::input::Redo,
                    Some("AstraEditor > Input"),
                ),
                KeyBinding::new(
                    "tab",
                    ToggleSource,
                    Some("AstraEditor && !Input && !AstraDialog"),
                ),
                KeyBinding::new(
                    "ctrl-i",
                    ImportNodes,
                    Some("AstraEditor && !Input && !AstraDialog"),
                ),
                KeyBinding::new(
                    "ctrl-e",
                    ExportNodes,
                    Some("AstraEditor && !Input && !AstraDialog"),
                ),
                KeyBinding::new(
                    "ctrl-shift-e",
                    ExportLinks,
                    Some("AstraEditor && !Input && !AstraDialog"),
                ),
            ]);
        }
        let (source, source_marks) = raw::create(window, cx);
        let mode_input = cx.new(|cx| InputState::new(window, cx));
        let member_search =
            cx.new(|cx| InputState::new(window, cx).placeholder(t("Search members")));
        let observations = vec![
            cx.observe_in(&model, window, |view, _, window, cx| {
                view.sync(window, cx);
                cx.notify();
            }),
            cx.subscribe_in(
                &source,
                window,
                |view, input, event, window, cx| match event {
                    InputEvent::Change => {
                        let text = input.read(cx).value().to_string();
                        view.model.update(cx, |model, cx| {
                            if let Some(editor) = &mut model.client.editor {
                                editor.set_source_buffer(text);
                            }
                            model.client.remember_draft();
                            cx.notify();
                        });
                        view.source_error = None;
                        raw::show_error(None, &view.source_marks, cx);
                    }
                    InputEvent::Blur => {
                        view.commit_source(window, cx);
                    }
                    _ => {}
                },
            ),
            cx.subscribe_in(
                &mode_input,
                window,
                |view, input, event, window, cx| match event {
                    InputEvent::Change => {
                        let value = input.read(cx).value().to_string();
                        view.edit(
                            |editor| {
                                editor.replace_input(value);
                                Ok(())
                            },
                            cx,
                        );
                    }
                    InputEvent::PressEnter { .. } => {
                        view.edit(Editor::commit_input, cx);
                        view.sync(window, cx);
                        window.focus(&view.focus, cx);
                    }
                    _ => {}
                },
            ),
            cx.subscribe(&member_search, |_, _, _: &InputEvent, cx| cx.notify()),
            cx.observe_window_activation(window, |view, window, cx| {
                if window.is_window_active() && view.model.read(cx).client.external_draft.is_some()
                {
                    view.external_returned = true;
                    cx.notify();
                }
            }),
        ];
        let mut view = Self {
            model,
            document_name: None,
            focus: cx.focus_handle(),
            source,
            source_marks,
            source_error: None,
            source_visible: false,
            source_colors: Vec::new(),
            field_error: None,
            error: None,
            fields: Vec::new(),
            field_subscriptions: Vec::new(),
            field_signature: String::new(),
            mode_input,
            mode_signature: String::new(),
            member_search,
            compare_visible: false,
            external_busy: false,
            external_returned: false,
            observations,
        };
        view.sync(window, cx);
        if let Some(editor) = view.model.read(cx).client.editor.clone() {
            let name = editor.name.clone();
            let adapters = cx
                .background_executor()
                .spawn(async move { editor.interface_names() });
            cx.spawn_in(window, async move |view, cx| {
                let result = adapters.await;
                let _ = view.update_in(cx, |view, _, cx| {
                    view.model.update(cx, |model, cx| {
                        if let Some(editor) = &mut model.client.editor {
                            if editor.name == name {
                                if let Ok(names) = result {
                                    editor.interfaces = names;
                                    cx.notify();
                                }
                            }
                        }
                    });
                });
            })
            .detach();
        }
        view
    }
    fn edit(
        &mut self,
        operation: impl FnOnce(&mut Editor) -> anyhow::Result<()>,
        cx: &mut Context<Self>,
    ) -> bool {
        let result = self.model.update(cx, |model, cx| {
            let result = model.client.editor.as_mut().map(operation).transpose();
            model.client.remember_draft();
            cx.notify();
            result
        });
        self.error = result.as_ref().err().map(|error| format!("{error:#}"));
        cx.notify();
        result.is_ok()
    }
    fn dispatch(
        &mut self,
        operation: impl FnOnce(&mut Client) -> ClientInput,
        cx: &mut Context<Self>,
    ) {
        let input = self.model.update(cx, |model, cx| {
            let input = model.tokio(operation);
            model.client.remember_draft();
            cx.notify();
            input
        });
        cx.emit(ViewEvent::Input(input));
        cx.notify();
    }
    pub fn command(&mut self, id: Id, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(
            id,
            Id::Save | Id::Import | Id::ExternalEditor | Id::ReadExternal | Id::CompareDraft
        ) && !self.commit_pending(window, cx)
        {
            return;
        }
        if id == Id::CompareDraft {
            self.compare_visible = true;
        }
        if id == Id::ReadExternal {
            self.external_returned = false;
        }
        self.dispatch(|client| client.run_command(id), cx);
    }
    /// Draft I/O and process waiting run off the GPUI event thread.
    pub fn open_external(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.external_busy || !self.commit_pending(window, cx) {
            return;
        }
        let model = self.model.read(cx);
        let Some(context) = &model.context else {
            self.error = Some(t("The application is still connecting").into());
            cx.notify();
            return;
        };
        let home = context.home.clone();
        let session = model.client.session.clone();
        let rows = model.client.rows.clone();
        let identity = model
            .client
            .editor
            .as_ref()
            .map(|editor| (editor.name.clone(), editor.text()));
        self.external_busy = true;
        let prepare = cx.background_executor().spawn(async move {
            let mut client = Client::from_session(session);
            client.rows = rows;
            client.prepare_external(&home)
        });
        cx.spawn_in(window, async move |view, cx| {
            let result = prepare.await;
            let _ = view.update_in(cx, |view, window, cx| {
                view.external_busy = false;
                match result {
                    Err(error) => {
                        view.error = Some(format!("{error:#}"));
                    }
                    Ok((draft, directory)) => {
                        let current = view
                            .model
                            .read(cx)
                            .client
                            .editor
                            .as_ref()
                            .map(|editor| (editor.name.clone(), editor.text()));
                        if current != identity {
                            return;
                        }
                        view.model.update(cx, |model, cx| {
                            model.client.external_draft = Some(draft.clone());
                            cx.notify();
                        });
                        let explicit = ["ASTRA_EDITOR", "VISUAL", "EDITOR"].iter().any(|key| {
                            std::env::var(key).is_ok_and(|value| !value.trim().is_empty())
                        });
                        if !explicit {
                            cx.open_with_system(&draft.path);
                        } else {
                            let terminal = view.model.read(cx).client.preferences.terminal.clone();
                            let launch = cx.background_executor().spawn(async move {
                                astra_client::external_editor::command().and_then(|argv| {
                                    let executable = std::path::Path::new(&argv[0]);
                                    let name = executable.file_stem().and_then(|name| name.to_str()).unwrap_or_default();
                                    if matches!(name, "vi" | "vim" | "nvim" | "nano" | "emacs" | "hx" | "helix") {
                                        let args: Vec<std::ffi::OsString> = argv[1..].iter().map(Into::into).chain(std::iter::once(draft.path.as_os_str().to_owned())).collect();
                                        let outcome = astra_client::terminals::launch_command(&terminal, executable, &args)?;
                                        anyhow::ensure!(matches!(outcome, astra_client::terminals::LaunchOutcome::Launched { .. }), t("No terminal is available. Choose a terminal in Settings or configure a graphical editor."));
                                        Ok(())
                                    } else {
                                        astra_client::external_editor::launch(&argv, &draft, &directory)
                                    }
                                })
                            });
                            view.external_busy = true;
                            cx.spawn_in(window, async move |view, cx| {
                                let result = launch.await;
                                let _ = view.update_in(cx, |view, _, cx| {
                                    view.external_busy = false;
                                    view.external_returned = true;
                                    view.error = result.err().map(|error| format!("{error:#}"));
                                    cx.notify();
                                });
                            })
                            .detach();
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn commit_source(&mut self, _: &mut Window, cx: &mut Context<Self>) -> bool {
        let result = self.model.update(cx, |model, cx| {
            let result = model
                .client
                .editor
                .as_mut()
                .map(Editor::commit_source)
                .transpose();
            model.client.remember_draft();
            cx.notify();
            result
        });
        self.source_error = result.err();
        raw::show_error(self.source_error.as_ref(), &self.source_marks, cx);
        cx.notify();
        self.source_error.is_none()
    }
    fn commit_pending(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.commit_source(window, cx) {
            self.source_visible = true;
            return false;
        }
        let mode = self
            .model
            .read(cx)
            .client
            .editor
            .as_ref()
            .map(|editor| editor.form.mode.clone());
        match mode {
            Some(Mode::Field { .. }) => self.edit(Editor::commit_field, cx),
            Some(Mode::Browse) | None => true,
            _ => {
                self.error =
                    Some(t("Confirm or cancel the current entry before continuing").into());
                cx.notify();
                false
            }
        }
    }
    /// Flush native input first; incomplete fields remain open for correction.
    pub fn prepare_leave(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.commit_pending(window, cx) {
            return false;
        }
        if !self
            .model
            .read(cx)
            .client
            .editor
            .as_ref()
            .is_none_or(Editor::can_close)
        {
            self.edit(
                |editor| {
                    editor.collapse_all();
                    Ok(())
                },
                cx,
            );
        }
        self.model
            .read(cx)
            .client
            .editor
            .as_ref()
            .is_none_or(Editor::can_close)
    }
    fn toggle_source(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.commit_pending(window, cx) {
            return;
        }
        self.source_visible = !self.source_visible;
        if self.source_visible {
            self.source.update(cx, |input, cx| input.focus(window, cx));
        } else {
            window.focus(&self.focus, cx);
        }
        cx.notify();
    }
    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.model.read(cx).client.editor.as_ref() else {
            self.document_name = None;
            self.field_signature.clear();
            self.fields.clear();
            self.field_subscriptions.clear();
            return;
        };
        let name = editor.name.clone();
        let text = editor
            .source_buffer
            .clone()
            .unwrap_or_else(|| editor.text());
        let mode = editor.form.mode.clone();
        if self.document_name.as_ref() != Some(&name) {
            self.document_name = Some(name);
            self.field_signature.clear();
            self.mode_signature.clear();
            self.source_error = None;
            self.error = None;
            self.external_returned = false;
        }
        if self.source.read(cx).value().as_ref() != text {
            self.source
                .update(cx, |input, cx| input.set_value(text, window, cx));
        }
        let (signature, value) = match mode {
            Mode::Header(input) => ("header".to_owned(), input.text),
            Mode::Search(input) => ("search".to_owned(), input.text),
            _ => (String::new(), String::new()),
        };
        if self.mode_signature != signature {
            self.mode_signature = signature;
            let focus = !self.mode_signature.is_empty() && self.focus.contains_focused(window, cx);
            self.mode_input.update(cx, |input, cx| {
                input.set_value(value, window, cx);
                if focus {
                    input.focus(window, cx);
                }
            });
        } else if !self.mode_input.focus_handle(cx).is_focused(window)
            && self.mode_input.read(cx).value().as_ref() != value
        {
            self.mode_input
                .update(cx, |input, cx| input.set_value(value, window, cx));
        }
        self.sync_fields(window, cx);
    }
    fn key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.focus.is_focused(window)
            || window.has_focused_input(cx)
            || self.model.read(cx).client.prompt.is_some()
        {
            return;
        }
        let Some(key) = crate::keys::from_keystroke(&event.keystroke, event.is_held) else {
            return;
        };
        if event.keystroke.key == "escape" {
            if self
                .model
                .read(cx)
                .client
                .editor
                .as_ref()
                .is_some_and(Editor::can_close)
            {
                self.dispatch(|client| client.editor_action(EditorAction::Suspend), cx);
            } else {
                self.edit(|editor| editor.key(key), cx);
            }
        } else {
            let update = self
                .model
                .read(cx)
                .client
                .editor
                .as_ref()
                .is_some_and(|editor| editor.requests_subscription_update(key));
            if update {
                self.dispatch(
                    |client| client.editor_action(EditorAction::UpdateSubscriptions),
                    cx,
                );
            } else {
                let vim = self.model.read(cx).client.vim_keys;
                self.edit(|editor| editor.key_with_vim(key, vim), cx);
            }
        }
        self.sync(window, cx);
        cx.stop_propagation();
    }
}
impl Render for View {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _ = &self.observations;
        let colors = vec![
            cx.theme().foreground,
            cx.theme().muted_foreground,
            cx.theme().primary,
            cx.theme().warning,
            cx.theme().info,
            cx.theme().success,
            cx.theme().danger,
        ];
        if self.source_colors != colors {
            self.source_colors = colors;
            self.source
                .update(cx, |input, cx| input.set_highlighter("toml", cx));
        }
        let Some(editor) = &self.model.read(cx).client.editor else {
            return Empty::new(t("Editor"), t("Open a configuration to edit")).into_any_element();
        };
        let name = editor.name.clone();
        let dirty = editor.source_buffer.is_some() || editor.text() != editor.original;
        let conflict = self
            .model
            .read(cx)
            .client
            .drafts
            .get(&self.model.read(cx).client.draft_key(&name).0)
            .is_some_and(|draft| draft.conflict);
        let mode = editor.form.mode.clone();
        let mut body = div()
            .id("configuration-editor")
            .key_context("AstraEditor")
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .gap_3()
            .on_action(
                cx.listener(|view, _: &SaveSource, window, cx| view.command(Id::Save, window, cx)),
            )
            .on_action(
                cx.listener(|view, _: &ToggleSource, window, cx| view.toggle_source(window, cx)),
            )
            .on_action(cx.listener(|view, _: &ImportNodes, window, cx| {
                view.transfer(false, false, window, cx)
            }))
            .on_action(cx.listener(|view, _: &ExportNodes, window, cx| {
                view.transfer(true, false, window, cx)
            }))
            .on_action(cx.listener(|view, _: &ExportLinks, window, cx| {
                view.transfer(true, true, window, cx)
            }))
            .on_key_down(cx.listener(Self::key))
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .child(format!("{name}{}", if dirty { " •" } else { "" }))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(
                                Button::new("editor-mode")
                                    .label(t(if self.source_visible {
                                        "Structured view"
                                    } else {
                                        "TOML source"
                                    }))
                                    .on_click(cx.listener(|view, _, window, cx| {
                                        view.toggle_source(window, cx)
                                    })),
                            )
                            .child(
                                Button::new("editor-save")
                                    .primary()
                                    .label(t("Save and apply draft"))
                                    .on_click(cx.listener(|view, _, window, cx| {
                                        view.command(Id::Save, window, cx)
                                    })),
                            )
                            .child(Button::new("editor-close").label(t("Close")).on_click(
                                cx.listener(|view, _, window, cx| {
                                    if view.prepare_leave(window, cx) {
                                        view.dispatch(
                                            |client| client.editor_action(EditorAction::Suspend),
                                            cx,
                                        );
                                    }
                                }),
                            )),
                    ),
            )
            .child(self.transfer_bar(cx));
        if conflict {
            body = body.child(
                Card::new(t("Draft conflict"))
                    .child(t(
                        "The configuration on disk changed. Review the difference before saving.",
                    ))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(
                                Button::new("editor-compare")
                                    .label(t("Compare with disk"))
                                    .on_click(cx.listener(|view, _, window, cx| {
                                        view.command(Id::CompareDraft, window, cx)
                                    })),
                            )
                            .child(
                                Button::new("editor-discard")
                                    .label(t("Discard draft"))
                                    .on_click(cx.listener(|view, _, window, cx| {
                                        view.command(Id::Discard, window, cx)
                                    })),
                            ),
                    ),
            );
        }
        if let Some(error) = &self.error {
            body = body.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(error.clone()),
            );
        }
        if self.external_returned {
            body = body.child(div().text_sm().text_color(cx.theme().primary).child(t(
                "External editor returned. Read external changes to update this draft.",
            )));
        }
        if self.compare_visible {
            if let Some(diff) = &self.model.read(cx).client.help_override {
                let lines: Vec<_> = diff.lines().map(str::to_owned).collect();
                body = body
                    .child(
                        gpui::uniform_list("editor-diff", lines.len(), move |range, _, _| {
                            range
                                .map(|i| div().h(px(24.)).child(lines[i].clone()))
                                .collect::<Vec<_>>()
                        })
                        .h(px(240.)),
                    )
                    .child(
                        Button::new("close-diff")
                            .label(t("Close comparison"))
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.compare_visible = false;
                                cx.notify();
                            })),
                    );
            }
        }
        if self.source_visible {
            body = body.child(
                CodeEditor::new(&self.source)
                    .h(px(520.))
                    .aria_label(t("TOML source")),
            );
            if let Some(error) = &self.source_error {
                body = body.child(
                    div()
                        .text_color(cx.theme().danger)
                        .child(error.message.clone()),
                );
            }
        } else {
            body = body.child(self.tree_toolbar(cx)).child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .gap_4()
                    .child(self.tree(cx))
                    .child(self.form(cx)),
            );
            match mode {
                Mode::Members {
                    options, chosen, ..
                } => body = body.child(self.members(options, chosen, cx)),
                Mode::NewOutbound(selected) => {
                    body = body.child(self.outbound_picker(selected, cx))
                }
                Mode::Header(_) | Mode::Search(_) => {
                    body = body.child(
                        Card::new(t(if matches!(mode, Mode::Search(_)) {
                            "Search"
                        } else {
                            "Name"
                        }))
                        .child(Input::new(&self.mode_input))
                        .child(
                            Button::new("editor-input-accept")
                                .primary()
                                .label(t("Confirm"))
                                .on_click(cx.listener(|view, _, window, cx| {
                                    view.edit(Editor::commit_input, cx);
                                    window.focus(&view.focus, cx);
                                })),
                        ),
                    )
                }
                Mode::Delete { .. } => {
                    body = body.child(
                        crate::widgets::Confirm::new(false)
                            .on_accept(cx.listener(|view, _, _, cx| {
                                view.edit(Editor::confirm_delete, cx);
                            }))
                            .on_cancel(cx.listener(|view, _, _, cx| {
                                view.edit(
                                    |editor| {
                                        editor.cancel_input();
                                        Ok(())
                                    },
                                    cx,
                                );
                            })),
                    )
                }
                _ => {}
            }
        }
        body.into_any_element()
    }
}
