use super::{Card, Confirm};
use crate::model::AppModel;
use astra_client::{
    i18n::t,
    node_transfer::{self, PickerKind, TransferAction},
    prompts::PromptPathKind,
    state::{Input as ClientInput, Prompt},
};
use gpui::{
    div, px, uniform_list, AnyElement, App, AppContext, Context, Entity, EventEmitter, FocusHandle,
    Global, InteractiveElement, IntoElement, KeyBinding, KeyDownEvent, ParentElement,
    PathPromptOptions, Render, Styled, Subscription, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants},
    input::{Input, InputEvent, InputState, Textarea, TextareaState},
    ActiveTheme,
};
use std::{path::PathBuf, sync::Arc};

gpui::actions!(
    astra_prompt,
    [PickerPrevious, PickerNext, PickerToggle, ConfirmationToggle]
);
struct Bindings;
impl Global for Bindings {}
fn init(cx: &mut App) {
    if cx.has_global::<Bindings>() {
        return;
    }
    cx.set_global(Bindings);
    // The descendant predicate wins at the input's depth without changing
    // ordinary text fields or intercepting spaces typed into picker searches.
    let picker = Some("AstraPicker > Input || (AstraPicker && !Input)");
    cx.bind_keys([
        KeyBinding::new("up", PickerPrevious, picker),
        KeyBinding::new("down", PickerNext, picker),
        KeyBinding::new(
            "tab",
            ConfirmationToggle,
            Some("AstraConfirmation && !Input"),
        ),
        KeyBinding::new(
            "shift-tab",
            ConfirmationToggle,
            Some("AstraConfirmation && !Input"),
        ),
        KeyBinding::new(
            "left",
            ConfirmationToggle,
            Some("AstraConfirmation && !Input"),
        ),
        KeyBinding::new(
            "right",
            ConfirmationToggle,
            Some("AstraConfirmation && !Input"),
        ),
        KeyBinding::new("space", PickerToggle, Some("AstraPicker && !Input")),
    ]);
}

#[derive(Clone)]
pub enum PromptEvent {
    Input(ClientInput),
    Cancelled,
}

pub struct PromptHost {
    model: Entity<AppModel>,
    focus: FocusHandle,
    input: Entity<InputState>,
    multiline: Entity<TextareaState>,
    search: Entity<InputState>,
    signature: Option<String>,
    generation: u64,
    error: Option<String>,
    _observations: Vec<Subscription>,
}
impl EventEmitter<PromptEvent> for PromptHost {}

