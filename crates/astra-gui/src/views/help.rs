use super::ViewEvent;
use crate::model::AppModel;
use astra_client::{commands, i18n::t};
use gpui::{prelude::*, *};
use gpui_component::ActiveTheme;
pub struct View {
    focus: FocusHandle,
}
impl EventEmitter<ViewEvent> for View {}
impl Focusable for View {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl View {
    pub fn new(_: Entity<AppModel>, _: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            focus: cx.focus_handle(),
        }
    }
}
impl Render for View {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("help")
            .track_focus(&self.focus)
            .size_full()
            .overflow_y_scroll()
            .p_6()
            .flex()
            .flex_col()
            .gap_4()
            .child(div().text_xl().child(t("Keyboard help")))
            .children(commands::help_groups().into_iter().map(|(label, specs)| {
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(div().text_lg().child(t(label)))
                    .children(specs.into_iter().map(|spec| {
                        div()
                            .flex()
                            .gap_4()
                            .child(
                                div()
                                    .w(px(180.))
                                    .text_color(cx.theme().muted_foreground)
                                    .child(spec.key),
                            )
                            .child(t(spec.label))
                    }))
            }))
    }
}
