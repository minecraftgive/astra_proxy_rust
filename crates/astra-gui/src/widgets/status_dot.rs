use astra_client::i18n::t;
use gpui::{div, px, App, IntoElement, ParentElement, RenderOnce, Styled, Window};
use gpui_component::ActiveTheme;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Running,
    Stopped,
    Starting,
    Error,
}

/// A status includes a text label so color is never its only indication.
#[derive(IntoElement)]
pub struct StatusDot {
    status: Status,
}
impl StatusDot {
    pub fn new(status: Status) -> Self {
        Self { status }
    }
}
impl RenderOnce for StatusDot {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let (label, color) = match self.status {
            Status::Running => (t("Running"), cx.theme().success),
            Status::Stopped => (t("Stopped"), cx.theme().muted_foreground),
            Status::Starting => (t("Starting"), cx.theme().warning),
            Status::Error => (t("Error"), cx.theme().danger),
        };
        div()
            .flex()
            .items_center()
            .gap_2()
            .text_sm()
            .child(div().size(px(8.)).rounded_full().bg(color))
            .child(label.to_owned())
    }
}
