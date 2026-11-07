use crate::{
    model::AppModel,
    views::ViewEvent,
    widgets::{Card, DelayChip, Empty, Metric, NoticeBar, Pill, Sparkline, StatusDot, Tone},
};
use astra_client::{
    i18n::t,
    navigation,
    overview::Severity,
    state::{Client, Input as ClientInput, Operation},
};
use astra_model::runtime::{Command, Mode};
use gpui::{
    div, prelude::FluentBuilder, px, uniform_list, App, Context, Entity, EventEmitter, FocusHandle,
    Focusable, InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render,
    StatefulInteractiveElement, Styled, Subscription, Window,
};
use gpui_component::button::{Button, ButtonVariants};
use std::sync::Arc;

pub struct View {
    model: Entity<AppModel>,
    focus: FocusHandle,
    observation: Subscription,
    /// The proxy group unfolded in the overview's Proxies card, managed in place.
    expanded: Option<String>,
    /// Keyboard cursor in the Proxies card: a group row or one of its node rows.
    cursor: Option<CardRow>,
    group_scroll: gpui::ScrollHandle,
}
/// A row of the Proxies card's fold-out menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CardRow {
    Group(String),
    Member(String, String),
    /// The mode/test line under an unfolded group; never takes the cursor.
    Footer(String),
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
            expanded: None,
            cursor: None,
            group_scroll: gpui::ScrollHandle::new(),
        }
    }
    /// Rows the Proxies card shows, in order, including unfolded node lists.
    fn card_rows(&self, cx: &App) -> Vec<CardRow> {
        let mut rows = vec![];
        for group in self.model.read(cx).client.proxy_groups("") {
            let open = self.expanded.as_deref() == Some(group.name.as_str());
            rows.push(CardRow::Group(group.name.clone()));
            if open {
                rows.extend(
                    group
                        .members
                        .iter()
                        .map(|member| CardRow::Member(group.name.clone(), member.name.clone())),
                );
                rows.push(CardRow::Footer(group.name));
            }
        }
        rows
    }
    pub fn cursor(&self) -> Option<&CardRow> {
        self.cursor.as_ref()
    }
    fn set_cursor(&mut self, row: CardRow, cx: &mut Context<Self>) {
        if let Some(index) = self.card_rows(cx).iter().position(|item| *item == row) {
            self.group_scroll.scroll_to_item(index);
        }
        self.cursor = Some(row);
        cx.notify();
    }
    /// ↑/↓ walk group and node rows; skips the footer line.
    pub fn move_cursor(&mut self, delta: isize, cx: &mut Context<Self>) {
        let rows: Vec<_> = self
            .card_rows(cx)
            .into_iter()
            .filter(|row| !matches!(row, CardRow::Footer(_)))
            .collect();
        if rows.is_empty() {
            return;
        }
        let next = match self
            .cursor
            .as_ref()
            .and_then(|cursor| rows.iter().position(|row| row == cursor))
        {
            Some(index) => index.saturating_add_signed(delta).min(rows.len() - 1),
            // Start from the group already chosen in the client, else the first row.
            None => {
                let selected = self
                    .model
                    .read(cx)
                    .client
                    .selected_proxy_group()
                    .map(|group| CardRow::Group(group.name.clone()));
                rows.iter()
                    .position(|row| Some(row) == selected.as_ref())
                    .unwrap_or(0)
            }
        };
        self.set_cursor(rows[next].clone(), cx);
    }
    /// → unfolds the group under the cursor (or steps into its nodes).
    pub fn expand_cursor(&mut self, cx: &mut Context<Self>) {
        match self.cursor.clone() {
            Some(CardRow::Group(name)) if self.expanded.as_deref() == Some(name.as_str()) => {
                self.move_cursor(1, cx)
            }
            Some(CardRow::Group(name)) => self.toggle_group(name, cx),
            None => self.move_cursor(0, cx),
            _ => {}
        }
    }
    /// ← folds the group under the cursor, or jumps from a node back to its group.
    pub fn collapse_cursor(&mut self, cx: &mut Context<Self>) {
        match self.cursor.clone() {
            Some(CardRow::Member(group, _)) => {
                self.expanded = None;
                self.set_cursor(CardRow::Group(group), cx);
            }
            Some(CardRow::Group(name)) if self.expanded.as_deref() == Some(name.as_str()) => {
                self.expanded = None;
                cx.notify();
            }
            _ => {}
        }
    }
    /// Enter toggles a group or picks the node under the cursor.
    pub fn activate_cursor(&mut self, cx: &mut Context<Self>) -> bool {
        match self.cursor.clone() {
            Some(CardRow::Group(name)) => self.toggle_group(name, cx),
            Some(CardRow::Member(group, member)) => {
                self.choose(group.clone(), member, cx);
                self.set_cursor(CardRow::Group(group), cx);
            }
            _ => return false,
        }
        true
    }
    /// Unfolds a group in the Proxies card (or folds it again) without leaving the page.
    pub fn toggle_group(&mut self, name: String, cx: &mut Context<Self>) {
        if self.expanded.as_deref() == Some(name.as_str()) {
            self.expanded = None;
        } else {
            self.dispatch(
                |client| {
                    client.select_proxy_group(&name);
                    ClientInput::Idle
                },
                cx,
            );
            self.expanded = Some(name.clone());
        }
        self.cursor = Some(CardRow::Group(name));
        cx.notify();
    }
    /// Picks a node and folds the group back up, like choosing from a menu.
    pub fn choose(&mut self, group: String, member: String, cx: &mut Context<Self>) {
        self.dispatch(|client| client.select_proxy_member(&group, &member), cx);
        if self.expanded.as_deref() == Some(group.as_str()) {
            self.expanded = None;
        }
        cx.notify();
    }
    pub fn expanded_group(&self) -> Option<&str> {
        self.expanded.as_deref()
    }
    pub fn test_group(&mut self, group: String, cx: &mut Context<Self>) {
        self.dispatch(
            |client| {
                if client.select_proxy_group(&group) {
                    client.test_selected_group()
                } else {
                    ClientInput::Idle
                }
            },
            cx,
        );
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
        // Arrows may auto-repeat through the Proxies card; other keys fire once.
        match key.code {
            KeyCode::Up => self.move_cursor(-1, cx),
            KeyCode::Down => self.move_cursor(1, cx),
            KeyCode::Right => self.expand_cursor(cx),
            KeyCode::Left => self.collapse_cursor(cx),
            KeyCode::Enter if !event.is_held && self.activate_cursor(cx) => {}
            _ if event.is_held => return,
            _ => return self.shortcut(key.code, cx),
        }
        cx.stop_propagation();
    }
    fn shortcut(&mut self, code: astra_client::input::KeyCode, cx: &mut Context<Self>) {
        use astra_client::input::KeyCode;
        match code {
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
impl View {
    fn operation(&mut self, operation: Operation, cx: &mut Context<Self>) {
        self.dispatch(move |client| client.active_operation(operation), cx);
    }
}
impl Render for View {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _ = &self.observation;
        let pal = crate::theme::palette(cx);
        let mono = crate::theme::mono(cx);
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
                .child(
                    Card::new("")
                        .child(Empty::new(t("Overview"), t("No profile yet")))
                        .child(
                            div().flex().justify_center().child(
                                Button::new("overview-create")
                                    .primary()
                                    .label(t("Profiles"))
                                    .on_click(cx.listener(|_, _, _, cx| {
                                        cx.emit(ViewEvent::Navigate(navigation::PROFILES))
                                    })),
                            ),
                        ),
                )
                .into_any_element();
        };
        let snapshot = client.live().cloned();
        let notices = client.notices();
        let expanded = client.traffic_expanded;
        let paused = client.connections_paused();
        let health = client.runtime_health();
        let row = client.active_proxy().cloned();
        let listeners = row
            .as_ref()
            .and_then(|row| client.details.get(&row.instance.name))
            .map(|details| details.listeners.join(" · "))
            .unwrap_or_default();
        let config = row
            .as_ref()
            .map(|row| row.instance.config.to_string_lossy().into_owned())
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
        let pending: Vec<(String, String)> = groups
            .iter()
            .flat_map(|group| {
                group
                    .members
                    .iter()
                    .filter(|member| client.proxy_member_pending(&group.name, &member.name))
                    .map(|member| (group.name.clone(), member.name.clone()))
            })
            .collect();

        // State card: who is running, where it exits, and the profile's quick actions.
        let mut quick = div().flex().flex_wrap().gap_2();
        for (index, (label, operation)) in [
            ("Reload", Operation::Reload),
            ("Node tests", Operation::Diagnostics),
            ("Network tools", Operation::NetTools),
            ("Logs", Operation::Logs),
        ]
        .into_iter()
        .enumerate()
        {
            quick = quick.child(
                Button::new(("overview-operation", index))
                    .label(t(label))
                    .on_click(cx.listener(move |view, _, _, cx| view.operation(operation, cx))),
            );
        }
        let state = Card::new("")
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(StatusDot::new(super::profiles::status(header.state)))
                    .child(
                        div()
                            .text_size(px(15.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(name),
                    )
                    .when(!header.uptime.is_empty() && header.connected, |line| {
                        line.child(Pill::new(header.uptime.clone()))
                    })
                    .child(div().flex_1())
                    .child(Pill::new(header.mode_label).tone(Tone::Accent)),
            )
            .when(!config.is_empty(), |card| {
                card.child(
                    div()
                        .text_xs()
                        .font_family(mono.clone())
                        .text_color(pal.muted)
                        .child(config),
                )
            })
            .when(!header.exit_chain.is_empty(), |card| {
                card.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(div().text_sm().text_color(pal.muted).child(t("Exit")))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(header.exit_chain.clone()),
                        )
                        .child(crate::widgets::ExitProbe::new(&header)),
                )
            })
            .when(!listeners.is_empty(), |card| {
                card.child(div().text_sm().text_color(pal.muted).child(listeners))
            })
            .when(!health.is_empty(), |card| {
                card.child(div().text_xs().text_color(pal.muted).child(health))
            })
            .child(quick);
        content = content.child(state);
        for (severity, message) in notices {
            let tone = match severity {
                Severity::Success => Tone::Ok,
                Severity::Warning => Tone::Warn,
                Severity::Error => Tone::Err,
            };
            content = content.child(NoticeBar::new(message, tone));
        }

        // Metrics: the web UI's four-tile row, with live rate sparklines.
        let rate = |index: usize| {
            format!(
                "{}/s",
                astra_client::live::rate_value(header.rates[index], false).trim()
            )
        };
        let mut metrics = div().flex().flex_wrap().gap_3();
        metrics = metrics
            .child(Metric::new(
                t("Connections"),
                snapshot
                    .as_ref()
                    .map(|s| s.active.to_string())
                    .unwrap_or_else(|| "—".into()),
            ))
            .child(
                Metric::new(format!("↑ {}", t("Upload")), rate(0)).detail(Sparkline::new(
                    &histories[0].iter().copied().collect::<Vec<_>>(),
                    pal.ok,
                )),
            )
            .child(
                Metric::new(format!("↓ {}", t("Download")), rate(1)).detail(Sparkline::new(
                    &histories[1].iter().copied().collect::<Vec<_>>(),
                    pal.accent,
                )),
            )
            .child({
                let total = |value: Option<u64>| {
                    value
                        .map(astra_client::live::bytes)
                        .unwrap_or_else(|| "—".into())
                };
                Metric::new(
                    t("Total"),
                    format!("↑ {}", total(snapshot.as_ref().map(|s| s.upload))),
                )
                .detail(
                    div()
                        .text_size(px(24.))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .whitespace_nowrap()
                        .child(format!(
                            "↓ {}",
                            total(snapshot.as_ref().map(|s| s.download))
                        )),
                )
            });
        content = content.child(metrics);

        // Runtime control: mode and takeover switches, as in the web UI.
        if let Some(snapshot) = snapshot.as_ref() {
            let mut modes = div().flex().gap_1().p_1().rounded_md().bg(pal.surface_2);
            for (index, (label, mode)) in [
                ("Rule", Mode::Rule),
                ("Global", Mode::Global),
                ("Direct", Mode::Direct),
            ]
            .into_iter()
            .enumerate()
            {
                let mut button = Button::new(("runtime-mode", index)).label(t(label));
                button = if snapshot.mode == mode {
                    button.primary()
                } else {
                    button.ghost()
                };
                modes = modes.child(button.on_click(
                    cx.listener(move |view, _, _, cx| view.runtime(Command::SetMode(mode), cx)),
                ));
            }
            let mut takeovers = div().flex().flex_wrap().gap_2();
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
                    if on { "●" } else { "○" },
                    t(label)
                ));
                if on {
                    button = button.primary();
                }
                takeovers = takeovers.child(button.on_click(cx.listener(move |view, _, _, cx| {
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
            let mut control = Card::new(t("Runtime control"))
                .action(
                    Button::new("overview-pause")
                        .ghost()
                        .label(if paused { t("Resume") } else { t("Pause") })
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.dispatch(Client::toggle_live_pause, cx)
                        })),
                )
                .action(
                    Button::new("overview-traffic")
                        .ghost()
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
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_4()
                        .child(modes)
                        .child(takeovers),
                );
            if expanded {
                let line = |label: &str, value: String| {
                    div()
                        .flex()
                        .gap_3()
                        .text_sm()
                        .child(
                            div()
                                .w(px(90.))
                                .text_color(pal.muted)
                                .child(t(label).to_owned()),
                        )
                        .child(div().font_family(mono.clone()).child(value))
                };
                control = control
                    .child(line(
                        "Total",
                        format!(
                            "↑ {} · ↓ {}",
                            astra_client::live::bytes(snapshot.upload),
                            astra_client::live::bytes(snapshot.download)
                        ),
                    ))
                    .child(line(
                        "Proxy",
                        format!(
                            "↑ {} · ↓ {}",
                            astra_client::live::bytes(snapshot.proxy_upload),
                            astra_client::live::bytes(snapshot.proxy_download)
                        ),
                    ))
                    .child(line(
                        "Direct",
                        format!(
                            "↑ {}/s · ↓ {}/s",
                            astra_client::live::rate_value(direct[0], false).trim(),
                            astra_client::live::rate_value(direct[1], false).trim()
                        ),
                    ));
            }
            content = content.child(control);
        }

        // Connections and proxy groups side by side, each opening its full page.
        let entity = cx.entity().downgrade();
        let muted = pal.muted;
        let hover = pal.surface_2;
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
                            .h(px(36.))
                            .px_2()
                            .rounded_md()
                            .flex()
                            .items_center()
                            .gap_3()
                            .text_sm()
                            .hover(move |style| style.bg(hover))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .child(format!("{}:{}", connection.host, connection.port)),
                            )
                            .child(div().text_color(muted).child(connection.outbound.clone()))
                            .child(
                                div()
                                    .text_xs()
                                    .font_family(mono.clone())
                                    .text_color(muted)
                                    .child(format!(
                                        "↑ {} · ↓ {}",
                                        astra_client::live::bytes(connection.upload),
                                        astra_client::live::bytes(connection.download)
                                    )),
                            )
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
        .h(px(288.));
        // Proxy groups unfold in place: pick a node or test the group without
        // leaving the overview; "Open" still leads to the full Proxies page.
        let mut group_list = div()
            .id("overview-groups")
            .h(px(288.))
            .overflow_y_scroll()
            .track_scroll(&self.group_scroll)
            .flex()
            .flex_col();
        // Each card row is a direct child so the keyboard cursor can scroll to it.
        let cursor = self.cursor.clone();
        let ring = |row: CardRow| {
            if cursor.as_ref() == Some(&row) {
                pal.accent
            } else {
                gpui::transparent_black()
            }
        };
        if groups.is_empty() {
            group_list = group_list.child(Empty::new(t("Proxies"), t("Waiting for runtime data…")));
        }
        for (index, group) in groups.iter().enumerate() {
            let name = group.name.clone();
            let open = self.expanded.as_deref() == Some(name.as_str());
            let header = div()
                .id(("overview-group", index))
                .h(px(36.))
                .flex_none()
                .px_2()
                .rounded_md()
                .border_1()
                .border_color(ring(CardRow::Group(name.clone())))
                .flex()
                .items_center()
                .gap_3()
                .text_sm()
                .cursor_pointer()
                .when(open, |row| row.bg(pal.accent_soft))
                .hover(move |style| style.bg(hover))
                .child(div().w(px(10.)).text_xs().text_color(muted).child(if open {
                    "▾"
                } else {
                    "▸"
                }))
                .child(div().flex_1().min_w_0().truncate().child(name.clone()))
                .child(
                    div()
                        .max_w(px(160.))
                        .truncate()
                        .text_color(if open { pal.accent } else { muted })
                        .child(
                            pending
                                .iter()
                                .find(|(g, _)| *g == group.name)
                                .map(|(_, m)| m.clone())
                                .or_else(|| group.selected.clone())
                                .unwrap_or_else(|| "—".into()),
                        ),
                )
                .on_click(cx.listener({
                    let name = name.clone();
                    move |view, _, _, cx| view.toggle_group(name.clone(), cx)
                }));
            group_list = group_list.child(header);
            if !open {
                continue;
            }
            // A fold-out menu: one row per node, the chosen one checked.
            let guide = || {
                div()
                    .flex_none()
                    .ml(px(13.))
                    .pl_2()
                    .pr_2()
                    .border_l_1()
                    .border_color(pal.border)
            };
            for (member_index, member) in group.members.iter().enumerate() {
                let member_name = member.name.clone();
                let is_pending = pending
                    .iter()
                    .any(|(g, m)| *g == group.name && *m == member.name);
                let chosen = group.selected.as_ref() == Some(&member.name) || is_pending;
                group_list = group_list.child(
                    guide().child(
                        div()
                            .id(("overview-member", index * 10_000 + member_index))
                            .h(px(30.))
                            .px_2()
                            .rounded_md()
                            .border_1()
                            .border_color(ring(CardRow::Member(name.clone(), member.name.clone())))
                            .cursor_pointer()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_sm()
                            .when(chosen, |row| row.bg(pal.accent_soft))
                            .when(!chosen, |row| row.hover(move |s| s.bg(hover)))
                            .child(
                                div()
                                    .w(px(12.))
                                    .text_xs()
                                    .text_color(pal.accent)
                                    .child(if chosen { "✓" } else { "" }),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .when(chosen, |text| {
                                        text.text_color(pal.accent)
                                            .font_weight(gpui::FontWeight::MEDIUM)
                                    })
                                    .child(if is_pending {
                                        format!("{} · {}", member.name, t("Selecting…"))
                                    } else {
                                        member.name.clone()
                                    }),
                            )
                            .child(DelayChip::new(
                                member.alive.filter(|alive| *alive).and(member.delay_ms),
                            ))
                            .on_click(cx.listener({
                                let group = name.clone();
                                move |view, _, _, cx| {
                                    view.choose(group.clone(), member_name.clone(), cx)
                                }
                            })),
                    ),
                );
            }
            group_list = group_list.child(
                guide().pb_2().child(
                    div()
                        .pt_1()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(Pill::new(group.mode.clone()))
                        .child(div().flex_1())
                        .child(
                            Button::new(("overview-test-group", index))
                                .ghost()
                                .label(if group.testing {
                                    t("Testing…")
                                } else {
                                    t("Test")
                                })
                                .on_click(cx.listener({
                                    let name = name.clone();
                                    move |view, _, _, cx| view.test_group(name.clone(), cx)
                                })),
                        ),
                ),
            );
        }
        let open = |id: &'static str, page: usize, cx: &mut Context<Self>| {
            Button::new(id)
                .ghost()
                .label(t("Open"))
                .on_click(cx.listener(move |_, _, _, cx| cx.emit(ViewEvent::Navigate(page))))
        };
        content
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_4()
                    .child(
                        div().flex_1().min_w(px(320.)).child(
                            Card::new(t("Connections"))
                                .action(open(
                                    "overview-open-connections",
                                    navigation::CONNECTIONS,
                                    cx,
                                ))
                                .child(connection_list),
                        ),
                    )
                    .child(
                        div().flex_1().min_w(px(320.)).child(
                            Card::new(t("Proxies"))
                                .action(open("overview-open-proxies", navigation::PROXIES, cx))
                                .child(group_list),
                        ),
                    ),
            )
            .into_any_element()
    }
}
