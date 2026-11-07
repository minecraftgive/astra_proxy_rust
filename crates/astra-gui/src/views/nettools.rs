use crate::{model::AppModel, views::ViewEvent, widgets::Empty};
use astra_client::{
    i18n::t,
    nettools::{Field, Kind, RECORDS},
};
use gpui::{
    div, px, uniform_list, App, AppContext, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, Render, Styled, Subscription, Window,
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
            observations.push(
                cx.subscribe(&input, move |view, state, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        let value = state.read(cx).value().to_string();
                        view.mutate(move |v| v.set_text(field, value), cx);
                    }
                }),
            );
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
        let selected_label = astra_client::nettools::egress_label(&view.egresses[selected]);
        let choices =
            std::sync::Arc::new(view.egress_choices(self.egress.read(cx).value().as_ref()));
        let output = std::sync::Arc::new(view.output());
        for field in fields {
            let mut row = div().flex().gap_2().items_center();
            match field {
                Field::Egress => {
                    let choices = choices.clone();
                    let weak = cx.entity().downgrade();
                    row = row.child(div().w(px(110.)).child(t("Egress"))).child(
                        Button::new("egress-picker")
                            .label(format!("{} ▾", selected_label))
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
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.model.update(cx, |model, cx| {
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
                        })),
                ),
        );
        if let Some(error) = &self.error {
            root = root.child(div().text_color(cx.theme().danger).child(error.clone()));
        }
        root.child(
            uniform_list("probe-output", output.len(), move |range, _, _| {
                range
                    .map(|i| {
                        div()
                            .h(px(28.))
                            .whitespace_nowrap()
                            .child(output[i].clone())
                    })
                    .collect()
            })
            .flex_1(),
        )
    }
}