impl PromptHost {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        init(cx);
        let input = cx.new(|cx| InputState::new(window, cx));
        let multiline = cx.new(|cx| TextareaState::new(window, cx).submit_on_enter(true));
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(t("Search")));
        let mut host = Self {
            model: model.clone(),
            focus: cx.focus_handle(),
            input: input.clone(),
            multiline: multiline.clone(),
            search: search.clone(),
            signature: None,
            generation: 0,
            error: None,
            _observations: vec![],
        };
        host._observations = vec![
            cx.observe_in(&model, window, |host, _, window, cx| {
                host.reconcile(window, cx);
                cx.notify();
            }),
            cx.subscribe_in(
                &input,
                window,
                |host, state, event, window, cx| match event {
                    InputEvent::Change => host.edit(state.read(cx).value().to_string(), cx),
                    InputEvent::PressEnter { .. } => host.submit(false, window, cx),
                    _ => {}
                },
            ),
            cx.subscribe_in(
                &multiline,
                window,
                |host, state, event, window, cx| match event {
                    InputEvent::Change => host.edit(state.read(cx).value().to_string(), cx),
                    InputEvent::PressEnter { .. } => host.submit(false, window, cx),
                    _ => {}
                },
            ),
            cx.subscribe_in(&search, window, |host, _, event, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    host.submit(false, window, cx);
                }
                cx.notify();
            }),
        ];
        host.reconcile(window, cx);
        host
    }

    fn reconcile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model.read(cx);
        let signature = model.client.prompt.as_ref().map(prompt_signature);
        let input = model.client.input.clone();
        let changed = signature != self.signature;
        if changed {
            self.signature = signature;
            self.generation = self.generation.wrapping_add(1);
            self.error = None;
            self.search
                .update(cx, |state, cx| state.set_value("", window, cx));
        }
        if self.input.read(cx).value().as_ref() != input {
            self.input
                .update(cx, |state, cx| state.set_value(input.clone(), window, cx));
        }
        if self.multiline.read(cx).value().as_ref() != input {
            self.multiline
                .update(cx, |state, cx| state.set_value(input, window, cx));
        }
        if changed {
            if let Some(prompt) = self.model.read(cx).client.prompt.as_ref() {
                if prompt.picker() {
                    self.search.update(cx, |state, cx| state.focus(window, cx));
                } else if prompt.confirmation() {
                    window.focus(&self.focus, cx);
                } else if is_multiline(prompt) {
                    self.multiline
                        .update(cx, |state, cx| state.focus(window, cx));
                } else {
                    self.input.update(cx, |state, cx| state.focus(window, cx));
                }
            }
        }
    }

    fn edit(&mut self, value: String, cx: &mut Context<Self>) {
        self.error = None;
        self.model.update(cx, |model, cx| {
            model.client.input_cursor = value.chars().count();
            model.client.input = value;
            cx.notify();
        });
        cx.notify();
    }
    fn submit(&mut self, accepted: bool, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model.read(cx);
        let Some(prompt) = model.client.prompt.as_ref() else {
            return;
        };
        if prompt.confirmation() && !accepted {
            self.cancel(window, cx);
            return;
        }
        let value = if prompt.confirmation() {
            "yes".to_owned()
        } else {
            model.client.input.clone()
        };
        let result = self.model.update(cx, |model, cx| {
            let result = model.client.submit_prompt(value);
            cx.notify();
            result
        });
        match result {
            Ok(input) => {
                self.error = None;
                cx.emit(PromptEvent::Input(input));
            }
            Err(error) => self.error = Some(format!("{:#}", error.error)),
        }
        self.reconcile(window, cx);
        cx.notify();
    }
    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| {
            model.client.cancel_prompt();
            cx.notify();
        });
        self.error = None;
        self.reconcile(window, cx);
        cx.emit(PromptEvent::Cancelled);
        cx.notify();
    }
    fn transfer(&mut self, action: TransferAction, window: &mut Window, cx: &mut Context<Self>) {
        self.error = None;
        self.model.update(cx, |model, cx| {
            model.client.node_transfer(action);
            cx.notify();
        });
        window.focus(&self.focus, cx);
        cx.notify();
    }
    fn move_picker(&mut self, direction: isize, cx: &mut Context<Self>) {
        let model = self.model.read(cx);
        let Some(picker) = model.client.prompt.as_ref().and_then(node_transfer::picker) else {
            return;
        };
        let query = self.search.read(cx).value().to_lowercase();
        let indices = picker
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.to_lowercase().contains(&query))
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if indices.is_empty() {
            return;
        }
        let current = indices.iter().position(|index| *index == picker.cursor);
        let next = current
            .map(|index| {
                index
                    .saturating_add_signed(direction)
                    .min(indices.len() - 1)
            })
            .unwrap_or(if direction < 0 { indices.len() - 1 } else { 0 });
        let action = match picker.kind {
            PickerKind::Import => TransferAction::SetCursor(indices[next]),
            PickerKind::Export => TransferAction::SetDestination(indices[next] == 1),
        };
        self.model.update(cx, |model, cx| {
            model.client.node_transfer(action);
            cx.notify();
        });
        cx.notify();
    }

    fn browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model.read(cx);
        let Some(kind) = model.client.prompt.as_ref().and_then(Prompt::path_kind) else {
            return;
        };
        let directory = model.client.working_dir.clone();
        let current = PathBuf::from(&model.client.input);
        let generation = self.generation;
        match kind {
            PromptPathKind::SaveFile => {
                let parent = current
                    .parent()
                    .filter(|path| path.is_absolute())
                    .unwrap_or(&directory);
                let name = current.file_name().and_then(|name| name.to_str());
                let choice = cx.prompt_for_new_path(parent, name);
                cx.spawn_in(window, async move |host, cx| {
                    let result = match choice.await {
                        Ok(result) => result.map_err(|error| error.to_string()),
                        Err(error) => Err(error.to_string()),
                    };
                    let _ = host.update_in(cx, |host, window, cx| {
                        host.accept_path(generation, result, window, cx)
                    });
                })
                .detach();
            }
            PromptPathKind::OpenFile | PromptPathKind::OpenFileOrDirectory => {
                let choice = cx.prompt_for_paths(PathPromptOptions {
                    files: true,
                    directories: matches!(kind, PromptPathKind::OpenFileOrDirectory),
                    multiple: false,
                    prompt: Some(t("Browse…").into()),
                });
                cx.spawn_in(window, async move |host, cx| {
                    let result = match choice.await {
                        Ok(result) => result
                            .map(|paths| paths.and_then(|paths| paths.into_iter().next()))
                            .map_err(|error| error.to_string()),
                        Err(error) => Err(error.to_string()),
                    };
                    let _ = host.update_in(cx, |host, window, cx| {
                        host.accept_path(generation, result, window, cx)
                    });
                })
                .detach();
            }
        }
    }
    fn accept_path(
        &mut self,
        generation: u64,
        result: Result<Option<PathBuf>, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.reconcile(window, cx);
        if generation != self.generation || self.model.read(cx).client.prompt.is_none() {
            return;
        }
        match result {
            Ok(Some(path)) => self.edit(path.to_string_lossy().into_owned(), cx),
            Ok(None) => {}
            Err(error) => {
                self.error = Some(astra_client::tr!("Cannot open file chooser: {}", error))
            }
        }
        self.reconcile(window, cx);
        cx.notify();
    }

    fn picker(&self, picker: node_transfer::Picker, cx: &mut Context<Self>) -> AnyElement {
        let query = self.search.read(cx).value().to_lowercase();
        let items: Arc<[(usize, String)]> = picker
            .items
            .into_iter()
            .enumerate()
            .filter(|(_, label)| label.to_lowercase().contains(&query))
            .collect::<Vec<_>>()
            .into();
        let count = items.len();
        let kind = picker.kind;
        let selected = picker.cursor;
        let entity = cx.entity().downgrade();
        let list = uniform_list("prompt-choices", count, move |range, _, _| {
            range
                .map(|index| {
                    let (original, label) = &items[index];
                    let original = *original;
                    let entity = entity.clone();
                    let mut button = Button::new(("prompt-choice", original)).label(label.clone());
                    if original == selected {
                        button = button.primary();
                    }
                    div()
                        .h(px(38.))
                        .child(button.on_click(move |_, window, cx| {
                            let action = match kind {
                                PickerKind::Import => TransferAction::ToggleIndex(original),
                                PickerKind::Export => TransferAction::SetDestination(original == 1),
                            };
                            let _ = entity.update(cx, |host, cx| host.transfer(action, window, cx));
                        }))
                })
                .collect::<Vec<_>>()
        })
        .h(px(240.));
        let mut content = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(Input::new(&self.search))
            .child(list);
        if matches!(kind, PickerKind::Import) {
            content = content.child(
                Button::new("prompt-toggle-all")
                    .label(t("Select all / Clear selection"))
                    .on_click(cx.listener(|host, _, window, cx| {
                        host.transfer(TransferAction::ToggleAll, window, cx)
                    })),
            );
        }
        if !picker.warnings.is_empty() {
            content = content.child(div().text_sm().child(picker.warnings));
        }
        content.into_any_element()
    }
}

