use crate::{model::AppModel, views::ViewEvent, widgets::Card};
use astra_client::{
    connections::ConnectionRowKey,
    i18n::t,
    state::{Client, Input as ClientInput},
};
use gpui::{
    div, prelude::FluentBuilder, px, uniform_list, App, AppContext, Context, Entity, EventEmitter,
    FocusHandle, Focusable, InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render,
    StatefulInteractiveElement, Styled, Subscription, Window,
};
use gpui_component::{
    button::Button,
    input::{Input, InputEvent, InputState},
    menu::{ContextMenuExt, PopupMenuItem},
    ActiveTheme,
};
use std::sync::Arc;

pub struct View {
    model: Entity<AppModel>,
    focus: FocusHandle,
    filter: Entity<InputState>,
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
                client.page = astra_client::navigation::CONNECTIONS;
                action(client)
            });
            cx.notify();
            input
        });
        let index = self.model.read(cx).client.selected_connection_row_index();
        if let Some(index) = index {
            self.scroll
                .scroll_to_item(index, gpui::ScrollStrategy::Nearest);
        }
        let page = self.model.read(cx).client.page;
        if page != astra_client::navigation::CONNECTIONS {
            cx.emit(ViewEvent::Navigate(page));
        }
        cx.emit(ViewEvent::Input(input));
        cx.notify();
    }
}

