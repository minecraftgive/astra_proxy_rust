use crate::{
    model::AppModel,
    views::ViewEvent,
    widgets::{Card, Empty},
};
use astra_client::{
    i18n::t,
    state::{Client, Input as ClientInput},
};
use gpui::{
    div, App, Context, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement,
    IntoElement, KeyDownEvent, ParentElement, Render, StatefulInteractiveElement, Styled,
    Subscription, Window,
};
use gpui_component::button::Button;
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
                client.page = astra_client::navigation::PROFILES;
                action(client)
            });
            cx.notify();
            input
        });
        let page = self.model.read(cx).client.page;
        if page != astra_client::navigation::PROFILES {
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
    pub fn action(&mut self, action: astra_client::intents::ServerAction, cx: &mut Context<Self>) {
        self.dispatch(|client| client.server_action(action), cx);
    }
}
impl Render for View {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _ = &self.observation;
        let summary = self.model.read(cx).client.selected_server_summary();
        let mut content = div()
            .id("server-view")
            .key_context("AstraServer")
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
                    || event.keystroke.modifiers.platform
                {
                    return;
                }
                use astra_client::intents::ServerAction;
                let action = match event.keystroke.key.as_str() {
                    "c" => ServerAction::Toggle,
                    "r" => ServerAction::Reload,
                    "e" => ServerAction::Edit,
                    "l" if event.keystroke.modifiers.shift => ServerAction::Logs,
                    "v" => ServerAction::Validate,
                    _ => return,
                };
                view.action(action, cx);
                cx.stop_propagation();
            }));
        if let Some(summary) = summary {
            let details = summary.details.clone();
            content = content.child(super::profiles::instance_card(summary, cx));
            if let Some(status) = details.and_then(|details| details.server_status) {
                content = content.child(
                    Card::new(t("Server"))
                        .child(div().child(format!(
                            "{}: {} / {} / {}",
                            t("Connections"),
                            status.active,
                            status.accepted,
                            status.rejected
                        )))
                        .child(div().child(format!(
                            "↑ {} · ↓ {}",
                            astra_client::live::decimal_bytes(&status.upload),
                            astra_client::live::decimal_bytes(&status.download)
                        ))),
                );
            }
            let mut toolbar = div().flex().flex_wrap().gap_2();
            for (index, (label, action)) in [
                ("Start / stop", astra_client::intents::ServerAction::Toggle),
                ("Reload", astra_client::intents::ServerAction::Reload),
                ("Edit", astra_client::intents::ServerAction::Edit),
                (
                    "Validate configuration",
                    astra_client::intents::ServerAction::Validate,
                ),
                ("Logs", astra_client::intents::ServerAction::Logs),
            ]
            .into_iter()
            .enumerate()
            {
                toolbar = toolbar.child(
                    Button::new(("server-action", index))
                        .label(t(label))
                        .on_click(cx.listener(move |view, _, _, cx| view.action(action, cx))),
                );
            }
            content = content.child(toolbar);
        } else {
            content = content
                .child(Empty::new(t("Server"), t("No server selected")))
                .child(
                    Button::new("server-profiles")
                        .label(t("Profiles"))
                        .on_click(cx.listener(|_, _, _, cx| {
                            cx.emit(ViewEvent::Navigate(astra_client::navigation::PROFILES))
                        })),
                );
        }
        content
    }
}
