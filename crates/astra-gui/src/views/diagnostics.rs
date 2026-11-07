use crate::{model::AppModel, views::ViewEvent, widgets::Empty};
use astra_client::i18n::t;
use gpui::{
    div, px, uniform_list, App, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, Render, StatefulInteractiveElement, Styled,
    Subscription, Window,
};
use gpui_component::{button::Button, ActiveTheme, Disableable};
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
            .id("diagnostics-view")
            .key_context("AstraDiagnostics")
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .gap_3();
        let Some(view) = self.model.read(cx).client.diagnostics.as_ref() else {
            return root.child(Empty::new(t("Node tests"), t("Select a proxy")));
        };
        let rows = std::sync::Arc::new(view.rows());
        let running = view.receiver.is_some();
        let loading = view.loading;
        let progress = view.progress();
        let error = view.error.clone();
        let mut toolbar = div().flex().flex_wrap().gap_2();
        for (index, (label, action)) in [
            ("Select all", 0),
            ("Clear", 1),
            ("Run", 2),
            ("Stop", 3),
            ("Network tools", 4),
            ("Close", 5),
        ]
        .into_iter()
        .enumerate()
        {
            toolbar = toolbar.child(
                Button::new(("diagnostic-action", index))
                    .label(t(label))
                    .disabled((action == 2 && running) || (action == 3 && (!running || loading)))
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.mutate(
                            move |c| {
                                if action == 5 {
                                    c.diagnostics = None;
                                } else if let Some(d) = &mut c.diagnostics {
                                    match action {
                                        0 => d.select_all(true),
                                        1 => d.select_all(false),
                                        2 => d.run_selected(),
                                        3 => d.stop(),
                                        4 => d.open_tools_page(),
                                        _ => {}
                                    }
                                }
                            },
                            cx,
                        );
                        if action == 5 {
                            cx.emit(ViewEvent::Dismiss);
                        }
                    })),
            );
        }
        let weak = cx.entity().downgrade();
        root = root.child(toolbar).child(progress).child(
            uniform_list("diagnostic-targets", rows.len(), move |range, _, cx| {
                range
                    .map(|index| {
                        let row = &rows[index];
                        let weak = weak.clone();
                        let mut item = div()
                            .id(("diagnostic-target", index))
                            .h(px(58.))
                            .px_2()
                            .flex()
                            .flex_col()
                            .justify_center()
                            .child(
                                div()
                                    .flex()
                                    .gap_3()
                                    .child(if row.checked { "☑" } else { "☐" })
                                    .child(row.name.clone())
                                    .child(match row.success {
                                        Some(true) => "✓",
                                        Some(false) => "✕",
                                        None => "◌",
                                    })
                                    .child(row.state.clone())
                                    .child(
                                        row.latency_ms
                                            .map(|v| format!("{v} ms"))
                                            .unwrap_or_else(|| "—".into()),
                                    )
                                    .child(row.ip.clone().unwrap_or_default()),
                            );
                        if let Some(error) = &row.error {
                            item = item
                                .child(div().text_color(cx.theme().danger).child(error.clone()));
                        }
                        item.on_click(move |_, _, cx| {
                            let _ = weak.update(cx, |view, cx| {
                                view.mutate(
                                    |c| {
                                        if let Some(d) = &mut c.diagnostics {
                                            d.toggle_target(index)
                                        }
                                    },
                                    cx,
                                )
                            });
                        })
                    })
                    .collect()
            })
            .flex_1(),
        );
        if let Some(error) = error {
            root = root.child(div().text_color(cx.theme().danger).child(error));
        }
        root
    }
}
