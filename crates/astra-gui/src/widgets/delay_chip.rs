use gpui::{div, App, IntoElement, ParentElement, RenderOnce, Styled, Window};
use gpui_component::ActiveTheme;

/// None is a timeout or missing measurement. The caller owns measurement state.
#[derive(IntoElement)]
pub struct DelayChip {
    milliseconds: Option<u64>,
}
impl DelayChip {
    pub fn new(milliseconds: Option<u64>) -> Self {
        Self { milliseconds }
    }
}
impl RenderOnce for DelayChip {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let color = match self.milliseconds {
            Some(0..=99) => cx.theme().success,
            Some(100..=299) => cx.theme().warning,
            Some(_) => cx.theme().danger,
            None => cx.theme().muted_foreground,
        };
        let text = self
            .milliseconds
            .map(|value| astra_client::tr!("{} ms", value))
            .unwrap_or_else(|| "—".into());
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .text_xs()
            .border_1()
            .border_color(color)
            .text_color(color)
            .child(text)
    }
}
