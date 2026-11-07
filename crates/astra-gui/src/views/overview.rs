use crate::{
    model::AppModel,
    views::ViewEvent,
    widgets::{Card, DelayChip, Empty, Sparkline, StatusDot},
};
use astra_client::{
    i18n::t,
    navigation,
    overview::Severity,
    state::{Client, Input as ClientInput, Operation},
};
use astra_model::runtime::{Command, Mode};
use gpui::{
    div, px, uniform_list, App, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render,
    StatefulInteractiveElement, Styled, Subscription, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants},
    ActiveTheme,
};
use std::sync::Arc;

pub struct View {
    model: Entity<AppModel>,
    focus: FocusHandle,
    observation: Subscription,
}
impl EventEmitter<ViewEvent> for View {}
impl Focusable for View {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl View {
    pub fn new(model: Entity<AppModel>, _: &mut Window, cx: &mut Context<Self>) -> Self {
        let observation = cx.observe(&model, |_, _, cx| cx.notify());
        Self {
            model,
            focus: cx.focus_handle(),
            observation,
        }
    }
    fn dispatch(
        &mut self,
        action: impl FnOnce(&mut Client) -> ClientInput,
        cx: &mut Context<Self>,
    ) {
        let input = self.model.update(cx, |model, cx| {
            let input = model.tokio(|client| {
                client.page = astra_client::navigation::OVERVIEW;
                action(client)
            });
            cx.notify();
            input
        });
        let page = self.model.read(cx).client.page;
        if page != astra_client::navigation::OVERVIEW {
            cx.emit(ViewEvent::Navigate(page));
        }
        cx.emit(ViewEvent::Input(input));
        cx.notify();
    }
    pub fn runtime(&mut self, command: Command, cx: &mut Context<Self>) {
        self.dispatch(|client| client.runtime_action(command), cx);
    }
    fn key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.focus.is_focused(window)
            || event.is_held
            || event.keystroke.modifiers.control
            || event.keystroke.modifiers.alt
            || event.keystroke.modifiers.platform
        {
            return;
        }
        use astra_client::input::KeyCode;
        let Some(key) = crate::keys::from_keystroke(&event.keystroke, false) else {
            return;
        };
        match key.code {
            KeyCode::Char('c') => self.dispatch(Client::toggle_current, cx),
            KeyCode::Char('r') => {
                self.dispatch(|client| client.active_operation(Operation::Reload), cx)
            }
            KeyCode::Char('m') => self.dispatch(Client::cycle_mode, cx),
            KeyCode::Char('S') => self.dispatch(Client::toggle_system_proxy, cx),
            KeyCode::Char('T') => self.dispatch(Client::toggle_tun, cx),
            KeyCode::Char('A') => self.dispatch(Client::toggle_lan, cx),
            KeyCode::Char('D') => {
                self.dispatch(|client| client.active_operation(Operation::Diagnostics), cx)
            }
            KeyCode::Char('N') => {
                self.dispatch(|client| client.active_operation(Operation::NetTools), cx)
            }
            KeyCode::Char('L') => {
                self.dispatch(|client| client.active_operation(Operation::Logs), cx)
            }
            KeyCode::Char('e') => {
                self.dispatch(|client| client.active_operation(Operation::Edit), cx)
            }
            KeyCode::Char('z') => self.dispatch(
                |client| {
                    client.toggle_traffic_panel();
                    ClientInput::Idle
                },
                cx,
            ),
            KeyCode::Char(' ') => self.dispatch(Client::toggle_live_pause, cx),
            _ => return,
        }
        cx.stop_propagation();
    }
}
impl Render for View {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _ = &self.observation;
        let client = &self.model.read(cx).client;
        let header = client.header();
        let mut content = div()
            .id("overview-view")
            .key_context("AstraLive")
            .track_focus(&self.focus)
            .flex()
            .flex_col()
            .gap_4()
            .size_full()
            .min_h_0()
            .overflow_y_scroll()
            .on_key_down(cx.listener(Self::key));
        let Some(name) = header.name.clone() else {
            return content
                .child(Empty::new(t("Overview"), t("No profile yet")))
                .child(
                    Button::new("overview-create")
                        .primary()
                        .label(t("Profiles"))
                        .on_click(cx.listener(|_, _, _, cx| {
                            cx.emit(ViewEvent::Navigate(navigation::PROFILES))
                        })),
                )
                .into_any_element();
        };
        let snapshot = client.live().cloned();
        let notices = client.notices();
        let expanded = client.traffic_expanded;
        let paused = client.connections_paused();
        let health = client.runtime_health();
        let listeners = client
            .active_proxy()
            .and_then(|row| client.details.get(&row.instance.name))
            .map(|details| details.listeners.join(" · "))
            .unwrap_or_default();
        let histories = client.history.clone();
        let direct = client.direct_rate_values();
        let connections: Arc<[_]> = client
            .visible_connections_at(navigation::OVERVIEW)
            .into_iter()
            .take(8)
            .cloned()
            .collect::<Vec<_>>()
            .into();
        let groups: Arc<[_]> = client.proxy_groups("").into();
        let mut title = Card::new(name)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(StatusDot::new(super::profiles::status(header.state)))
                    .child(div().text_sm().child(header.uptime))
                    .child(
                        Button::new("overview-toggle")
                            .primary()
                            .label(header.connect_label)
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.dispatch(Client::toggle_current, cx)
                            })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(div().child(header.mode_label))
                    .child(div().flex_1().child(header.exit_chain))
                    .child(DelayChip::new(header.delay_ms)),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(listeners),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(health),
            );
        let rates = div()
            .flex()
            .gap_4()
            .child(
                div()
                    .flex_1()
                    .child(format!(
                        "↑ {}/s",
                        astra_client::live::rate_value(header.rates[0], false).trim()
                    ))
                    .child(Sparkline::new(
                        &histories[0].iter().copied().collect::<Vec<_>>(),
                        cx.theme().success,
                    )),
            )
            .child(
                div()
                    .flex_1()
                    .child(format!(
                        "↓ {}/s",
                        astra_client::live::rate_value(header.rates[1], false).trim()
                    ))
                    .child(Sparkline::new(
                        &histories[1].iter().copied().collect::<Vec<_>>(),
                        cx.theme().primary,
                    )),
            );
        title = title.child(rates);
        content = content.child(title);
        for (index, (severity, message)) in notices.into_iter().enumerate() {
            let color = match severity {
                Severity::Success => cx.theme().success,
                Severity::Warning => cx.theme().warning,
                Severity::Error => cx.theme().danger,
            };
            content = content.child(
                div()
                    .id(("runtime-notice", index))
                    .p_3()
                    .rounded_md()
                    .border_1()
                    .border_color(color)
                    .text_color(color)
                    .child(message),
            );
        }
        if let Some(snapshot) = snapshot {
            let mut controls = div().flex().flex_wrap().gap_2();
            for (index, (label, mode)) in [
                ("Rule", Mode::Rule),
                ("Global", Mode::Global),
                ("Direct", Mode::Direct),
            ]
            .into_iter()
            .enumerate()
            {
                let mut button = Button::new(("runtime-mode", index)).label(t(label));
                if snapshot.mode == mode {
                    button = button.primary();
                }
                controls = controls.child(button.on_click(
                    cx.listener(move |view, _, _, cx| view.runtime(Command::SetMode(mode), cx)),
                ));
            }
            for (index, (label, on)) in [
                ("System proxy", snapshot.system_proxy),
                ("TUN", snapshot.tun),
                ("Allow LAN", snapshot.allow_lan),
            ]
            .into_iter()
            .enumerate()
            {
                let mut button = Button::new(("takeover", index)).label(format!(
                    "{} {}",
                    if on { "☑" } else { "☐" },
                    t(label)
                ));
                if on {
                    button = button.primary();
                }
                controls = controls.child(button.on_click(cx.listener(move |view, _, _, cx| {
                    view.dispatch(
                        match index {
                            0 => Client::toggle_system_proxy,
                            1 => Client::toggle_tun,
                            _ => Client::toggle_lan,
                        },
                        cx,
                    )
                })));
            }
            content = content.child(controls).child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        Button::new("overview-pause")
                            .label(if paused { t("Resume") } else { t("Pause") })
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.dispatch(Client::toggle_live_pause, cx)
                            })),
                    )
                    .child(
                        Button::new("overview-traffic")
                            .label(t("Traffic"))
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.dispatch(
                                    |client| {
                                        client.toggle_traffic_panel();
                                        ClientInput::Idle
                                    },
                                    cx,
                                )
                            })),
                    ),
            );
            if expanded {
                content = content.child(
                    Card::new(t("Traffic"))
                        .child(div().child(format!(
                            "{} · ↑ {} · ↓ {}",
                            t("Total"),
                            astra_client::live::bytes(snapshot.upload),
                            astra_client::live::bytes(snapshot.download)
                        )))
                        .child(div().child(format!(
                            "{} · ↑ {} · ↓ {}",
                            t("Proxy"),
                            astra_client::live::bytes(snapshot.proxy_upload),
                            astra_client::live::bytes(snapshot.proxy_download)
                        )))
                        .child(div().child(format!(
                            "{} · ↑ {}/s · ↓ {}/s",
                            t("Direct"),
                            astra_client::live::rate_value(direct[0], false).trim(),
                            astra_client::live::rate_value(direct[1], false).trim()
                        ))),
                );
            }
        }
        let entity = cx.entity().downgrade();
        let connection_list = uniform_list(
            "overview-connections",
            connections.len(),
            move |range, _, _| {
                range
                    .map(|index| {
                        let connection = &connections[index];
                        let id = connection.id;
                        let entity = entity.clone();
                        div()
                            .id(("overview-connection", index))
                            .h(px(40.))
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .child(format!("{}:{}", connection.host, connection.port))
                            .child(connection.outbound.clone())
                            .child(format!(
                                "↑ {} · ↓ {}",
                                astra_client::live::bytes(connection.upload),
                                astra_client::live::bytes(connection.download)
                            ))
                            .on_click(move |_, _, cx| {
                                let _ = entity.update(cx, |view, cx| {
                                    view.dispatch(
                                        |client| {
                                            client.select_connection_id(id);
                                            ClientInput::Idle
                                        },
                                        cx,
                                    );
                                    cx.emit(ViewEvent::Navigate(navigation::CONNECTIONS));
                                });
                            })
                    })
                    .collect::<Vec<_>>()
            },
        )
        .h(px(180.));
        let entity = cx.entity().downgrade();
        let group_list = uniform_list("overview-groups", groups.len(), move |range, _, _| {
            range
                .map(|index| {
                    let group = &groups[index];
                    let name = group.name.clone();
                    let entity = entity.clone();
                    div()
                        .id(("overview-group", index))
                        .h(px(40.))
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_3()
                        .child(group.name.clone())
                        .child(group.selected.clone().unwrap_or_else(|| "—".into()))
                        .on_click(move |_, _, cx| {
                            let _ = entity.update(cx, |view, cx| {
                                view.dispatch(
                                    |client| {
                                        client.select_proxy_group(&name);
                                        ClientInput::Idle
                                    },
                                    cx,
                                );
                                cx.emit(ViewEvent::Navigate(navigation::PROXIES));
                            });
                        })
                })
                .collect::<Vec<_>>()
        })
        .h(px(180.));
        content
            .child(Card::new(t("Connections")).child(connection_list))
            .child(Card::new(t("Proxies")).child(group_list))
            .into_any_element()
    }
}
