use crate::{
    model::AppModel,
    views::ViewEvent,
    widgets::{Card, Empty},
};
use astra_client::{
    i18n::t,
    state::{Client, Input as ClientInput, Operation},
};
use gpui::{
    div, prelude::FluentBuilder, px, uniform_list, App, Context, Entity, EventEmitter, FocusHandle,
    Focusable, InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render,
    StatefulInteractiveElement, Styled, Subscription, Window,
};
use gpui_component::button::{Button, ButtonVariants};
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
    fn dispatch(
        &mut self,
        action: impl FnOnce(&mut Client) -> ClientInput,
        cx: &mut Context<Self>,
    ) {
        let input = self.model.update(cx, |model, cx| {
            let input = model.tokio(|client| {
                client.page = astra_client::navigation::RULES;
                action(client)
            });
            cx.notify();
            input
        });
        let page = self.model.read(cx).client.page;
        if page != astra_client::navigation::RULES {
            cx.emit(ViewEvent::Navigate(page));
        }
        cx.emit(ViewEvent::Input(input));
        cx.notify();
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
    pub fn operation(&mut self, operation: Operation, cx: &mut Context<Self>) {
        self.dispatch(|client| client.rule_operation(operation), cx);
    }
}
impl Render for View {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _ = &self.observation;
        let client = &self.model.read(cx).client;
        let explanation = client.explanation.as_ref().map(|view| view.lines());
        let summary = client.rule_summary();
        let empty = client.rule_empty_message();
        let mut content = div()
            .id("rules-view")
            .key_context("AstraLive")
            .track_focus(&self.focus)
            .flex()
            .flex_col()
            .gap_4()
            .size_full()
            .overflow_y_scroll()
            .on_key_down(cx.listener(|view, event: &KeyDownEvent, window, cx| {
                if !view.focus.is_focused(window)
                    || event.is_held
                    || event.keystroke.modifiers.control
                    || event.keystroke.modifiers.alt
                {
                    return;
                }
                match event.keystroke.key.as_str() {
                    "t" | "enter" => view.operation(Operation::Explain, cx),
                    "e" => view.operation(Operation::Edit, cx),
                    "/" => view.dispatch(Client::rule_search, cx),
                    _ => return,
                };
                cx.stop_propagation();
            }));
        if let Some(summary) = summary {
            let details = summary.details.clone();
            content = content.child(super::profiles::instance_card(summary, cx));
            // The rule set card carries its own actions, like the web UI's card tools.
            let card = Card::new(t("Rules"))
                .action(
                    Button::new("rule-search")
                        .ghost()
                        .label(t("Search"))
                        .on_click(
                            cx.listener(|view, _, _, cx| view.dispatch(Client::rule_search, cx)),
                        ),
                )
                .action(
                    Button::new("rule-logs").ghost().label(t("Logs")).on_click(
                        cx.listener(|view, _, _, cx| view.operation(Operation::Logs, cx)),
                    ),
                )
                .action(
                    Button::new("rule-edit").label(t("Edit rules")).on_click(
                        cx.listener(|view, _, _, cx| view.operation(Operation::Edit, cx)),
                    ),
                )
                .action(
                    Button::new("rule-test")
                        .primary()
                        .label(t("Test a route"))
                        .on_click(
                            cx.listener(|view, _, _, cx| view.operation(Operation::Explain, cx)),
                        ),
                )
                .when_some(details, |card, details| {
                    card.child(div().child(astra_client::tr!("{} rules", details.rules)))
                });
            content = content.child(card);
        } else {
            content = content.child(Empty::new(t("Rules"), empty)).child(
                Button::new("rule-profiles")
                    .label(t("Profiles"))
                    .on_click(cx.listener(|_, _, _, cx| {
                        cx.emit(ViewEvent::Navigate(astra_client::navigation::PROFILES))
                    })),
            );
        }
        if let Some(lines) = explanation {
            let lines: std::sync::Arc<[_]> = lines.into();
            let report = uniform_list("route-report", lines.len(), move |range, _, _| {
                range
                    .map(|index| div().h(px(72.)).text_sm().child(lines[index].clone()))
                    .collect::<Vec<_>>()
            })
            .h(px(360.));
            content = content.child(
                Card::new(t("Route explanation")).child(report).child(
                    Button::new("close-route-report")
                        .label(t("Close"))
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.dispatch(
                                |client| {
                                    client.close_explanation();
                                    ClientInput::Idle
                                },
                                cx,
                            )
                        })),
                ),
            );
        }
        content
    }
}