fn is_multiline(prompt: &Prompt) -> bool {
    matches!(
        prompt,
        Prompt::Import | Prompt::ImportNode(_) | Prompt::ImportRule(_)
    )
}
/// Selection changes belong to the same opening and must retain its search text.
fn prompt_signature(prompt: &Prompt) -> String {
    let mut prompt = prompt.clone();
    match &mut prompt {
        Prompt::ImportNodeSelection {
            cursor, selected, ..
        } => {
            *cursor = 0;
            selected.clear();
        }
        Prompt::ExportNodeDestination { clipboard, .. } => *clipboard = false,
        _ => {}
    }
    serde_json::to_string(&prompt).expect("Prompt serialization is infallible")
}

impl Render for PromptHost {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let model = self.model.read(cx);
        let Some(prompt) = model.client.prompt.clone() else {
            return div().into_any_element();
        };
        let accepted = model.client.confirmation_accept;
        let picker = node_transfer::picker(&prompt);
        let title = picker
            .as_ref()
            .map(|picker| picker.title.clone())
            .unwrap_or_else(|| prompt.title().to_owned());
        let mut body = Card::new(title);
        if let Some(description) = prompt.body() {
            body = body.child(div().text_sm().child(description));
        }
        if let Some(picker) = picker {
            body = body.child(self.picker(picker, cx));
        } else if !prompt.confirmation() {
            body = if is_multiline(&prompt) {
                body.child(Textarea::new(&self.multiline).h(px(140.)))
            } else {
                body.child(Input::new(&self.input))
            };
            if prompt.path_kind().is_some() {
                body = body.child(
                    Button::new("prompt-browse")
                        .label(t("Browse…"))
                        .on_click(cx.listener(|host, _, window, cx| host.browse(window, cx))),
                );
            }
        }
        if let Some(error) = &self.error {
            body = body.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(error.clone()),
            );
        }
        body = body.child(
            Confirm::new(if prompt.confirmation() {
                accepted
            } else {
                true
            })
            .on_accept(cx.listener(|host, _, window, cx| host.submit(true, window, cx)))
            .on_cancel(cx.listener(|host, _, window, cx| host.cancel(window, cx))),
        );
        div()
            .id("astra-prompt")
            .key_context(if prompt.picker() {
                "AstraDialog AstraPicker"
            } else if prompt.confirmation() {
                "AstraDialog AstraConfirmation"
            } else {
                "AstraDialog"
            })
            .track_focus(&self.focus)
            .w_full()
            .on_action(cx.listener(|host, _: &ConfirmationToggle, window, cx| {
                host.model.update(cx, |model, cx| {
                    model.client.confirmation_accept = !model.client.confirmation_accept;
                    cx.notify();
                });
                window.focus(&host.focus, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|host, _: &PickerPrevious, _, cx| {
                host.move_picker(-1, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|host, _: &PickerNext, _, cx| {
                host.move_picker(1, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|host, _: &PickerToggle, window, cx| {
                host.transfer(TransferAction::Toggle, window, cx);
                cx.stop_propagation();
            }))
            .on_key_down(cx.listener(|host, event: &KeyDownEvent, window, cx| {
                match event.keystroke.key.as_str() {
                    "escape" => host.cancel(window, cx),
                    "enter" => {
                        let accepted = host.model.read(cx).client.confirmation_accept;
                        host.submit(accepted, window, cx);
                    }
                    "tab" | "left" | "right"
                        if host
                            .model
                            .read(cx)
                            .client
                            .prompt
                            .as_ref()
                            .is_some_and(Prompt::confirmation) =>
                    {
                        host.model.update(cx, |model, cx| {
                            model.client.confirmation_accept = !model.client.confirmation_accept;
                            cx.notify();
                        });
                    }
                    _ => return,
                }
                cx.stop_propagation();
            }))
            .child(body)
            .into_any_element()
    }
}

#[cfg(test)]
#[path = "prompt_test.rs"]
mod tests;