impl View {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let value = model.read(cx).client.connection_filter.clone();
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder(t("Filter")));
        filter.update(cx, |input, cx| input.set_value(value, window, cx));
        let observations = vec![
            cx.observe_in(&model, window, |view, _, window, cx| {
                let value = view.model.read(cx).client.connection_filter.clone();
                if view.filter.read(cx).value().as_ref() != value {
                    view.filter
                        .update(cx, |input, cx| input.set_value(value, window, cx));
                }
                cx.notify();
            }),
            cx.subscribe_in(
                &filter,
                window,
                |view, state, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::Change) {
                        let query = state.read(cx).value().to_string();
                        view.dispatch(
                            |client| {
                                client.filter_connections(query);
                                ClientInput::Idle
                            },
                            cx,
                        );
                    } else if matches!(event, InputEvent::PressEnter { .. }) {
                        window.focus(&view.focus, cx);
                    }
                    cx.notify();
                },
            ),
        ];
        Self {
            model,
            focus: cx.focus_handle(),
            filter,
            observations,
            scroll: gpui::UniformListScrollHandle::new(),
        }
    }
    pub fn close(
        &mut self,
        target: astra_client::live::ConnectionActionTarget,
        cx: &mut Context<Self>,
    ) {
        self.dispatch(|client| client.close_connection_target(target), cx);
    }
    fn select(&mut self, key: ConnectionRowKey, window: &mut Window, cx: &mut Context<Self>) {
        self.dispatch(|client| client.show_connection(key), cx);
        window.focus(&self.focus, cx);
    }
    fn key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.modifiers.control
            || event.keystroke.modifiers.alt
            || event.keystroke.modifiers.platform
        {
            return;
        }
        if !self.focus.is_focused(window) {
            if event.keystroke.key == "escape" {
                window.focus(&self.focus, cx);
                cx.stop_propagation();
            }
            return;
        }
        match event.keystroke.key.as_str() {
            "up" => self.dispatch(
                |client| {
                    client.move_connection_selection(-1);
                    ClientInput::Idle
                },
                cx,
            ),
            "down" => self.dispatch(
                |client| {
                    client.move_connection_selection(1);
                    ClientInput::Idle
                },
                cx,
            ),
            "enter" if !event.is_held => self.dispatch(Client::activate_connection_selection, cx),
            "x" if !event.is_held && event.keystroke.modifiers.shift => {
                self.dispatch(Client::close_all_connections, cx)
            }
            "x" | "delete" if !event.is_held => {
                self.dispatch(Client::close_selected_connection, cx)
            }
            "space" if !event.is_held => self.dispatch(Client::toggle_live_pause, cx),
            "/" => self.filter.update(cx, |input, cx| input.focus(window, cx)),
            "escape" => self.dispatch(
                |client| {
                    client.close_connection_detail();
                    ClientInput::Idle
                },
                cx,
            ),
            _ => return,
        }
        cx.stop_propagation();
    }
}
impl Render for View {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _ = &self.observations;
        let client = &self.model.read(cx).client;
        let rows: Arc<[_]> = client
            .connection_rows_at(astra_client::navigation::CONNECTIONS)
            .into_iter()
            .map(|row| {
                let target = client.connection_action_target(&row.key);
                (row, target)
            })
            .collect::<Vec<_>>()
            .into();
        let selected = client.selected_connection_row_key_at(astra_client::navigation::CONNECTIONS);
        let badges = client.connection_badges();
        let paused = client.connections_paused();
        let detail_target = client
            .selected_connection_row_key_at(astra_client::navigation::CONNECTIONS)
            .and_then(|key| client.connection_action_target(&key));
        let detail = client
            .live_detail_open()
            .then(|| {
                client
                    .selected_connection_at(astra_client::navigation::CONNECTIONS)
                    .cloned()
            })
            .flatten();
        let toolbar = div()
            .flex()
            .gap_2()
            .items_center()
            .child(div().flex_1().child(Input::new(&self.filter)))
            .child(
                Button::new("connections-pause")
                    .label(if paused { t("Resume") } else { t("Pause") })
                    .on_click(
                        cx.listener(|view, _, _, cx| view.dispatch(Client::toggle_live_pause, cx)),
                    ),
            )
            .child(
                Button::new("connections-close-all")
                    .label(t("Close all connections"))
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.dispatch(Client::close_all_connections, cx)
                    })),
            );
        let entity = cx.entity().downgrade();
        let theme = cx.theme().clone();
        let pal = crate::theme::palette(cx);
        let list = uniform_list("connections-table", rows.len(), move |range, _, _| {
            range
                .map(|index| {
                    let (row, target) = &rows[index];
                    let key = row.key.clone();
                    let select_key = key.clone();
                    let menu_target = target.clone();
                    let row_entity = entity.clone();
                    let menu_entity = entity.clone();
                    let is_group = row.is_group();
                    let chosen = selected.as_ref() == Some(&key);
                    let c = &row.connection;
                    div()
                        .id(("connection", index))
                        .w_full()
                        .h(px(44.))
                        .flex()
                        .items_center()
                        .gap_3()
                        .px_3()
                        .border_b_1()
                        .border_color(theme.border)
                        .bg(if chosen {
                            pal.accent_soft
                        } else if row.fresh {
                            pal.ok_soft
                        } else {
                            pal.surface
                        })
                        .child(div().w(px(280.)).child(row.label()))
                        .child(div().w(px(70.)).text_sm().child(c.network.clone()))
                        .child(div().w(px(140.)).text_sm().child(c.outbound.clone()))
                        .child(
                            div()
                                .w(px(90.))
                                .text_sm()
                                .child(astra_client::live::bytes(c.upload)),
                        )
                        .child(
                            div()
                                .w(px(90.))
                                .text_sm()
                                .child(astra_client::live::bytes(c.download)),
                        )
                        .child(
                            div()
                                .w(px(80.))
                                .text_sm()
                                .child(astra_client::format::duration(c.elapsed_ms / 1000)),
                        )
                        .on_click(move |_, window, cx| {
                            let _ = row_entity
                                .update(cx, |view, cx| view.select(select_key.clone(), window, cx));
                        })
                        .context_menu(move |menu, _, _| {
                            let entity = menu_entity.clone();
                            let target = menu_target.clone();
                            menu.item(
                                PopupMenuItem::new(t("Close connection"))
                                    .disabled(is_group)
                                    .on_click(move |_, _, cx| {
                                        let _ = entity.update(cx, |view, cx| {
                                            if let Some(target) = target.clone() {
                                                view.close(target, cx)
                                            }
                                        });
                                    }),
                            )
                        })
                })
                .collect::<Vec<_>>()
        })
        .track_scroll(&self.scroll)
        .flex_1();
        let mut header = div()
            .flex()
            .flex_none()
            .gap_3()
            .px_3()
            .py_2()
            .bg(pal.surface_2)
            .border_b_1()
            .border_color(pal.border)
            .text_xs()
            .text_color(pal.muted);
        for (label, width) in [
            ("Target", 280.),
            ("Network", 70.),
            ("Outbound", 140.),
            ("Upload", 90.),
            ("Download", 90.),
            ("Duration", 80.),
        ] {
            header = header.child(div().w(px(width)).child(t(label)));
        }
        let mut content = div()
            .id("connections-view")
            .key_context("AstraLive")
            .track_focus(&self.focus)
            .flex()
            .flex_col()
            .gap_3()
            .size_full()
            .min_h_0()
            .on_key_down(cx.listener(Self::key))
            .child(toolbar)
            .when(!badges.is_empty(), |view| {
                view.child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(badges),
                )
            })
            .child(
                // The web UI's `.card.table-wrap`: a bordered surface with a grey header row.
                div()
                    .id("connections-scroll")
                    .flex_1()
                    .min_h_0()
                    .rounded_lg()
                    .border_1()
                    .border_color(pal.border)
                    .bg(pal.surface)
                    .overflow_x_scroll()
                    .child(
                        div()
                            .min_w(px(850.))
                            .h_full()
                            .flex()
                            .flex_col()
                            .child(header)
                            .child(list),
                    ),
            );
        if let Some(connection) = detail {
            let close_target = detail_target.clone();
            content = content.child(
                Card::new(format!("{}:{}", connection.host, connection.port))
                    .child(div().text_sm().child(format!(
                        "{}: {} · {}: {}",
                        t("Source"),
                        connection.source.unwrap_or_else(|| "—".into()),
                        t("Network"),
                        connection.network
                    )))
                    .child(div().text_sm().child(format!(
                        "{}: {} · {}: {}",
                        t("Inbound"),
                        connection.inbound,
                        t("Outbound"),
                        connection.outbound
                    )))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(
                                Button::new("close-connection")
                                    .label(t("Close connection"))
                                    .on_click(cx.listener(move |view, _, _, cx| {
                                        if let Some(target) = close_target.clone() {
                                            view.close(target, cx)
                                        }
                                    })),
                            )
                            .child(Button::new("close-detail").label(t("Close")).on_click(
                                cx.listener(|view, _, _, cx| {
                                    view.dispatch(
                                        |client| {
                                            client.close_connection_detail();
                                            ClientInput::Idle
                                        },
                                        cx,
                                    )
                                }),
                            )),
                    ),
            );
        }
        content
    }
}
