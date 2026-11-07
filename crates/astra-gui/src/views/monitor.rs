use crate::{
    model::AppModel,
    views::ViewEvent,
    widgets::{Card, Empty},
};
use astra_client::i18n::t;
use gpui::{
    div, App, Context, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement,
    IntoElement, KeyDownEvent, ParentElement, Render, StatefulInteractiveElement, Styled,
    Subscription, Window,
};
use gpui_component::{button::Button, ActiveTheme};
pub struct View {
    model: Entity<AppModel>,
    focus: FocusHandle,
    observation: Subscription,
}
impl EventEmitter<ViewEvent> for View {}
impl Focusable for View {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl View {
    pub fn new(model: Entity<AppModel>, _: &mut Window, cx: &mut Context<Self>) -> Self {
        model.update(cx, |model, cx| {
            model.tokio(|c| {
                if c.monitor.is_none() {
                    c.open_monitor();
                }
            });
            cx.notify();
        });
        let observation = cx.observe(&model, |_, _, cx| cx.notify());
        Self {
            model,
            focus: cx.focus_handle(),
            observation,
        }
    }
    fn mutate(
        &mut self,
        action: impl FnOnce(&mut astra_client::state::Client),
        cx: &mut Context<Self>,
    ) {
        self.model.update(cx, |model, cx| {
            model.tokio(action);
            cx.notify();
        });
        cx.notify();
    }
}
impl Render for View {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _ = &self.observation;
        let mut root = div()
            .id("monitor-view")
            .key_context("AstraMonitor")
            .track_focus(&self.focus)
            .size_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_4();
        let Some(view) = self.model.read(cx).client.monitor.as_ref() else {
            return root.child(Empty::new(t("Monitor"), t("Waiting for monitor sample")));
        };
        let series = view.series();
        let paused = view.paused;
        let error = view.error.clone();
        let name = view.name.clone();
        let count = view.samples.len();
        let primary = cx.theme().primary;
        let secondary = cx.theme().success;
        root = root
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(name)
                    .child(
                        Button::new("monitor-pause")
                            .label(t(if paused { "Resume" } else { "Pause" }))
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.mutate(
                                    |c| {
                                        if let Some(m) = &mut c.monitor {
                                            m.set_paused(!m.paused)
                                        }
                                    },
                                    cx,
                                )
                            })),
                    )
                    .child(
                        Button::new("monitor-clear")
                            .label(t("Clear"))
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.mutate(
                                    |c| {
                                        if let Some(m) = &mut c.monitor {
                                            m.clear()
                                        }
                                    },
                                    cx,
                                )
                            })),
                    )
                    .child(
                        Button::new("monitor-close")
                            .label(t("Close"))
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.mutate(|c| c.monitor = None, cx);
                                cx.emit(ViewEvent::Dismiss);
                            })),
                    ),
            )
            .child(t(
                "Selected PID only; children excluded. CPU 100% = one logical core.",
            ))
            .child(t(
                "Network counters cover this core's forwarded traffic, not the host network.",
            ))
            .child(format!(
                "{}: {count}/{}",
                t("Recent samples"),
                astra_client::monitor::LIMIT
            ));
        use crate::widgets::{Chart, ChartSeries};
        root = root
            .child(Card::new("CPU").child(Chart::new([ChartSeries::new(
                "CPU %",
                &series.cpu,
                primary,
            )])))
            .child(Card::new(t("Memory")).child(Chart::new([
                ChartSeries::new("RSS", &series.memory, primary),
                ChartSeries::new(t("Virtual memory"), &series.virtual_memory, secondary),
            ])))
            .child(Card::new(t("Network")).child(Chart::new([
                ChartSeries::new(t("Upload"), &series.upload, primary),
                ChartSeries::new(t("Download"), &series.download, secondary),
            ])));
        if let Some(error) = error {
            root = root.child(div().text_color(cx.theme().danger).child(error));
        }
        root.on_key_down(cx.listener(|view, event: &KeyDownEvent, window, cx| {
            if !view.focus.is_focused(window)
                || event.is_held
                || event.keystroke.modifiers.control
                || event.keystroke.modifiers.alt
                || event.keystroke.modifiers.platform
            {
                return;
            }
            match event.keystroke.key.as_str() {
                "space" => view.mutate(
                    |c| {
                        if let Some(m) = &mut c.monitor {
                            m.set_paused(!m.paused)
                        }
                    },
                    cx,
                ),
                "c" => view.mutate(
                    |c| {
                        if let Some(m) = &mut c.monitor {
                            m.clear()
                        }
                    },
                    cx,
                ),
                _ => return,
            }
            cx.stop_propagation();
        }))
    }
}
