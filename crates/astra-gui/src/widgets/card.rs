use crate::theme::palette;
use gpui::{
    div, prelude::FluentBuilder, AnyElement, App, FontWeight, IntoElement, ParentElement,
    RenderOnce, SharedString, Styled, Window,
};

/// The web UI's `.card.card-pad` with an `h2` heading and optional header actions.
#[derive(IntoElement)]
pub struct Card {
    title: SharedString,
    actions: Vec<AnyElement>,
    children: Vec<AnyElement>,
}
impl Card {
    /// The title is already localized by the caller; empty omits the heading.
    pub fn new(title: impl Into<SharedString>) -> Self {
        Self {
            title: title.into(),
            actions: vec![],
            children: vec![],
        }
    }
    /// Controls shown at the right of the heading, like `.page-head .tools`.
    pub fn action(mut self, action: impl IntoElement) -> Self {
        self.actions.push(action.into_any_element());
        self
    }
}
impl ParentElement for Card {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}
impl RenderOnce for Card {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = palette(cx);
        let heading = !self.title.is_empty() || !self.actions.is_empty();
        div()
            .flex()
            .flex_col()
            .min_w_0()
            .gap_3()
            .p_4()
            .rounded_lg()
            .border_1()
            .border_color(p.border)
            .bg(p.surface)
            .when(heading, |card| {
                card.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(gpui::px(15.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(p.text)
                                .child(self.title),
                        )
                        .children(self.actions),
                )
            })
            .children(self.children)
    }
}
