//! Update transactions reuse the same capability checks and confirmation prompts as the TUI.
use super::ViewEvent;
use crate::{
    model::AppModel,
    widgets::{Card, Empty, TokenText},
};
use astra_client::{
    i18n::t,
    update::{Command, View},
};
use gpui::{
    div, prelude::*, px, uniform_list, App, Context, Entity, EventEmitter, FocusHandle, Focusable,
    Render, Subscription, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants},
    ActiveTheme, Disableable,
};

pub struct UpdatesView {
    model: Entity<AppModel>,
    focus: FocusHandle,
    _observations: Vec<Subscription>,
}
impl EventEmitter<ViewEvent> for UpdatesView {}
impl Focusable for UpdatesView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl UpdatesView {
    pub fn new(model: Entity<AppModel>, _: &mut Window, cx: &mut Context<Self>) -> Self {
        let observation = cx.observe(&model, |_, _, cx| cx.notify());
        Self {
            model,
            focus: cx.focus_handle(),
            _observations: vec![observation],
        }
    }
    fn command(&mut self, command: Command, cx: &mut Context<Self>) {
        let input = self.model.update(cx, |model, cx| {
            let input = model.tokio(|client| command.run(client));
            cx.notify();
            input
        });
        cx.emit(ViewEvent::Input(input));
    }
}
impl Render for UpdatesView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let status = self.model.read(cx).client.update_status.clone();
        let mut toolbar = div().flex().flex_wrap().gap_2();
        for (index, command) in Command::ALL.iter().copied().enumerate() {
            let reason = command.unavailable(status.as_ref());
            let mut button = Button::new(("update-command", index))
                .label(t(command.label()))
                .disabled(reason.is_some());
            if matches!(command, Command::LaunchNow) {
                button = button.disabled(!self.model.read(cx).client.update_launch.pending());
            }
            if matches!(command, Command::Import) {
                button = button.primary();
            }
            if let Some(reason) = reason {
                button = button.tooltip(t(reason));
            }
            toolbar = toolbar.child(
                button.on_click(cx.listener(move |this, _, _, cx| this.command(command, cx))),
            );
        }
        let content = if let Some(status) = status {
            let lines = View::new(status).summary();
            uniform_list("update-status", lines.len(), move |range, _, _| {
                range
                    .map(|index| div().h(px(28.)).child(TokenText::new(lines[index].clone())))
                    .collect::<Vec<_>>()
            })
            .h_full()
            .into_any_element()
        } else {
            Empty::new(t("Update status"), t("Read update status first")).into_any_element()
        };
        div()
            .id("updates-panel")
            .key_context("AstraUpdates")
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .p_4()
            .gap_3()
            .child(Card::new(t("Updates")).child(toolbar))
            .child(div().child(self.model.read(cx).client.update_launch.label()))
            .child(div().child(self.model.read(cx).client.maintenance.label()))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .text_sm()
                    .text_color(cx.theme().foreground)
                    .child(content),
            )
    }
}

#[cfg(test)]
#[path = "../test/updates_view_test.rs"]
mod tests;
