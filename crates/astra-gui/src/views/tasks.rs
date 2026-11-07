use crate::{model::AppModel, views::ViewEvent};
use astra_client::i18n::t;
use gpui::{
    div, px, uniform_list, App, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render,
    StatefulInteractiveElement, Styled, Subscription, Window,
};
use gpui_component::{button::Button, Disableable};
pub struct View {
    model: Entity<AppModel>,
    focus: FocusHandle,
    observation: Subscription,
    selected: Option<u64>,
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
            selected: None,
        }
    }
    pub fn cancel(&mut self, id: u64, cx: &mut Context<Self>) {
        if self
            .model
            .read(cx)
            .client
            .tasks
            .records
            .iter()
            .any(|r| r.id == id && r.finished.is_none())
        {
            cx.emit(ViewEvent::Cancel(id));
        }
    }
}
impl Render for View {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _ = &self.observation;
        let tasks = &self.model.read(cx).client.tasks;
        let records: Vec<_> = tasks
            .records
            .iter()
            .map(|r| {
                (
                    r.id,
                    r.title.clone(),
                    r.finished.is_none(),
                    r.events.iter().cloned().collect::<Vec<_>>(),
                    r.finished
                        .unwrap_or_else(std::time::Instant::now)
                        .duration_since(r.started)
                        .as_secs(),
                )
            })
            .collect();
        let selected = self
            .selected
            .and_then(|id| records.iter().find(|r| r.0 == id))
            .or_else(|| records.last())
            .cloned();
        let weak = cx.entity().downgrade();
        let rows = std::sync::Arc::new(records);
        let list = uniform_list("task-list", rows.len(), move |range, _, _| {
            range
                .map(|index| {
                    let (id, title, running, _, elapsed) = &rows[index];
                    let id = *id;
                    let weak = weak.clone();
                    div()
                        .id(("task", index))
                        .h(px(36.))
                        .px_2()
                        .flex()
                        .gap_3()
                        .child(if *running { "◌" } else { "■" })
                        .child(title.clone())
                        .child(format!("{elapsed}s"))
                        .on_click(move |_, _, cx| {
                            let _ = weak.update(cx, |view, cx| {
                                view.selected = Some(id);
                                cx.notify();
                            });
                        })
                })
                .collect()
        })
        .h(px(240.));
        let mut root = div()
            .id("tasks-view")
            .key_context("AstraTasks")
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div().flex().gap_2().child(t("Tasks")).child(
                    Button::new("task-refresh")
                        .label(t("Refresh"))
                        .on_click(cx.listener(|_, _, _, cx| {
                            cx.emit(ViewEvent::Input(astra_client::state::Input::Run(
                                astra_client::protocol::Action::UpdateStatus,
                                None,
                            )))
                        })),
                ),
            )
            .child(list);
        if let Some((id, title, running, events, _)) = selected {
            root = root.child(
                div().flex().gap_2().child(title).child(
                    Button::new("task-cancel")
                        .label(t("Cancel"))
                        .disabled(!running)
                        .on_click(cx.listener(move |view, _, _, cx| view.cancel(id, cx))),
                ),
            );
            let events = std::sync::Arc::new(events);
            root = root.child(
                uniform_list("task-events", events.len(), move |range, _, _| {
                    range
                        .map(|i| {
                            div()
                                .h(px(26.))
                                .whitespace_nowrap()
                                .child(events[i].clone())
                        })
                        .collect()
                })
                .flex_1(),
            );
        }
        root.on_key_down(cx.listener(|view, event: &KeyDownEvent, window, cx| {
            if view.focus.is_focused(window) && event.keystroke.key == "f5" {
                cx.emit(ViewEvent::Input(astra_client::state::Input::Run(
                    astra_client::protocol::Action::UpdateStatus,
                    None,
                )));
                cx.stop_propagation();
            }
        }))
    }
}
