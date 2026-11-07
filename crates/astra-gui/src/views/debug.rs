//! Debug panel: memory composition of this interface and the running cores.
//!
//! The view registers a watcher in the shared client state while it exists and
//! reports its window's visibility; sampling stops when the panel is closed, the
//! window is hidden, minimized or fully covered, or the user pauses it.
use crate::{
    model::AppModel,
    theme::palette,
    views::ViewEvent,
    widgets::{Card, Chart, ChartSeries, Empty, Metric},
};
use astra_client::{
    i18n::t,
    memory::{Composition, Module},
    monitor::bytes,
};
use gpui::{
    div, px, relative, App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    Hsla, InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render,
    StatefulInteractiveElement, Styled, Subscription, Window,
};
use gpui_component::{button::Button, ActiveTheme};

pub struct View {
    model: Entity<AppModel>,
    focus: FocusHandle,
    _subscriptions: [Subscription; 2],
}
impl EventEmitter<ViewEvent> for View {}
impl Focusable for View {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

fn color(module: Module, cx: &App) -> Hsla {
    let theme = cx.theme();
    match module {
        Module::Interface => theme.chart_1,
        Module::Proxy => theme.chart_2,
        Module::Rule => theme.chart_3,
        Module::Server => theme.chart_4,
    }
}

fn percent(share: f64) -> String {
    format!("{:.1}%", share * 100.0)
}

impl View {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let id = cx.entity_id().as_u64();
        let visible = window.is_visible();
        model.update(cx, |model, cx| {
            model.watch_memory(id, visible);
            cx.notify();
        });
        let release_model = model.clone();
        cx.on_release(move |_, cx| {
            release_model.update(cx, |model, cx| {
                model.unwatch_memory(id);
                cx.notify();
            });
        })
        .detach();
        let observation = cx.observe(&model, |_, _, cx| cx.notify());
        let visibility = cx.observe_window_visibility(window, move |view, visibility, _, cx| {
            view.model.update(cx, |model, cx| {
                model.watch_memory(id, visibility.is_visible());
                cx.notify();
            });
        });
        Self {
            model,
            focus: cx.focus_handle(),
            _subscriptions: [observation, visibility],
        }
    }
    fn mutate(
        &mut self,
        action: impl FnOnce(&mut astra_client::memory::View),
        cx: &mut Context<Self>,
    ) {
        self.model.update(cx, |model, cx| {
            model.tokio(|client| {
                if let Some(memory) = &mut client.memory {
                    action(memory);
                }
            });
            cx.notify();
        });
        cx.notify();
    }
    fn toggle_pause(&mut self, cx: &mut Context<Self>) {
        self.mutate(|m| m.set_paused(!m.paused), cx);
    }
}

fn share_bar(composition: &Composition, cx: &App) -> impl IntoElement {
    let p = palette(cx);
    let mut bar = div()
        .w_full()
        .h(px(12.))
        .flex()
        .rounded_full()
        .overflow_hidden()
        .bg(p.surface_2);
    for summary in composition.summaries() {
        if summary.share > 0.0 {
            bar = bar.child(
                div()
                    .h_full()
                    .w(relative(summary.share as f32))
                    .bg(color(summary.module, cx)),
            );
        }
    }
    bar
}

