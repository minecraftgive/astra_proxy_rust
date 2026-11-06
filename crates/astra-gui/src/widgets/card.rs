use gpui::{
    div, AnyElement, App, IntoElement, ParentElement, RenderOnce, SharedString, Styled, Window,
};
use gpui_component::ActiveTheme;

#[derive(IntoElement)]
pub struct Card {
    title: SharedString,
    children: Vec<AnyElement>,
}
impl Card {
    /// The title is already localized by the caller.
    pub fn new(title: impl Into<SharedString>) -> Self {
        Self {
            title: title.into(),
            children: vec![],
        }
    }
}
impl ParentElement for Card {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}
impl RenderOnce for Card {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .min_w_0()
            .gap_3()
            .p_4()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(self.title),
            )
            .children(self.children)
    }
}
