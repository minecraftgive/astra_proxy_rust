use super::*;
use astra_app::schema::Type;
use astra_client::editor::FieldPresentation;
use gpui::AnyElement;
use gpui_component::{
    searchable_list::SearchableVec,
    select::{Select, SelectEvent, SelectState},
    switch::Switch,
};

#[derive(Clone)]
pub(super) struct Field {
    meta: FieldPresentation,
    input: Entity<InputState>,
    select: Option<Entity<SelectState<SearchableVec<String>>>>,
}
impl View {
    pub(super) fn sync_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let metadata = self
            .model
            .read(cx)
            .client
            .editor
            .as_ref()
            .and_then(|editor| editor.presentation().ok())
            .map(|p| p.fields)
            .unwrap_or_default();
        let signature = metadata
            .iter()
            .map(|field| {
                format!(
                    "{:?}:{}:{:?}:{:?}",
                    field.path,
                    field.spec.key,
                    std::mem::discriminant(&field.spec.kind),
                    field.suggestions
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        if signature == self.field_signature {
            for (field, meta) in self.fields.iter_mut().zip(metadata) {
                if !field.input.focus_handle(cx).is_focused(window)
                    && !matches!(meta.spec.kind, Type::StringList)
                    && field.input.read(cx).value().as_ref() != meta.value
                {
                    field.input.update(cx, |input, cx| {
                        input.set_value(meta.value.clone(), window, cx)
                    });
                }
                if let Some(select) = &field.select {
                    if field.meta.value != meta.value {
                        select.update(cx, |select, cx| {
                            select.set_selected_value(&meta.value, window, cx)
                        });
                    }
                }
                field.meta = meta;
            }
            return;
        }
        self.field_signature = signature;
        self.fields.clear();
        self.field_subscriptions.clear();
        self.field_error = None;
        for meta in metadata {
            let input = cx.new(|cx| InputState::new(window, cx).masked(meta.spec.secret()));
            if !matches!(meta.spec.kind, Type::StringList) {
                input.update(cx, |input, cx| {
                    input.set_value(meta.value.clone(), window, cx)
                });
            }
            let path = meta.path.clone();
            let key = meta.spec.key.clone();
            let list = matches!(meta.spec.kind, Type::StringList);
            self.field_subscriptions.push(cx.subscribe_in(&input, window, move |view, input, event, window, cx| {
                if list { return; }
                match event {
                    InputEvent::Focus => {
                        let pending = view.model.read(cx).client.editor.as_ref().and_then(|editor| match &editor.form.mode {
                            Mode::Field { key: current, .. } if editor.form.path != path || current != &key => Some((editor.form.path.clone(), current.clone())),
                            _ => None,
                        });
                        if let Some((pending_path, pending_key)) = pending {
                            if !view.commit_pending(window, cx) {
                                if let Some(field) = view.fields.iter().find(|field| field.meta.path == pending_path && field.meta.spec.key == pending_key) {
                                    field.input.update(cx, |input, cx| input.focus(window, cx));
                                }
                            }
                        }
                    }
                    InputEvent::Change => {
                        let value = input.read(cx).value().to_string();
                        view.edit(|editor| { editor.edit_field(path.clone(), key.clone(), value); Ok(()) }, cx);
                    }
                    InputEvent::PressEnter { .. } | InputEvent::Blur => {
                        let pending = view.model.read(cx).client.editor.as_ref().is_some_and(|editor| editor.form.path == path && matches!(&editor.form.mode, Mode::Field { key: current, .. } if current == &key));
                        if pending {
                            let ok = view.edit(Editor::commit_input, cx);
                            view.field_error = if ok { None } else { view.error.take().map(|error| (key.clone(), error)) };
                            if matches!(event, InputEvent::PressEnter { .. }) && ok { window.focus(&view.focus, cx); }
                        }
                    }
                }
            }));
            let select = if meta.suggestions.is_empty()
                || matches!(meta.spec.kind, Type::StringList)
            {
                None
            } else {
                let choices = meta.suggestions.clone();
                let select = cx.new(|cx| {
                    SelectState::new(SearchableVec::new(choices), None, window, cx).searchable(true)
                });
                select.update(cx, |state, cx| {
                    state.set_selected_value(&meta.value, window, cx)
                });
                let path = meta.path.clone();
                let key = meta.spec.key.clone();
                self.field_subscriptions.push(cx.subscribe_in(
                    &select,
                    window,
                    move |view, _, event, window, cx| {
                        if let SelectEvent::Confirm(Some(value)) = event {
                            if !view.commit_pending(window, cx) {
                                return;
                            }
                            let ok = view.edit(
                                |editor| editor.set_field(path.clone(), key.clone(), value.clone()),
                                cx,
                            );
                            view.field_error = if ok {
                                None
                            } else {
                                view.error.take().map(|error| (key.clone(), error))
                            };
                        }
                    },
                ));
                Some(select)
            };
            self.fields.push(Field {
                meta,
                input,
                select,
            });
        }
    }
    fn set_list(&mut self, field: &Field, values: Vec<String>, cx: &mut Context<Self>) {
        let ok = self.edit(
            |editor| {
                editor.set_list_values(field.meta.path.clone(), field.meta.spec.key.clone(), values)
            },
            cx,
        );
        self.field_error = if ok {
            None
        } else {
            self.error
                .take()
                .map(|error| (field.meta.spec.key.clone(), error))
        };
    }
    pub(super) fn form(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut form = div()
            .id("editor-form")
            .flex_1()
            .min_w_0()
            .h(px(470.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_3();
        for (index, field) in self.fields.iter().enumerate() {
            let meta = &field.meta;
            let mut row = div()
                .flex()
                .flex_col()
                .gap_1()
                .child(div().text_sm().child(format!(
                    "{}{}",
                    meta.spec.label,
                    if meta.spec.required { " *" } else { "" }
                )));
            if meta.members {
                let path = meta.path.clone();
                let key = meta.spec.key.clone();
                row = row.child(
                    Button::new(("editor-members", index))
                        .label(t("Choose members"))
                        .on_click(cx.listener(move |view, _, window, cx| {
                            if view.commit_pending(window, cx) {
                                view.edit(|editor| editor.open_field(path.clone(), &key), cx);
                                window.focus(&view.focus, cx);
                            }
                        })),
                );
            } else if matches!(meta.spec.kind, Type::Boolean) {
                let path = meta.path.clone();
                let key = meta.spec.key.clone();
                row = row.child(
                    Switch::new(("editor-switch", index))
                        .checked(meta.value == "true")
                        .on_click(cx.listener(move |view, value: &bool, window, cx| {
                            if !view.commit_pending(window, cx) {
                                return;
                            }
                            let ok = view.edit(
                                |editor| {
                                    editor.set_field(path.clone(), key.clone(), value.to_string())
                                },
                                cx,
                            );
                            view.field_error = if ok {
                                None
                            } else {
                                view.error.take().map(|error| (key.clone(), error))
                            };
                        })),
                );
            } else if matches!(meta.spec.kind, Type::StringList) {
                let list_field = field.clone();
                let weak = cx.entity().downgrade();
                let chips = gpui::uniform_list(
                    ("editor-list-values", index),
                    meta.list.len(),
                    move |range, _, _| {
                        range
                            .map(|item_index| {
                                let field = list_field.clone();
                                let weak = weak.clone();
                                div().h(px(34.)).child(
                                    Button::new(("editor-list-chip", item_index))
                                        .label(format!("{} ×", field.meta.list[item_index]))
                                        .on_click(move |_, window, cx| {
                                            let _ = weak.update(cx, |view, cx| {
                                                if !view.commit_pending(window, cx) {
                                                    return;
                                                }
                                                let mut values = field.meta.list.clone();
                                                values.remove(item_index);
                                                view.set_list(&field, values, cx);
                                            });
                                        }),
                                )
                            })
                            .collect::<Vec<_>>()
                    },
                )
                .h(px((meta.list.len().min(4) * 34) as f32));
                let field = field.clone();
                row = row.child(chips).child(
                    div().flex().gap_2().child(Input::new(&field.input)).child(
                        Button::new(("editor-list-add", index))
                            .label(t("Add"))
                            .on_click(cx.listener(move |view, _, window, cx| {
                                if !view.commit_pending(window, cx) {
                                    return;
                                }
                                let value = field.input.read(cx).value().trim().to_owned();
                                if !value.is_empty() {
                                    let mut values = field.meta.list.clone();
                                    values.push(value);
                                    view.set_list(&field, values, cx);
                                    field
                                        .input
                                        .update(cx, |input, cx| input.set_value("", window, cx));
                                }
                            })),
                    ),
                );
            } else if let Some(select) = &field.select {
                row = row.child(Select::new(select).placeholder(meta.spec.label.clone()));
                // References may accept an explicit tag outside the loaded catalogue.
                if !matches!(meta.spec.kind, Type::Choice(_)) {
                    row = row.child(Input::new(&field.input));
                }
            } else {
                row = row.child(Input::new(&field.input));
            }
            if !meta.spec.help.is_empty() {
                row = row.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(meta.spec.help.clone()),
                );
            }
            if let Some((key, error)) = &self.field_error {
                if key == &meta.spec.key {
                    row = row.child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().danger)
                            .child(error.clone()),
                    );
                }
            }
            form = form.child(row);
        }
        form.into_any_element()
    }
}