fn module_card(composition: &Composition, module: Module, cx: &App) -> Option<Card> {
    let p = palette(cx);
    let summary = composition
        .summaries()
        .into_iter()
        .find(|summary| summary.module == module)?;
    let heading = div()
        .flex()
        .items_center()
        .gap_2()
        .child(div().size(px(8.)).rounded_full().bg(color(module, cx)))
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .child(bytes(summary.bytes.map(|n| n as f64))),
        )
        .child(div().text_color(p.muted).child(percent(summary.share)));
    let mut card = Card::new(module.label()).child(heading);
    if summary.parts.is_empty() {
        return Some(card.child(div().text_sm().text_color(p.muted).child(t("Not running"))));
    }
    for part in summary.parts {
        let pid = part.pid.map(|pid| format!("PID {pid}")).unwrap_or_default();
        let value = match part.bytes {
            Some(n) => format!(
                "{} · {}",
                bytes(Some(n as f64)),
                percent(composition.share(n))
            ),
            None => "—".into(),
        };
        let mut row = div().flex().flex_col().gap_1().child(
            div()
                .flex()
                .justify_between()
                .gap_4()
                .text_sm()
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .min_w_0()
                        .child(div().truncate().child(part.name.clone()))
                        .child(div().text_color(p.faint).child(pid)),
                )
                .child(div().whitespace_nowrap().child(value)),
        );
        if let Some(error) = &part.error {
            row = row.child(div().text_xs().text_color(p.err).child(error.clone()));
        }
        card = card.child(row);
    }
    Some(card)
}

impl Render for View {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut root = div()
            .id("debug-view")
            .key_context("AstraDebug")
            .track_focus(&self.focus)
            .size_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_4()
            .on_key_down(cx.listener(|view, event: &KeyDownEvent, window, cx| {
                if !view.focus.is_focused(window)
                    || event.is_held
                    || event.keystroke.modifiers.control
                    || event.keystroke.modifiers.alt
                    || event.keystroke.modifiers.platform
                {
                    return;
                }
                match event.keystroke.key.as_str() {
                    "space" => view.toggle_pause(cx),
                    "c" => view.mutate(|m| m.clear(), cx),
                    _ => return,
                }
                cx.stop_propagation();
            }));
        let model = self.model.read(cx);
        let Some(memory) = model.client.memory.as_ref() else {
            return root.child(Empty::new(
                t("Memory composition"),
                t("Waiting for memory sample"),
            ));
        };
        let paused = memory.paused;
        let latest = memory.latest.clone();
        let history: Vec<_> = Module::ALL
            .into_iter()
            .map(|module| (module, memory.series(module)))
            .collect();
        root = root
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(t("Memory composition")),
                    )
                    .child(
                        Button::new("debug-pause")
                            .label(t(if paused { "Resume" } else { "Pause" }))
                            .on_click(cx.listener(|view, _, _, cx| view.toggle_pause(cx))),
                    )
                    .child(
                        Button::new("debug-clear")
                            .label(t("Clear"))
                            .on_click(cx.listener(|view, _, _, cx| view.mutate(|m| m.clear(), cx))),
                    )
                    .child(
                        Button::new("debug-close")
                            .label(t("Close"))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(ViewEvent::Dismiss))),
                    ),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(t("Resident memory (RSS) per process; shares are of the readable total. Sampled every 2 s, only while this panel is visible.")),
            );
        if paused {
            root = root.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().warning)
                    .child(t("Sampling paused")),
            );
        }
        let Some(composition) = latest else {
            return root.child(Empty::new(
                t("Memory composition"),
                t("Waiting for memory sample"),
            ));
        };
        let mut tiles = div().flex().flex_wrap().gap_3().child(Metric::new(
            t("Total memory"),
            bytes(Some(composition.total() as f64)),
        ));
        for summary in composition.summaries() {
            tiles = tiles.child(
                Metric::new(
                    summary.module.label(),
                    bytes(summary.bytes.map(|n| n as f64)),
                )
                .detail(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_sm()
                        .child(
                            div()
                                .size(px(8.))
                                .rounded_full()
                                .bg(color(summary.module, cx)),
                        )
                        .child(percent(summary.share)),
                ),
            );
        }
        root = root.child(tiles).child(share_bar(&composition, cx));
        for module in Module::ALL {
            if let Some(card) = module_card(&composition, module, cx) {
                root = root.child(card);
            }
        }
        let series = history
            .iter()
            .filter(|(module, values)| {
                *module != Module::Server || values.iter().any(Option::is_some)
            })
            .map(|(module, values)| ChartSeries::new(module.label(), values, color(*module, cx)));
        root.child(Card::new(t("Memory history")).child(Chart::new(series)))
    }
}
