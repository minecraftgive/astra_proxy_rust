use astra_client::i18n::t;
use gpui::{div, App, ClickEvent, IntoElement, ParentElement, RenderOnce, Styled, Window};
use gpui_component::button::{Button, ButtonVariants};

/// Explicit accept/cancel controls; the caller supplies the semantic actions.
#[derive(IntoElement)]
pub struct Confirm {
    accept: Button,
    cancel: Button,
}
impl Confirm {
    pub fn new(accept_selected: bool) -> Self {
        let mut accept = Button::new("prompt-accept").label(t("Confirm"));
        let mut cancel = Button::new("prompt-cancel").label(t("Cancel"));
        if accept_selected {
            accept = accept.primary();
        } else {
            cancel = cancel.primary();
        }
        Self { accept, cancel }
    }
    pub fn on_accept(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.accept = self.accept.on_click(handler);
        self
    }
    pub fn on_cancel(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.cancel = self.cancel.on_click(handler);
        self
    }
}
impl RenderOnce for Confirm {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        div()
            .flex()
            .justify_end()
            .gap_2()
            .child(self.cancel)
            .child(self.accept)
    }
}
