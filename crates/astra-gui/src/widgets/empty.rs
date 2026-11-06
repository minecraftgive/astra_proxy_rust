use gpui::{div, App, IntoElement, ParentElement, RenderOnce, SharedString, Styled, Window};
use gpui_component::ActiveTheme;

#[derive(IntoElement)]
pub struct Empty {
    title: SharedString,
    detail: SharedString,
}
impl Empty {
    /// Both strings are localized by the caller.
    pub fn new(title: impl Into<SharedString>, detail: impl Into<SharedString>) -> Self {
        Self {
            title: title.into(),
            detail: detail.into(),
        }
    }
}
impl RenderOnce for Empty {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2()
            .p_6()
            .child(div().text_lg().child(self.title))
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(self.detail),
            )
    }
}
