use crate::{model::AppModel, views::ViewEvent, widgets::Empty};
use astra_client::{
    i18n::t,
    nettools::{Field, Kind, OutputLine, OutputTone, RECORDS},
};
use gpui::{
    div, prelude::FluentBuilder, px, uniform_list, App, AppContext, Context, Entity, EventEmitter,
    FocusHandle, Focusable, InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render,
    ScrollHandle, StatefulInteractiveElement, Styled, Subscription, UniformListScrollHandle,
    Window,
};
use gpui_component::{
    button::{Button, ButtonVariants},
    input::{Input, InputEvent, InputState},
    ActiveTheme, Disableable,
};
#[path = "nettools_path.rs"]
mod path;
fn result_color(tone: OutputTone, cx: &App) -> gpui::Hsla {
    match tone {
        OutputTone::Success => cx.theme().success,
        OutputTone::Warning => cx.theme().warning,
        OutputTone::Error => cx.theme().danger,
        OutputTone::Info => cx.theme().info,
        OutputTone::Muted => cx.theme().muted_foreground,
    }
}
fn result_list(
    rows: std::sync::Arc<Vec<OutputLine>>,
    vertical: &UniformListScrollHandle,
    horizontal: &ScrollHandle,
    cx: &App,
) -> impl IntoElement {
    let width = rows
        .iter()
        .map(|line| astra_client::nettools::display_width(&line.text))
        .max()
        .unwrap_or(1) as f32
        * 7.;
    let mono = crate::theme::mono(cx);
    div()
        .id("network-result-scroll")
        .flex_1()
        .min_h(px(160.))
        .min_w_0()
        .overflow_x_scroll()
        .track_scroll(horizontal)
        .child(
            uniform_list("probe-result", rows.len(), move |range, _, cx| {
                range
                    .map(|i| {
                        div()
                            .h(px(22.))
                            .text_xs()
                            .font_family(mono.clone())
                            .whitespace_nowrap()
                            .text_color(result_color(rows[i].tone, cx))
                            .child(rows[i].text.clone())
                    })
                    .collect()
            })
            .track_scroll(vertical)
            .min_w(px(width))
            .h_full(),
        )
}
pub struct View {
    model: Entity<AppModel>,
    focus: FocusHandle,
    inputs: Vec<(Field, Entity<InputState>)>,
    egress: Entity<InputState>,
    observations: Vec<Subscription>,
    error: Option<String>,
    egress_open: bool,
    choice_open: Option<Field>,
    path: path::State,
    result_scroll: UniformListScrollHandle,
    result_horizontal: ScrollHandle,
}
impl EventEmitter<ViewEvent> for View {}
impl Focusable for View {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl View {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut inputs = vec![];
        let mut observations = vec![cx.observe(&model, |_, _, cx| cx.notify())];
        for field in [
            Field::Target,
            Field::Server,
            Field::Resolver,
            Field::Count,
            Field::Hops,
        ] {
            let value = model
                .read(cx)
                .client
                .diagnostics
                .as_ref()
                .and_then(|d| d.tools.as_ref())
                .map(|v| v.field_value(field))
                .unwrap_or_default();
            let input = cx.new(|cx| {
                let mut input = InputState::new(window, cx);
                input.set_value(value, window, cx);
                input
            });
            observations.push(cx.subscribe_in(
                &input,
                window,
                move |view, state, event: &InputEvent, window, cx| match event {
                    InputEvent::Change => {
                        let value = state.read(cx).value().to_string();
                        view.mutate(move |v| v.set_text(field, value), cx);
                    }
                    // Keep the shared field cursor on the box the pointer chose.
                    InputEvent::Focus => view.mutate(
                        move |v| {
                            v.field = field;
                            Ok(())
                        },
                        cx,
                    ),
                    InputEvent::PressEnter { .. } => {
                        view.start(cx);
                        window.focus(&view.focus, cx);
                    }
                    InputEvent::Blur => {}
                },
            ));
            inputs.push((field, input));
        }
        let egress = cx.new(|cx| InputState::new(window, cx).placeholder(t("Search egress")));
        observations.push(cx.subscribe(&egress, |_, _, _: &InputEvent, cx| cx.notify()));
        Self {
            model,
            focus: cx.focus_handle(),
            inputs,
            egress,
            observations,
            error: None,
            egress_open: false,
            choice_open: None,
            path: Default::default(),
            result_scroll: UniformListScrollHandle::new(),
            result_horizontal: ScrollHandle::new(),
        }
    }
    fn mutate(
        &mut self,
        action: impl FnOnce(&mut astra_client::nettools::View) -> Result<(), String>,
        cx: &mut Context<Self>,
    ) {
        self.error = self.model.update(cx, |model, cx| {
            let result = model.tokio(|c| {
                c.diagnostics
                    .as_mut()
                    .and_then(|d| d.tools.as_mut())
                    .map(action)
            });
            cx.notify();
            result.and_then(Result::err)
        });
        cx.notify();
    }
    pub fn set_kind(&mut self, kind: Kind, window: &mut Window, cx: &mut Context<Self>) {
        self.mutate(
            |v| {
                v.set_kind(kind);
                Ok(())
            },
            cx,
        );
        self.choice_open = None;
        self.egress_open = false;
        self.sync_inputs(window, cx);
    }
    fn tools<R>(
        &self,
        cx: &App,
        read: impl FnOnce(&astra_client::nettools::View) -> R,
    ) -> Option<R> {
        self.model
            .read(cx)
            .client
            .diagnostics
            .as_ref()
            .and_then(|d| d.tools.as_ref())
            .map(read)
    }
    /// Copy model-owned text back into the native boxes after a shared key changed it.
    fn sync_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(values) = self.tools(cx, |v| {
            self.inputs
                .iter()
                .map(|(field, _)| (*field, v.field_value(*field)))
                .collect::<Vec<_>>()
        }) else {
            return;
        };
        for (field, input) in &self.inputs {
            let Some((_, value)) = values.iter().find(|(key, _)| key == field) else {
                continue;
            };
            if input.read(cx).value().as_ref() != value.as_str() {
                let value = value.clone();
                input.update(cx, |input, cx| input.set_value(value, window, cx));
            }
        }
    }
    fn back(&mut self, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| {
            model.tokio(|c| {
                if let Some(d) = &mut c.diagnostics {
                    d.tools = None;
                    if d.tools_only {
                        c.diagnostics = None;
                    }
                }
            });
            cx.notify();
        });
        cx.notify();
    }
    /// Page shortcuts (copy and scroll) work even inside a text box; F-keys
    /// are swallowed so the global bindings do not fire on this page.
    fn shortcut(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let stroke = &event.keystroke;
        if stroke
            .key
            .strip_prefix('f')
            .and_then(|number| number.parse::<u8>().ok())
            .is_some_and(|number| (1..=12).contains(&number))
        {
            cx.stop_propagation();
            return;
        }
        if !stroke.modifiers.control
            || stroke.modifiers.alt
            || stroke.modifiers.platform
            || stroke.modifiers.shift
        {
            return;
        }
        let action = matches!(stroke.key.as_str(), "y" | "e");
        let navigation =
            self.focus.is_focused(window) && matches!(stroke.key.as_str(), "up" | "down");
        if action || navigation {
            if let Some(key) = crate::keys::from_keystroke(stroke, event.is_held) {
                self.mutate(
                    |v| {
                        v.key(key);
                        Ok(())
                    },
                    cx,
                );
            }
            cx.stop_propagation();
        }
    }
    fn key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let stroke = &event.keystroke;
        if stroke.modifiers.alt || stroke.modifiers.platform {
            return;
        }
        if !self.focus.is_focused(window) {
            // Inside a box: Escape returns to the form instead of closing the page.
            if stroke.key == "escape" {
                window.focus(&self.focus, cx);
                cx.stop_propagation();
            }
            return;
        }
        let Some(running) = self.tools(cx, |v| v.running()) else {
            return;
        };
        match stroke.key.as_str() {
            "escape" | "enter" if running => self.mutate(
                |v| {
                    v.stop();
                    Ok(())
                },
                cx,
            ),
            "escape" => self.back(cx),
            "enter" => self.start(cx),
            _ => {
                if stroke.modifiers.control && stroke.key != "u" {
                    return;
                }
                let Some(key) = crate::keys::from_keystroke(stroke, event.is_held) else {
                    return;
                };
                let navigation = matches!(stroke.key.as_str(), "up" | "down" | "tab");
                let field = self.tools(cx, |v| v.field).unwrap_or(Field::Kind);
                if !navigation && field.editable() {
                    // Typing on a text row starts editing its native box.
                    if let Some((_, input)) = self.inputs.iter().find(|(f, _)| *f == field) {
                        input.update(cx, |input, cx| input.focus(window, cx));
                    }
                    cx.stop_propagation();
                    return;
                }
                if !navigation
                    && !matches!(
                        key.code,
                        astra_client::input::KeyCode::Left
                            | astra_client::input::KeyCode::Right
                            | astra_client::input::KeyCode::PageUp
                            | astra_client::input::KeyCode::PageDown
                            | astra_client::input::KeyCode::Backspace
                            | astra_client::input::KeyCode::BackTab
                            | astra_client::input::KeyCode::Char(_)
                    )
                {
                    return;
                }
                self.mutate(
                    |v| {
                        v.key(key);
                        Ok(())
                    },
                    cx,
                );
                self.sync_inputs(window, cx);
            }
        }
        cx.stop_propagation();
    }
    pub fn start(&mut self, cx: &mut Context<Self>) {
        let values = self
            .inputs
            .iter()
            .map(|(f, input)| (*f, input.read(cx).value().to_string()))
            .collect::<Vec<_>>();
        self.mutate(
            move |v| {
                let visible = v.fields();
                for (field, value) in values {
                    if visible.contains(&field) {
                        v.set_text(field, value)?;
                    }
                }
                v.start();
                Ok(())
            },
            cx,
        );
    }
    fn label(text: &str) -> gpui::Div {
        div().w(px(110.)).flex_shrink_0().child(text.to_owned())
    }
    fn cycle_button(field: Field, value: String, cx: &mut Context<Self>) -> Button {
        Button::new(("field-choice", field as usize))
            .label(format!("‹ {value} ›"))
            .on_click(cx.listener(move |view, _, _, cx| {
                view.mutate(
                    move |v| {
                        v.field = field;
                        v.cycle(1);
                        Ok(())
                    },
                    cx,
                )
            }))
    }
}
impl Render for View {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _ = &self.observations;
        let mut root = div()
            .id("nettools-view")
            .key_context("AstraNetTools")
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .overflow_y_scroll()
            .gap_3();
        let Some(view) = self
            .model
            .read(cx)
            .client
            .diagnostics
            .as_ref()
            .and_then(|d| d.tools.as_ref())
        else {
            return root.child(Empty::new(t("Network tools"), t("Select a proxy")));
        };
        let chain = view.live_chain();
        let fields = view.fields();
        let kind = view.kind;
        let running = view.running();
        let record = view.record.clone();
        let selected = view.egress;
        let current = view.field;
        let query = view.egress_query.clone();
        let selected_label = astra_client::nettools::egress_label(&view.egresses[selected]);
        let choices =
            std::sync::Arc::new(view.egress_choices(self.egress.read(cx).value().as_ref()));
        let rows = std::sync::Arc::new(view.result_rows());
        let include_nat = view.include_nat;
        let name = view.name.clone();
        let physical_choices = view.physical_choices();
        let values = fields
            .iter()
            .map(|field| (*field, view.field_value(*field), view.field_hint(*field)))
            .collect::<Vec<_>>();
        let ready = match kind {
            // Cheap readiness only; full validation runs on submit.
            Kind::Nat => view.nat_interface.is_some(),
            Kind::FullCheck => !view.include_nat || view.nat_interface.is_some(),
            _ => true,
        };
        let value = |field: Field| {
            values
                .iter()
                .find(|(key, ..)| *key == field)
                .map(|(_, value, _)| value.clone())
                .unwrap_or_default()
        };
        let hint_of = |field: Field| {
            values
                .iter()
                .find(|(key, ..)| *key == field)
                .map(|(_, _, hint)| *hint)
                .unwrap_or_default()
        };
        root = root.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(div().text_lg().child(t("Network tools")))
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(name),
                ),
        );
        root = root.child(self.live_chain_view(&chain, cx));
        let mut form = div().flex().flex_col().gap_2().flex_shrink_0();
        for field in fields {
            let mut row = div()
                .flex()
                .flex_wrap()
                .gap_2()
                .items_center()
                .pl_2()
                .border_l_2()
                .border_color(gpui::transparent_black())
                .when(field == current, |row| row.border_color(cx.theme().primary));
            match field {
                Field::Egress => {
                    let choices = choices.clone();
                    let weak = cx.entity().downgrade();
                    row = row.child(Self::label(t(field.label()))).child(
                        Button::new("egress-picker")
                            .label(if query.is_empty() {
                                format!("{selected_label} ▾")
                            } else {
                                format!("{selected_label} ▾  /{query}")
                            })
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.egress_open = !view.egress_open;
                                cx.notify();
                            })),
                    );
                    if self.egress_open {
                        row = row.child(Input::new(&self.egress)).child(
                            uniform_list("egress-choices", choices.len(), move |range, _, _| {
                                range
                                    .map(|i| {
                                        let (index, label) = &choices[i];
                                        let index = *index;
                                        let weak = weak.clone();
                                        Button::new(("egress", i))
                                            .label(format!(
                                                "{} {}",
                                                if index == selected { "●" } else { "○" },
                                                label
                                            ))
                                            .on_click(move |_, _, cx| {
                                                let _ = weak.update(cx, |view, cx| {
                                                    view.mutate(
                                                        |v| {
                                                            v.select_egress(index);
                                                            Ok(())
                                                        },
                                                        cx,
                                                    );
                                                    view.egress_open = false;
                                                });
                                            })
                                    })
                                    .collect()
                            })
                            .h(px(100.))
                            .flex_1(),
                        );
                    }
                }
                Field::Kind => {
                    row = row.child(Self::label(t(field.label())));
                    for (index, choice) in Kind::ALL.into_iter().enumerate() {
                        let mut button = Button::new(("probe-kind", index))
                            .label(choice.label())
                            .on_click(cx.listener(move |view, _, window, cx| {
                                view.set_kind(choice, window, cx)
                            }));
                        if choice == kind {
                            button = button.primary();
                        }
                        row = row.child(button);
                    }
                }
                Field::Record => {
                    row = row.child(Self::label(t(field.label())));
                    for (index, value) in RECORDS.into_iter().enumerate() {
                        let mut button = Button::new(("probe-record", index))
                            .label(value)
                            .on_click(cx.listener(move |view, _, _, cx| {
                                view.mutate(|v| v.set_text(Field::Record, value.into()), cx)
                            }));
                        if value == record {
                            button = button.primary();
                        }
                        row = row.child(button);
                    }
                }
                Field::PhysicalInterface => {
                    row = row.child(Self::label(t(field.label()))).child(
                        Button::new("physical-picker")
                            .label(format!("{} ▾", value(field)))
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.choice_open =
                                    if view.choice_open == Some(Field::PhysicalInterface) {
                                        None
                                    } else {
                                        Some(Field::PhysicalInterface)
                                    };
                                cx.notify();
                            })),
                    );
                    if self.choice_open == Some(field) {
                        if physical_choices.is_empty() {
                            row =
                                row.child(div().text_xs().child(
                                    "没有可确认的目标 Agent 物理网卡；不会自动选择默认出口。",
                                ));
                        }
                        for (index, (id, label)) in physical_choices.iter().enumerate() {
                            let id = id.clone();
                            row = row.child(
                                Button::new(("physical-choice", index))
                                    .label(label.clone())
                                    .on_click(cx.listener(move |view, _, _, cx| {
                                        let id = id.clone();
                                        view.mutate(
                                            move |v| {
                                                v.select_physical(id);
                                                Ok(())
                                            },
                                            cx,
                                        );
                                        view.choice_open = None;
                                    })),
                            );
                        }
                    }
                }
                Field::Family
                | Field::HttpMethod
                | Field::NatAdvanced
                | Field::Comparison
                | Field::CompareRounds
                | Field::DualStack
                | Field::IncludeNat => {
                    let hint = hint_of(field);
                    row = row
                        .child(Self::label(t(field.label())))
                        .child(Self::cycle_button(field, value(field), cx))
                        .when(!hint.is_empty(), |row| {
                            row.child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(hint),
                            )
                        });
                }
                _ => {
                    if let Some((_, input)) = self.inputs.iter().find(|(key, _)| *key == field) {
                        let hint = hint_of(field);
                        row = row.child(Self::label(t(field.label()))).child(
                            div()
                                .flex_1()
                                .min_w(px(160.))
                                .child(Input::new(input))
                                .when(!hint.is_empty(), |column| {
                                    column.child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(hint),
                                    )
                                }),
                        );
                    }
                }
            }
            form = form.child(row);
        }
        root = root.child(form);
        if matches!(kind, Kind::Nat) || (kind == Kind::FullCheck && include_nat) {
            root = root.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        "NAT 检测通过手选的物理网卡直连 STUN 服务，服务会看到该网络的公网出口 IP。",
                    ),
            );
        }
        root = root.child(
            div()
                .flex()
                .flex_wrap()
                .gap_2()
                .child(
                    Button::new("probe-run")
                        .primary()
                        .label(t(if running { "Stop" } else { "Run" }))
                        .disabled(!running && !ready)
                        .on_click(cx.listener(move |view, _, _, cx| {
                            if running {
                                view.mutate(
                                    |v| {
                                        v.stop();
                                        Ok(())
                                    },
                                    cx,
                                )
                            } else {
                                view.start(cx)
                            }
                        })),
                )
                .child(
                    Button::new("probe-copy")
                        .label("复制结果 (Ctrl+Y)")
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.mutate(
                                |v| {
                                    v.copy_result(false);
                                    Ok(())
                                },
                                cx,
                            );
                        })),
                )
                .child(
                    Button::new("probe-export")
                        .label("复制 JSON (Ctrl+E)")
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.mutate(
                                |v| {
                                    v.copy_result(true);
                                    Ok(())
                                },
                                cx,
                            );
                        })),
                )
                .child(
                    Button::new("probe-back")
                        .label(t("Back"))
                        .on_click(cx.listener(|view, _, _, cx| view.back(cx))),
                ),
        );
        if let Some(error) = &self.error {
            root = root.child(div().text_color(cx.theme().danger).child(error.clone()));
        }
        if let Some(path) = self.path_view(cx) {
            root = root.child(path);
        }
        let result = crate::widgets::surface(cx)
            .id("network-result")
            .debug_selector(|| "network-result".into())
            .w_full()
            .min_w_0()
            .min_h(px(220.))
            .flex_1()
            .flex()
            .flex_col()
            .overflow_hidden()
            .p_2()
            .child(result_list(
                rows,
                &self.result_scroll,
                &self.result_horizontal,
                cx,
            ));
        root.capture_key_down(cx.listener(Self::shortcut))
            .on_key_down(cx.listener(Self::key))
            .child(result)
    }
}

#[cfg(test)]
#[path = "nettools_tests.rs"]
mod tests;
