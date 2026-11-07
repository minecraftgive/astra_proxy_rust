//! Web UI layout primitives: metric tiles, pills, notice bars and sidebar items.
use crate::theme::palette;
use gpui::{
    div, prelude::FluentBuilder, px, AnyElement, App, ClickEvent, ElementId, FontWeight,
    InteractiveElement, IntoElement, ParentElement, RenderOnce, SharedString,
    StatefulInteractiveElement, Styled, Window,
};

/// `.card.metric`: a muted label above a large tabular value.
#[derive(IntoElement)]
pub struct Metric {
    label: SharedString,
    value: SharedString,
    detail: Option<AnyElement>,
}
impl Metric {
    pub fn new(label: impl Into<SharedString>, value: impl Into<SharedString>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
            detail: None,
        }
    }
    /// A small element below the value, such as a sparkline.
    pub fn detail(mut self, detail: impl IntoElement) -> Self {
        self.detail = Some(detail.into_any_element());
        self
    }
}
impl RenderOnce for Metric {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = palette(cx);
        div()
            .flex_1()
            .min_w(px(150.))
            .flex()
            .flex_col()
            .gap_2()
            .p_4()
            .rounded_lg()
            .border_1()
            .border_color(p.border)
            .bg(p.surface)
            .child(div().text_sm().text_color(p.muted).child(self.label))
            .child(
                div()
                    .text_size(px(24.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(p.text)
                    .whitespace_nowrap()
                    .child(self.value),
            )
            .children(self.detail)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Neutral,
    Accent,
    Ok,
    Warn,
    Err,
}

/// `.pill`: a rounded label for kinds, modes and states.
#[derive(IntoElement)]
pub struct Pill {
    label: SharedString,
    tone: Tone,
}
impl Pill {
    pub fn new(label: impl Into<SharedString>) -> Self {
        Self {
            label: label.into(),
            tone: Tone::Neutral,
        }
    }
    pub fn tone(mut self, tone: Tone) -> Self {
        self.tone = tone;
        self
    }
}
impl RenderOnce for Pill {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = palette(cx);
        let (bg, fg) = match self.tone {
            Tone::Neutral => (p.surface_2, p.muted),
            Tone::Accent => (p.accent_soft, p.accent),
            Tone::Ok => (p.ok_soft, p.ok),
            Tone::Warn => (p.warn_soft, p.warn),
            Tone::Err => (p.err_soft, p.err),
        };
        div()
            .flex_none()
            .px_2()
            .rounded_full()
            .text_xs()
            .whitespace_nowrap()
            .bg(bg)
            .text_color(fg)
            .child(self.label)
    }
}

/// `.notice-bar`: a page-wide message with optional actions on the right.
#[derive(IntoElement)]
pub struct NoticeBar {
    message: SharedString,
    tone: Tone,
    actions: Vec<AnyElement>,
}
impl NoticeBar {
    pub fn new(message: impl Into<SharedString>, tone: Tone) -> Self {
        Self {
            message: message.into(),
            tone,
            actions: vec![],
        }
    }
    pub fn action(mut self, action: impl IntoElement) -> Self {
        self.actions.push(action.into_any_element());
        self
    }
}
impl RenderOnce for NoticeBar {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = palette(cx);
        let (bg, fg) = match self.tone {
            Tone::Neutral | Tone::Accent => (p.surface_2, p.text),
            Tone::Ok => (p.ok_soft, p.ok),
            Tone::Warn => (p.warn_soft, p.warn),
            Tone::Err => (p.err_soft, p.err),
        };
        div()
            .flex()
            .items_center()
            .gap_3()
            .px_4()
            .py_3()
            .rounded_lg()
            .border_1()
            .border_color(p.border)
            .bg(bg)
            .text_color(fg)
            .child(div().flex_1().min_w_0().child(self.message))
            .children(self.actions)
    }
}

type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

/// `.nav`: a sidebar entry with its shortcut aligned right.
#[derive(IntoElement)]
pub struct NavItem {
    id: ElementId,
    label: SharedString,
    shortcut: Option<SharedString>,
    active: bool,
    on_click: Option<ClickHandler>,
}
impl NavItem {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            shortcut: None,
            active: false,
            on_click: None,
        }
    }
    pub fn shortcut(mut self, shortcut: impl Into<SharedString>) -> Self {
        self.shortcut = Some(shortcut.into());
        self
    }
    pub fn active(mut self, active: bool) -> Self {
        self.active = active;
        self
    }
    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Box::new(handler));
        self
    }
}
impl RenderOnce for NavItem {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = palette(cx);
        let hover = p.surface_2;
        div()
            .id(self.id)
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py(px(7.))
            .rounded_md()
            .text_sm()
            .cursor_pointer()
            .when(self.active, |item| {
                item.bg(p.accent_soft)
                    .text_color(p.accent)
                    .font_weight(FontWeight::MEDIUM)
            })
            .when(!self.active, |item| {
                item.text_color(p.muted)
                    .hover(move |style| style.bg(hover).text_color(p.text))
            })
            .child(div().flex_1().min_w_0().child(self.label))
            .when_some(self.shortcut, |item, key| {
                item.child(div().text_xs().text_color(p.faint).child(key))
            })
            .when_some(self.on_click, |item, handler| item.on_click(handler))
    }
}

/// A bordered `.card` surface for lists and panes that need their own sizing.
pub fn surface(cx: &App) -> gpui::Div {
    let p = palette(cx);
    div()
        .flex()
        .flex_col()
        .min_h_0()
        .min_w_0()
        .rounded_lg()
        .border_1()
        .border_color(p.border)
        .bg(p.surface)
        .overflow_hidden()
}
