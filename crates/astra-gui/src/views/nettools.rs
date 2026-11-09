use crate::{model::AppModel, views::ViewEvent, widgets::Empty};
use astra_client::{
    i18n::t,
    nettools::{Field, Kind, OutputLine, OutputTone, Section, RECORDS},
};
use gpui::{
    div, prelude::FluentBuilder, px, relative, uniform_list, App, AppContext, Context, Entity,
    EventEmitter, FocusHandle, Focusable, InteractiveElement, IntoElement, KeyDownEvent,
    ParentElement, Render, ScrollHandle, StatefulInteractiveElement, Styled, Subscription,
    UniformListScrollHandle, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants},
    input::{Input, InputEvent, InputState},
    ActiveTheme, Disableable,
};
#[path = "nettools_topology.rs"]
mod topology;
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
    id: &'static str,
    list_id: &'static str,
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
        .id(id)
        .flex_1()
        .min_h(px(120.))
        .min_w_0()
        .overflow_x_scroll()
        .track_scroll(horizontal)
        .child(
            uniform_list(list_id, rows.len(), move |range, _, cx| {
                range
                    .map(|i| {
                        div()
                            .h(px(24.))
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
    topology: topology::State,
    conclusion_scroll: UniformListScrollHandle,
    report_scroll: UniformListScrollHandle,
    conclusion_horizontal: ScrollHandle,
    report_horizontal: ScrollHandle,
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
            Field::ServerName,
            Field::Hops,
            Field::Samples,
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
            topology: Default::default(),
            conclusion_scroll: UniformListScrollHandle::new(),
            report_scroll: UniformListScrollHandle::new(),
            conclusion_horizontal: ScrollHandle::new(),
            report_horizontal: ScrollHandle::new(),
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
    /// The terminal form's keys. Native boxes own Target, DNS server and Count text;
    /// the shared model owns egress, kind and record choices.
    /// Capture only page actions, so ordinary editing stays with the focused input.
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
        if stroke.key == "p" {
            self.add_project(cx);
            cx.stop_propagation();
            return;
        }
        let action = matches!(stroke.key.as_str(), "d" | "r" | "o" | "y" | "e" | "l");
        let navigation = self.focus.is_focused(window)
            && matches!(stroke.key.as_str(), "left" | "right" | "up" | "down");
        if action || navigation {
            if let Some(key) = crate::keys::from_keystroke(stroke, event.is_held) {
                let section = self.tools(cx, |v| v.section);
                self.mutate(
                    |v| {
                        v.key(key);
                        Ok(())
                    },
                    cx,
                );
                self.sync_inputs(window, cx);
                if self.tools(cx, |v| v.section) != section {
                    // A section switch removes the form and its focused input.
                    window.focus(&self.focus, cx);
                }
            }
            cx.stop_propagation();
        }
    }
    fn key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let stroke = &event.keystroke;
        if stroke.modifiers.alt || stroke.modifiers.platform {
            return;
        }
        if self
            .tools(cx, |v| v.section == Section::Findings)
            .unwrap_or(false)
        {
            if let Some(key) = crate::keys::from_keystroke(stroke, event.is_held) {
                if stroke.key == "escape" && !self.tools(cx, |v| v.running()).unwrap_or(false) {
                    self.back(cx);
                } else {
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
            "escape" if running => self.mutate(
                |v| {
                    v.stop();
                    Ok(())
                },
                cx,
            ),
            "escape" => self.back(cx),
            "enter" if running => self.mutate(
                |v| {
                    v.stop();
                    Ok(())
                },
                cx,
            ),
            "enter" => self.start(cx),
            _ => {
                if stroke.modifiers.control && stroke.key != "u" {
                    return;
                }
                let Some(key) = crate::keys::from_keystroke(stroke, event.is_held) else {
                    return;
                };
                let navigation = matches!(stroke.key.as_str(), "up" | "down" | "tab");
                let field = self.tools(cx, |v| v.field).unwrap_or(Field::Egress);
                let owned = Field::editable;
                if !navigation && owned(field) {
                    // Typing on a text row starts editing its native box.
                    if let Some((_, input)) = self.inputs.iter().find(|(f, _)| *f == field) {
                        input.update(cx, |input, cx| input.focus(window, cx));
                    }
                    cx.stop_propagation();
                    return;
                }
                // Forward only form navigation and ordinary editing keys.
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
    fn add_project(&mut self, cx: &mut Context<Self>) {
        let values = self
            .inputs
            .iter()
            .map(|(field, input)| (*field, input.read(cx).value().to_string()))
            .collect::<Vec<_>>();
        self.mutate(
            move |v| {
                let visible = v.fields();
                for (field, value) in values {
                    if visible.contains(&field) {
                        v.set_text(field, value)?;
                    }
                }
                v.add_project()
            },
            cx,
        );
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
        let section = view.section;
        let show_advanced = view.show_advanced;
        let mut tabs = div().flex().flex_wrap().gap_2().flex_shrink_0();
        for (index, choice) in Section::ALL.into_iter().enumerate() {
            let mut button = Button::new(("network-section", index))
                .label(choice.label())
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.mutate(
                        |v| {
                            v.section = choice;
                            v.scroll = 0;
                            Ok(())
                        },
                        cx,
                    );
                }));
            if choice == section {
                button = button.primary();
            }
            tabs = tabs.child(button);
        }
        tabs = tabs.child(
            Button::new("network-advanced")
                .debug_selector(|| "network-advanced".into())
                .label(if show_advanced {
                    "收起原始报告 (Ctrl+D)"
                } else {
                    "显示原始报告 (Ctrl+D)"
                })
                .on_click(cx.listener(|view, _, _, cx| {
                    view.mutate(
                        |v| {
                            v.toggle_advanced();
                            Ok(())
                        },
                        cx,
                    );
                })),
        );
        root = root.child(tabs).child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child("Ctrl+L 切换分区 · 绿色成功 · 黄色需留意 · 红色失败 · 蓝色信息"),
        );
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
        let conclusions = std::sync::Arc::new(view.conclusion_rows());
        let report = std::sync::Arc::new(view.report_rows());
        let physical_choices = view.physical_choices();
        let values = fields
            .iter()
            .map(|field| (*field, view.field_value(*field), view.field_hint(*field)))
            .collect::<Vec<_>>();
        let nat_ready = kind != Kind::Nat || view.request().is_ok();
        let project_rows = view.project_rows();
        let comparison = view.plan.comparison;
        let plan_ready = view.batch_request().is_ok();
        let mut form = div()
            .id("network-form-scroll")
            .flex()
            .flex_col()
            .gap_2()
            .flex_shrink_0()
            .max_h(px(370.))
            .overflow_y_scroll();
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
                    row = row.child(div().w(px(110.)).child(t("Egress"))).child(
                        Button::new("egress-picker")
                            .label(if query.is_empty() {
                                format!("{} ▾", selected_label)
                            } else {
                                format!("{} ▾  /{}", selected_label, query)
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
                    row = row.child(div().w(px(110.)).child(t(field.label()))).child(
                        Button::new("kind-picker")
                            .label(format!("{} ▾", t(kind.label())))
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.choice_open = if view.choice_open == Some(Field::Kind) {
                                    None
                                } else {
                                    Some(Field::Kind)
                                };
                                cx.notify();
                            })),
                    );
                    if self.choice_open == Some(Field::Kind) {
                        let weak = cx.entity().downgrade();
                        row = row.child(
                            uniform_list("kind-choices", Kind::ALL.len(), move |range, _, _| {
                                range
                                    .map(|index| {
                                        let choice = Kind::ALL[index];
                                        let weak = weak.clone();
                                        Button::new(("probe-kind", index))
                                            .label(format!(
                                                "{} {}",
                                                if choice == kind { "●" } else { "○" },
                                                t(choice.label())
                                            ))
                                            .on_click(move |_, window, cx| {
                                                let _ = weak.update(cx, |view, cx| {
                                                    view.set_kind(choice, window, cx)
                                                });
                                            })
                                    })
                                    .collect()
                            })
                            .h(px(150.))
                            .w_full(),
                        );
                    }
                }
                Field::Record => {
                    row = row.child(div().w(px(110.)).child(t("Record")));
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
                    let value = values
                        .iter()
                        .find(|(key, ..)| *key == field)
                        .map(|(_, value, _)| value.clone())
                        .unwrap_or_default();
                    row = row.child(div().w(px(110.)).child(field.label())).child(
                        Button::new("physical-picker")
                            .label(format!("{value} ▾"))
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
                            row=row.child(div().text_xs().child("没有可确认的目标 Agent 物理网卡；不会使用客户端网卡或自动选择默认出口。"));
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
                Field::Family | Field::HttpMethod | Field::TrustComparison | Field::NatAdvanced => {
                    let value = values
                        .iter()
                        .find(|(key, ..)| *key == field)
                        .map(|(_, value, _)| value.clone())
                        .unwrap_or_default();
                    row = row.child(div().w(px(110.)).child(field.label())).child(
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
                            })),
                    );
                }
                _ => {
                    if let Some((_, input)) = self.inputs.iter().find(|(key, _)| *key == field) {
                        let hint = values
                            .iter()
                            .find(|(key, ..)| *key == field)
                            .map(|(_, _, hint)| *hint)
                            .unwrap_or_default();
                        row = row.child(div().w(px(110.)).child(t(field.label()))).child(
                            div()
                                .flex_1()
                                .min_w(px(160.))
                                .child(Input::new(input))
                                .when(!hint.is_empty(), |column| {
                                    column.child(div().text_xs().child(hint))
                                }),
                        );
                    }
                }
            }
            form = form.child(row);
        }
        if section == Section::Tools {
            root = root.child(form);
            if kind == Kind::Nat {
                root = root.child(div().text_xs().child(
                    "将通过手动选择的物理网卡直接访问测试服务，服务会看到该网络的公网出口 IP。",
                ));
            }
            root = root.child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        Button::new("probe-run")
                            .label(t(if running { "Stop" } else { "Run" }))
                            .disabled(!running && !nat_ready)
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
                        Button::new("probe-add-project")
                            .label("加入诊断计划 (Ctrl+P)")
                            .disabled(matches!(kind, Kind::FakeIp | Kind::Report) || !nat_ready)
                            .on_click(cx.listener(|view, _, _, cx| view.add_project(cx))),
                    )
                    .child(
                        Button::new("probe-back")
                            .label(t("Back"))
                            .on_click(cx.listener(|view, _, _, cx| view.back(cx))),
                    )
                    .when(running, |row| {
                        row.child(
                            Button::new("probe-detach")
                                .label("停止观察 (Ctrl+O)")
                                .on_click(cx.listener(|view, _, _, cx| {
                                    view.mutate(
                                        |v| {
                                            v.stop_observing();
                                            Ok(())
                                        },
                                        cx,
                                    );
                                })),
                        )
                    })
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
                    ),
            );
        }
        if section == Section::Findings {
            root = root.child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        Button::new("plan-new-project")
                            .label("工具箱添加项目")
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.mutate(
                                    |v| {
                                        v.section = Section::Tools;
                                        Ok(())
                                    },
                                    cx,
                                )
                            })),
                    )
                    .child(
                        Button::new("plan-presets")
                            .label("加入五项可编辑预设 (Ctrl+R)")
                            .on_click(
                                cx.listener(|view, _, _, cx| view.mutate(|v| v.add_presets(), cx)),
                            ),
                    )
                    .child(
                        Button::new("plan-comparison")
                            .label(if comparison {
                                "DNS/TLS 对照：开（两轮）"
                            } else {
                                "DNS/TLS 对照：关"
                            })
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.mutate(
                                    |v| {
                                        v.plan.comparison = !v.plan.comparison;
                                        Ok(())
                                    },
                                    cx,
                                )
                            })),
                    )
                    .child(
                        Button::new("plan-path")
                            .label(format!("复制目标出口：{selected_label} ›"))
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.mutate(
                                    |v| {
                                        v.field = Field::Egress;
                                        v.cycle(1);
                                        Ok(())
                                    },
                                    cx,
                                )
                            })),
                    )
                    .child(
                        Button::new("plan-copy-path")
                            .label("复制普通项目到此出口")
                            .disabled(project_rows.is_empty())
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.mutate(|v| v.copy_plan_path(), cx)
                            })),
                    )
                    .child(
                        Button::new("plan-run")
                            .label(if running {
                                "取消诊断"
                            } else {
                                "运行诊断计划"
                            })
                            .disabled(!running && !plan_ready)
                            .on_click(cx.listener(move |view, _, _, cx| {
                                view.mutate(
                                    |v| {
                                        if running {
                                            v.stop()
                                        } else {
                                            v.run_plan()
                                        }
                                        Ok(())
                                    },
                                    cx,
                                )
                            })),
                    )
                    .child(
                        Button::new("plan-detach")
                            .label("停止观察 (Ctrl+O)")
                            .disabled(!running)
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.mutate(
                                    |v| {
                                        v.stop_observing();
                                        Ok(())
                                    },
                                    cx,
                                )
                            })),
                    )
                    .child(
                        Button::new("plan-copy")
                            .label("复制诊断 (Ctrl+Y)")
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.mutate(
                                    |v| {
                                        v.copy_result(false);
                                        Ok(())
                                    },
                                    cx,
                                )
                            })),
                    )
                    .child(
                        Button::new("plan-json")
                            .label("复制 JSON (Ctrl+E)")
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.mutate(
                                    |v| {
                                        v.copy_result(true);
                                        Ok(())
                                    },
                                    cx,
                                )
                            })),
                    ),
            );
            let mut projects = div()
                .id("plan-projects")
                .flex()
                .flex_col()
                .gap_2()
                .max_h(px(230.))
                .overflow_y_scroll()
                .flex_shrink_0();
            for (index, label) in project_rows.into_iter().enumerate() {
                projects = projects.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .items_center()
                        .child(div().flex_1().min_w(px(160.)).text_xs().child(label))
                        .child(Button::new(("project-edit", index)).label("编辑").on_click(
                            cx.listener(move |view, _, window, cx| {
                                view.mutate(|v| v.edit_project(index), cx);
                                view.sync_inputs(window, cx);
                            }),
                        ))
                        .child(
                            Button::new(("project-delete", index))
                                .label("删除")
                                .on_click(cx.listener(move |view, _, _, cx| {
                                    view.mutate(
                                        |v| {
                                            v.remove_project(index);
                                            Ok(())
                                        },
                                        cx,
                                    )
                                })),
                        ),
                );
            }
            root = root.child(projects);
        }
        if let Some(error) = &self.error {
            root = root.child(div().text_color(cx.theme().danger).child(error.clone()));
        }
        let conclusions = crate::widgets::surface(cx)
            .id("network-conclusions-column")
            .debug_selector(|| "network-conclusions-column".into())
            .w(relative(if show_advanced { 0.4 } else { 1.0 }))
            .flex_shrink_0()
            .min_w_0()
            .flex()
            .flex_col()
            .overflow_hidden()
            .p_2()
            .gap_2()
            .child(div().text_sm().child("结论"))
            .child(result_list(
                "network-conclusions-scroll",
                "probe-conclusions",
                conclusions,
                &self.conclusion_scroll,
                &self.conclusion_horizontal,
                cx,
            ));
        let mut columns = div()
            .id("network-results-columns")
            .flex()
            .w_full()
            .min_w_0()
            .min_h(px(300.))
            .flex_1()
            .flex_shrink_0()
            .gap_3()
            .child(conclusions);
        if show_advanced {
            let mut report_column = crate::widgets::surface(cx)
                .id("network-report-column")
                .debug_selector(|| "network-report-column".into())
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .overflow_hidden()
                .p_2()
                .gap_2()
                .child(div().text_sm().child("原始报告"));
            if section == Section::Paths {
                report_column = report_column.child(
                    div()
                        .id("network-report-topology")
                        .debug_selector(|| "network-report-topology".into())
                        .max_h(px(180.))
                        .min_w_0()
                        .overflow_scroll()
                        .child(self.topology_view(cx)),
                );
            }
            report_column = report_column.child(result_list(
                "network-report-scroll",
                "probe-report",
                report,
                &self.report_scroll,
                &self.report_horizontal,
                cx,
            ));
            columns = columns.child(report_column);
        }
        root.capture_key_down(cx.listener(Self::shortcut))
            .on_key_down(cx.listener(Self::key))
            .child(columns)
    }
}

#[cfg(test)]
#[path = "nettools_tests.rs"]
mod tests;
