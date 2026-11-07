use crate::{
    model::AppModel,
    views::ViewEvent,
    widgets::{DelayChip, Empty},
};
use astra_client::{
    i18n::t,
    state::{Client, Input as ClientInput},
};
use gpui::{
    div, px, uniform_list, App, AppContext, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render,
    StatefulInteractiveElement, Styled, Subscription, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants},
    input::{Input, InputEvent, InputState},
    ActiveTheme,
};
use std::sync::Arc;

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
                client.page = astra_client::navigation::PROXIES;
                action(client)
            });
            cx.notify();
            input
        });
        let client = &self.model.read(cx).client;
        let selected = client
            .selected_proxy_group()
            .map(|group| group.name.as_str());
        let index = client
            .proxy_groups(self.search.read(cx).value().as_ref())
            .iter()
            .position(|group| Some(group.name.as_str()) == selected);
        if let Some(index) = index {
            self.scroll
                .scroll_to_item(index, gpui::ScrollStrategy::Nearest);
        }
        let page = self.model.read(cx).client.page;
        if page != astra_client::navigation::PROXIES {
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
            cx.subscribe(&search, |view, state, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    let query = state.read(cx).value().to_string();
                    view.dispatch(
                        |client| {
                            client.filter_proxy_groups(&query);
                            ClientInput::Idle
                        },
                        cx,
                    );
                }
                cx.notify();
            }),
        ];
        Self {
            model,
            focus: cx.focus_handle(),
            search,
            observations,
            scroll: gpui::UniformListScrollHandle::new(),
        }
    }
    pub fn choose(&mut self, group: String, member: String, cx: &mut Context<Self>) {
        self.dispatch(|client| client.select_proxy_member(&group, &member), cx);
    }
    fn select(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        self.dispatch(
            |client| {
                client.select_proxy_group(&name);
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
        match event.keystroke.key.as_str() {
            "up" => self.dispatch(
                |client| {
                    client.move_proxy_group(-1);
                    ClientInput::Idle
                },
                cx,
            ),
            "down" => self.dispatch(
                |client| {
                    client.move_proxy_group(1);
                    ClientInput::Idle
                },
                cx,
            ),
            "t" if !event.is_held => self.dispatch(Client::test_selected_group, cx),
            "/" => self.search.update(cx, |input, cx| input.focus(window, cx)),
            _ => return,
        }
        cx.stop_propagation();
    }
}
impl Render for View {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _ = &self.observations;
        let client = &self.model.read(cx).client;
        let query = self.search.read(cx).value().to_string();
        let groups: Arc<[_]> = client.proxy_groups(&query).into();
        let group = client.selected_proxy_group().cloned();
        let group_name = group.as_ref().map(|group| group.name.clone());
        let members: Arc<[_]> = client
            .proxy_members(&query)
            .into_iter()
            .map(|member| {
                let pending = group
                    .as_ref()
                    .is_some_and(|group| client.proxy_member_pending(&group.name, &member.name));
                (member, pending)
            })
            .collect::<Vec<_>>()
            .into();
        let entity = cx.entity().downgrade();
        let theme = cx.theme().clone();
        let group_list = uniform_list("proxy-groups", groups.len(), move |range, _, _| {
            range
                .map(|index| {
                    let group = &groups[index];
                    let name = group.name.clone();
                    let selected = group_name.as_ref() == Some(&name);
                    let entity = entity.clone();
                    div()
                        .id(("proxy-group", index))
                        .h(px(62.))
                        .p_3()
                        .border_b_1()
                        .border_color(theme.border)
                        .bg(if selected {
                            theme.accent
                        } else {
                            theme.background
                        })
                        .child(div().text_sm().child(name.clone()))
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(group.selected.clone().unwrap_or_else(|| "—".into())),
                        )
                        .on_click(move |_, window, cx| {
                            let _ =
                                entity.update(cx, |view, cx| view.select(name.clone(), window, cx));
                        })
                })
                .collect::<Vec<_>>()
        })
        .track_scroll(&self.scroll)
        .h(px(420.))
        .w(px(220.));
        let mut member_panel = div().flex().flex_col().flex_1().min_w_0().gap_3();
        if let Some(group) = group {
            let title = group.name.clone();
            let testing = group.testing;
            member_panel = member_panel.child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(div().child(title.clone()))
                    .child(
                        Button::new("test-proxy-group")
                            .label(if testing {
                                t("Testing…")
                            } else {
                                t("Test selected group")
                            })
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.dispatch(Client::test_selected_group, cx)
                            })),
                    ),
            );
            let entity = cx.entity().downgrade();
            let member_list = uniform_list(
                "proxy-members",
                members.len().div_ceil(3),
                move |range, _, _| {
                    range
                        .map(|row| {
                            let mut line = div().h(px(84.)).flex().gap_2();
                            for index in row * 3..((row + 1) * 3).min(members.len()) {
                                let (member, pending) = &members[index];
                                let name = member.name.clone();
                                let target = title.clone();
                                let entity = entity.clone();
                                let mut button =
                                    Button::new(("proxy-member", index)).label(if *pending {
                                        format!("{} · {}", name, t("Selecting…"))
                                    } else {
                                        name.clone()
                                    });
                                if group.selected.as_ref() == Some(&name) || *pending {
                                    button = button.primary();
                                }
                                line = line.child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .flex()
                                        .flex_col()
                                        .gap_1()
                                        .child(button.on_click(move |_, _, cx| {
                                            let _ = entity.update(cx, |view, cx| {
                                                view.choose(target.clone(), name.clone(), cx)
                                            });
                                        }))
                                        .child(DelayChip::new(
                                            member
                                                .alive
                                                .filter(|alive| *alive)
                                                .and(member.delay_ms),
                                        )),
                                );
                            }
                            line
                        })
                        .collect::<Vec<_>>()
                },
            )
            .h(px(370.));
            member_panel = member_panel.child(member_list);
        } else {
            member_panel =
                member_panel.child(Empty::new(t("Proxies"), t("Waiting for runtime data…")));
        }
        div()
            .id("proxies-view")
            .key_context("AstraLive")
            .track_focus(&self.focus)
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .gap_3()
            .on_key_down(cx.listener(Self::key))
            .child(Input::new(&self.search))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .gap_4()
                    .child(group_list)
                    .child(member_panel),
            )
    }
}
