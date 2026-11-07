use crate::{model::AppModel, views::ViewEvent, widgets::Card};
use astra_client::{
    i18n::t,
    state::{Client, Input as ClientInput},
};
use gpui::{
    div, App, Context, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement,
    IntoElement, KeyDownEvent, ParentElement, Render, StatefulInteractiveElement, Styled,
    Subscription, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants},
    ActiveTheme,
};
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
            let input = model.tokio(action);
            cx.notify();
            input
        });
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
    pub fn recover(
        &mut self,
        action: astra_client::prompts::RecoveryAction,
        cx: &mut Context<Self>,
    ) {
        self.dispatch(|client| client.recover(action), cx);
    }
}
impl Render for View {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _ = &self.observation;
        let message = self
            .model
            .read(cx)
            .client
            .recovery
            .as_ref()
            .map(|recovery| recovery.message.clone())
            .unwrap_or_default();
        div()
            .id("recovery-view")
            .key_context("AstraDialog")
            .track_focus(&self.focus)
            .flex()
            .flex_col()
            .size_full()
            .gap_4()
            .p_6()
            .overflow_y_scroll()
            .on_key_down(cx.listener(|view, event: &KeyDownEvent, window, cx| {
                if !view.focus.is_focused(window) || event.is_held {
                    return;
                }
                let action = match event.keystroke.key.as_str() {
                    "escape" | "b" => astra_client::prompts::RecoveryAction::Back,
                    "c" => astra_client::prompts::RecoveryAction::Continue,
                    _ => return,
                };
                view.recover(action, cx);
                cx.stop_propagation();
            }))
            .child(
                Card::new(t("Recovery"))
                    .child(div().text_color(cx.theme().danger).child(message))
                    .child(
                        div()
                            .flex()
                            .gap_3()
                            .child(Button::new("recovery-back").label(t("Back")).on_click(
                                cx.listener(|view, _, _, cx| {
                                    view.recover(astra_client::prompts::RecoveryAction::Back, cx)
                                }),
                            ))
                            .child(
                                Button::new("recovery-continue")
                                    .primary()
                                    .label(t("Continue"))
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        view.recover(
                                            astra_client::prompts::RecoveryAction::Continue,
                                            cx,
                                        )
                                    })),
                            ),
                    ),
            )
    }
}
