use crate::{model::AppModel, views::ViewEvent, widgets::Empty};
use astra_client::{
    i18n::t,
    nettools::{Field, Kind, RECORDS},
};
use gpui::{
    div, prelude::FluentBuilder, px, uniform_list, App, AppContext, Context, Entity, EventEmitter,
    FocusHandle, Focusable, InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render,
    Styled, Subscription, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants},
    input::{Input, InputEvent, InputState},
    ActiveTheme,
};
pub struct View {
    model: Entity<AppModel>,
    focus: FocusHandle,
    inputs: Vec<(Field, Entity<InputState>)>,
    egress: Entity<InputState>,
    observations: Vec<Subscription>,
    error: Option<String>,
    egress_open: bool,
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
        for field in [Field::Target, Field::Server, Field::Count] {
            let value = model
                .read(cx)
                .client
                .diagnostics
                .as_ref()
                .and_then(|d| d.tools.as_ref())
                .map(|v| match field {
                    Field::Target => v.target.clone(),
                    Field::Server => v.server.clone(),
                    _ => v.count.to_string(),
                })
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
        let values = self
            .model
            .read(cx)
            .client
            .diagnostics
            .as_ref()
            .and_then(|d| d.tools.as_ref())
            .map(|v| (v.target.clone(), v.count.to_string()));
        if let Some((target, count)) = values {
            for (field, input) in &self.inputs {
                if *field == Field::Target || *field == Field::Count {
                    let value = if *field == Field::Target {
                        target.clone()
                    } else {
                        count.clone()
                    };
                    input.update(cx, |input, cx| input.set_value(value, window, cx));
                }
            }
        }
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
            [v.target.clone(), v.server.clone(), v.count.to_string()]
        }) else {
            return;
        };
        for (field, input) in &self.inputs {
            let value = match field {
                Field::Target => &values[0],
                Field::Server => &values[1],
                _ => &values[2],
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
                let owned =
                    |field: Field| matches!(field, Field::Target | Field::Server | Field::Count);
                if !navigation && owned(field) {
                    // Typing on a text row starts editing its native box.
                    if let Some((_, input)) = self.inputs.iter().find(|(f, _)| *f == field) {
                        input.update(cx, |input, cx| input.focus(window, cx));
                    }
                    cx.stop_propagation();
                    return;
                }
                // Function keys and other global shortcuts keep working on this page.
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
                for (field, value) in values {
                    v.set_text(field, value)?;
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
        let output = std::sync::Arc::new(view.output());
        for field in fields {
            let mut row = div()
                .flex()
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
                    row = row.child(div().w(px(110.)).child(t("Test")));
                    for (index, choice) in Kind::ALL.into_iter().enumerate() {
                        let mut button = Button::new(("probe-kind", index))
                            .label(t(choice.label()))
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
                _ => {
                    let label = match field {
                        Field::Target => "Target",
                        Field::Server => "DNS server",
                        _ => "Count",
                    };
                    if let Some((_, input)) = self.inputs.iter().find(|(f, _)| *f == field) {
                        row = row
                            .child(div().w(px(110.)).child(t(label)))
                            .child(Input::new(input));
                    }
                }
            }
            root = root.child(row);
        }
        root = root.child(
            div()
                .flex()
                .gap_2()
                .child(
                    Button::new("probe-run")
                        .label(t(if running { "Stop" } else { "Run" }))
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
                    Button::new("probe-back")
                        .label(t("Back"))
                        .on_click(cx.listener(|view, _, _, cx| view.back(cx))),
                ),
        );
        if let Some(error) = &self.error {
            root = root.child(div().text_color(cx.theme().danger).child(error.clone()));
        }
        let surface = crate::widgets::surface(cx).flex_1().px_4().py_2();
        let mono = crate::theme::mono(cx);
        root.on_key_down(cx.listener(Self::key)).child(
            surface.child(
                uniform_list("probe-output", output.len(), move |range, _, _| {
                    range
                        .map(|i| {
                            div()
                                .h(px(24.))
                                .text_xs()
                                .font_family(mono.clone())
                                .whitespace_nowrap()
                                .child(output[i].clone())
                        })
                        .collect()
                })
                .flex_1(),
            ),
        )
    }
}
