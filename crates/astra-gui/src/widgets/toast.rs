use gpui::{
    div, App, Context, IntoElement, ParentElement, Render, RenderOnce, SharedString, Styled, Task,
    Window,
};
use gpui_component::ActiveTheme;
use std::time::Duration;

const LIFETIME: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastKind {
    Error,
    Info,
    Success,
}

#[derive(Clone, IntoElement)]
pub struct Toast {
    kind: ToastKind,
    message: SharedString,
}
impl Toast {
    pub fn new(kind: ToastKind, message: impl Into<SharedString>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}
impl RenderOnce for Toast {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let color = match self.kind {
            ToastKind::Error => cx.theme().danger,
            ToastKind::Info => cx.theme().info,
            ToastKind::Success => cx.theme().success,
        };
        // Deliberately static. Hover/focus never prolong the five-second lifetime.
        div()
            .p_3()
            .rounded_lg()
            .border_1()
            .border_color(color)
            .bg(cx.theme().popover)
            .text_color(cx.theme().popover_foreground)
            .child(self.message)
    }
}

/// Per-window presentation state. Replacing a toast cancels its previous timer.
#[derive(Default)]
pub struct ToastHost {
    toast: Option<Toast>,
    expiry: Option<Task<()>>,
}
impl ToastHost {
    pub fn show(
        &mut self,
        kind: ToastKind,
        message: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.toast = Some(Toast::new(kind, message));
        self.expiry = Some(cx.spawn(async move |host, cx| {
            cx.background_executor().timer(LIFETIME).await;
            let _ = host.update(cx, |host, cx| {
                host.toast = None;
                cx.notify();
            });
        }));
        cx.notify();
    }
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.expiry = None;
        self.toast = None;
        cx.notify();
    }
    pub fn is_visible(&self) -> bool {
        self.toast.is_some()
    }
}
impl Render for ToastHost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().children(self.toast.clone())
    }
}
