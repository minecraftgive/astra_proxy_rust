use crate::{
    model::AppModel,
    views::ViewEvent,
    widgets::{Card, Status, StatusDot},
};
use astra_client::{
    i18n::t,
    intents::{InstanceSummary, ProfileAction},
    protocol::Kind,
    state::{Client, Input as ClientInput, Operation},
};
use gpui::{
    div, px, uniform_list, AnyElement, App, AppContext, Context, Entity, EventEmitter, FocusHandle,
    Focusable, InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render,
    StatefulInteractiveElement, Styled, Subscription, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants},
    input::{Input, InputEvent, InputState},
    menu::{ContextMenuExt, DropdownMenu, PopupMenuItem},
    ActiveTheme,
};
use std::sync::Arc;

/// Instance table and startup bindings share the same Client selection.
pub struct View {
    model: Entity<AppModel>,
    focus: FocusHandle,
    search: Entity<InputState>,
    observations: Vec<Subscription>,
    scroll: gpui::UniformListScrollHandle,
}
impl EventEmitter<ViewEvent> for View {}
impl Focusable for View {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl View {
    fn dispatch(
        &mut self,
        action: impl FnOnce(&mut Client) -> ClientInput,
        cx: &mut Context<Self>,
    ) {
        let input = self.model.update(cx, |model, cx| {
            let input = model.tokio(|client| {
                client.page = astra_client::navigation::PROFILES;
                action(client)
            });
            cx.notify();
            input
        });
        let client = &self.model.read(cx).client;
        let selected = client.selected().map(|row| row.instance.name.as_str());
        let index = client
            .profile_rows(self.search.read(cx).value().as_ref())
            .iter()
            .position(|row| Some(row.instance.name.as_str()) == selected);
        if let Some(index) = index {
            self.scroll
                .scroll_to_item(index, gpui::ScrollStrategy::Nearest);
        }
        let page = self.model.read(cx).client.page;
        if page != astra_client::navigation::PROFILES {
            cx.emit(ViewEvent::Navigate(page));
        }
        cx.emit(ViewEvent::Input(input));
        cx.notify();
    }
}

impl View {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(t("Find")));
        let observations = vec![
            cx.observe(&model, |_, _, cx| cx.notify()),
            cx.subscribe(&search, |_, _, _: &InputEvent, cx| cx.notify()),
        ];
        Self {
            model,
            focus: cx.focus_handle(),
            search,
            observations,
            scroll: gpui::UniformListScrollHandle::new(),
        }
    }
    pub fn action(&mut self, action: ProfileAction, cx: &mut Context<Self>) {
        self.dispatch(|client| client.profile_action(action), cx);
    }
    fn binding(&mut self, action: astra_client::intents::BindingAction, cx: &mut Context<Self>) {
        self.dispatch(|client| client.binding_action(action), cx);
    }
    fn select(&mut self, kind: Kind, name: String, window: &mut Window, cx: &mut Context<Self>) {
        self.dispatch(
            |client| {
                client.select_instance(kind, &name);
                ClientInput::Idle
            },
            cx,
        );
        window.focus(&self.focus, cx);
    }
    fn key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.focus.is_focused(window)
            || event.keystroke.modifiers.control
            || event.keystroke.modifiers.alt
            || event.keystroke.modifiers.platform
        {
            return;
        }
        let Some(key) = crate::keys::from_keystroke(&event.keystroke, event.is_held) else {
            return;
        };
        use astra_client::input::KeyCode;
        if matches!(key.code, KeyCode::Up | KeyCode::Down) {
            self.dispatch(
                |client| {
                    client.move_instance_selection(if key.code == KeyCode::Up { -1 } else { 1 });
                    ClientInput::Idle
                },
                cx,
            );
            cx.stop_propagation();
            return;
        }
        let action = match key.code {
            KeyCode::Char('n') | KeyCode::Insert => ProfileAction::Create,
            KeyCode::Char('c') => ProfileAction::Toggle,
            KeyCode::Char('s') => ProfileAction::Operation(Operation::Start),
            KeyCode::Char('x') => ProfileAction::Operation(Operation::Stop),
            KeyCode::Char('r') => ProfileAction::Operation(Operation::Reload),
            KeyCode::Char('e') => ProfileAction::Operation(Operation::Edit),
            KeyCode::Char('L') => ProfileAction::Operation(Operation::Logs),
            KeyCode::Char('t') => ProfileAction::Inspect,
            KeyCode::Char('b') => ProfileAction::Operation(Operation::Bind),
            KeyCode::Char('d') | KeyCode::Delete => ProfileAction::Operation(Operation::Remove),
            KeyCode::Char('y') => ProfileAction::Operation(Operation::Copy),
            KeyCode::Char('E') => ProfileAction::Operation(Operation::ExportConfig),
            KeyCode::Char('P') => ProfileAction::Operation(Operation::ExportTerminal),
            KeyCode::Char('v') => ProfileAction::Validate,
            _ => return,
        };
        if event.is_held {
            cx.stop_propagation();
            return;
        }
        self.action(action, cx);
        cx.stop_propagation();
    }
    fn bindings(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let model = self.model.read(cx);
        let picker = model.client.binding_picker.as_ref()?;
        let choices: Arc<[_]> = picker
            .choices
            .iter()
            .enumerate()
            .map(|(index, (name, _))| (index, name.clone(), picker.targets.contains(name)))
            .collect::<Vec<_>>()
            .into();
        let entity = cx.entity().downgrade();
        let list = uniform_list("bindings-list", choices.len(), move |range, _, _| {
            range
                .map(|index| {
                    let (index, name, selected) = &choices[index];
                    let index = *index;
                    let entity = entity.clone();
                    div().h(px(38.)).child(
                        Button::new(("binding", index))
                            .label(format!("{} {name}", if *selected { "☑" } else { "☐" }))
                            .on_click(move |_, _, cx| {
                                let _ = entity.update(cx, |view, cx| {
                                    view.binding(
                                        astra_client::intents::BindingAction::ToggleAt(index),
                                        cx,
                                    )
                                });
                            }),
                    )
                })
                .collect::<Vec<_>>()
        })
        .h(px(240.));
        Some(
            Card::new(format!("{} · {}", t("Startup bindings"), picker.name))
                .child(list)
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(Button::new("binding-cancel").label(t("Cancel")).on_click(
                            cx.listener(|view, _, _, cx| {
                                view.binding(astra_client::intents::BindingAction::Cancel, cx)
                            }),
                        ))
                        .child(
                            Button::new("binding-save")
                                .primary()
                                .label(t("Confirm"))
                                .on_click(cx.listener(|view, _, _, cx| {
                                    view.binding(astra_client::intents::BindingAction::Confirm, cx)
                                })),
                        ),
                )
                .into_any_element(),
        )
    }
}
impl Render for View {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _ = &self.observations;
        let client = &self.model.read(cx).client;
        let kind = client.kind;
        let selected = client.selected().map(|row| row.instance.name.clone());
        let rows: Arc<[_]> = client
            .profile_rows(self.search.read(cx).value().as_ref())
            .into();
        let row_count = rows.len();
        let summary = client.selected_summary();
        let operations = client.operations();
        let mut tabs = div().flex().gap_2();
        for (index, (item, label)) in [
            (Kind::Proxy, "Proxy"),
            (Kind::Rule, "Rules"),
            (Kind::Server, "Server"),
        ]
        .into_iter()
        .enumerate()
        {
            let mut button = Button::new(("instance-kind", index)).label(t(label));
            if kind == item {
                button = button.primary();
            }
            tabs = tabs.child(button.on_click(cx.listener(move |view, _, _, cx| {
                view.dispatch(
                    |client| {
                        client.switch_core(item);
                        ClientInput::Idle
                    },
                    cx,
                )
            })));
        }
        let mut toolbar = div().flex().flex_wrap().gap_2().child(
            Button::new("new-instance")
                .primary()
                .label(t("New instance"))
                .on_click(cx.listener(|view, _, _, cx| view.action(ProfileAction::Create, cx))),
        );
        // Everyday lifecycle actions stay visible; the rest go in a "More" menu,
        // like the web UI's page tools, so the toolbar fits on one line.
        let everyday = |operation: Operation| {
            matches!(
                operation,
                Operation::Start
                    | Operation::Stop
                    | Operation::Restart
                    | Operation::Reload
                    | Operation::Edit
                    | Operation::Logs
            )
        };
        for (index, operation) in operations.iter().copied().enumerate() {
            if everyday(operation) {
                toolbar = toolbar.child(
                    Button::new(("instance-operation", index))
                        .label(operation.label())
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.action(ProfileAction::Operation(operation), cx)
                        })),
                );
            }
        }
        let more: Vec<Operation> = operations
            .iter()
            .copied()
            .filter(|operation| !everyday(*operation))
            .collect();
        let validate = summary.is_some();
        if !more.is_empty() || validate {
            let entity = cx.entity().downgrade();
            toolbar = toolbar.child(
                Button::new("instance-more")
                    .label(format!("{} ▾", t("More")))
                    .dropdown_menu(move |mut menu, _, _| {
                        for operation in more.iter().copied() {
                            let entity = entity.clone();
                            menu = menu.item(PopupMenuItem::new(operation.label()).on_click(
                                move |_, _, cx| {
                                    let _ = entity.update(cx, |view, cx| {
                                        view.action(ProfileAction::Operation(operation), cx)
                                    });
                                },
                            ));
                        }
                        if validate {
                            let entity = entity.clone();
                            menu = menu.item(
                                PopupMenuItem::new(t("Validate configuration")).on_click(
                                    move |_, _, cx| {
                                        let _ = entity.update(cx, |view, cx| {
                                            view.action(ProfileAction::Validate, cx)
                                        });
                                    },
                                ),
                            );
                        }
                        menu
                    }),
            );
        }
        let entity = cx.entity().downgrade();
        let theme = cx.theme().clone();
        let pal = crate::theme::palette(cx);
        let list = uniform_list("profile-table", rows.len(), move |range, _, _| {
            range
                .map(|index| {
                    let row = rows[index].clone();
                    let name = row.instance.name.clone();
                    let select_name = name.clone();
                    let menu_name = name.clone();
                    let entity = entity.clone();
                    let select_entity = entity.clone();
                    let menu_entity = entity.clone();
                    let selected = selected.as_ref() == Some(&name);
                    let operations = astra_client::intents::row_operations(&row);
                    div()
                        .id(("profile-row", index))
                        .w_full()
                        .h(px(48.))
                        .flex()
                        .items_center()
                        .gap_3()
                        .px_3()
                        .border_b_1()
                        .border_color(theme.border)
                        .bg(if selected {
                            pal.accent_soft
                        } else {
                            pal.surface
                        })
                        .child(div().w(px(180.)).child(name))
                        .child(StatusDot::new(status(
                            astra_client::overview::indicator_state(&row),
                        )))
                        .child(
                            div()
                                .flex_1()
                                .text_sm()
                                .child(row.instance.config.display().to_string()),
                        )
                        .child(
                            div()
                                .w(px(100.))
                                .child(astra_client::format::duration(row.uptime)),
                        )
                        .on_click(move |_, window, cx| {
                            let _ = select_entity.update(cx, |view, cx| {
                                view.select(kind, select_name.clone(), window, cx)
                            });
                        })
                        .context_menu(move |mut menu, _, _| {
                            for operation in operations.iter().copied() {
                                let entity = menu_entity.clone();
                                let name = menu_name.clone();
                                menu = menu.item(PopupMenuItem::new(operation.label()).on_click(
                                    move |_, _, cx| {
                                        let _ = entity.update(cx, |view, cx| {
                                            view.dispatch(
                                                |client| {
                                                    client.named_profile_action(
                                                        kind,
                                                        &name,
                                                        ProfileAction::Operation(operation),
                                                    )
                                                },
                                                cx,
                                            )
                                        });
                                    },
                                ));
                            }
                            menu
                        })
                })
                .collect::<Vec<_>>()
        })
        .track_scroll(&self.scroll)
        // Grow with the profiles, up to six rows, instead of a fixed empty block.
        .h(px(48. * row_count.clamp(1, 6) as f32));
        let mut content = div()
            .id("profiles-view")
            .key_context("AstraProfiles")
            .track_focus(&self.focus)
            .flex()
            .flex_col()
            .gap_3()
            .size_full()
            .min_h_0()
            .overflow_y_scroll()
            .on_key_down(cx.listener(Self::key))
            .child(tabs)
            .child(Input::new(&self.search))
            .child(toolbar)
            .child(
                div()
                    .flex_none()
                    .rounded_lg()
                    .border_1()
                    .border_color(pal.border)
                    .bg(pal.surface)
                    .overflow_hidden()
                    .child(list),
            );
        if let Some(summary) = summary {
            content = content.child(instance_card(summary, cx));
        }
        if let Some(bindings) = self.bindings(cx) {
            content = content.child(bindings);
        }
        content
    }
}
pub(crate) fn status(state: astra_client::overview::InstanceState) -> Status {
    use astra_client::overview::InstanceState;
    match state {
        InstanceState::Running => Status::Running,
        InstanceState::Stopped => Status::Stopped,
        InstanceState::Starting => Status::Starting,
        InstanceState::Error => Status::Error,
    }
}
pub(super) fn instance_card(summary: InstanceSummary, cx: &App) -> AnyElement {
    let mut card = Card::new(summary.row.instance.name.clone())
        .child(StatusDot::new(status(summary.state)))
        .child(div().text_sm().child(format!(
            "{}: {}",
            t("Configuration path"),
            summary.row.instance.config.display()
        )))
        .child(div().text_sm().child(format!(
                "PID: {} · {}",
                summary
                    .row
                    .pid
                    .map_or_else(|| "—".into(), |pid| pid.to_string()),
                astra_client::format::duration(summary.row.uptime)
            )));
    if let Some(details) = summary.details {
        card = card
            .child(div().text_sm().child(format!(
                "{}: {}",
                t("Log level"),
                details.log_level.as_str().to_uppercase()
            )))
            .child(div().text_sm().child(format!(
                "{}: {}",
                t("Startup bindings"),
                details.bindings.join(", ")
            )));
        for listener in details.listeners {
            card = card.child(div().text_sm().child(listener));
        }
    }
    for warning in summary.warnings {
        card = card.child(
            div()
                .text_sm()
                .text_color(cx.theme().warning)
                .child(warning),
        );
    }
    card.into_any_element()
}

#[cfg(test)]
#[path = "../test/native_pages_test.rs"]
mod tests;
