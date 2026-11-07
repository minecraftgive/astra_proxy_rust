//! A focus-isolated approval host; its guard revokes every abandoned TUN ticket.
use super::ViewEvent;
use crate::{
    backend::{
        self,
        elevation::{self, Control, Outcome, Prepared, Request},
    },
    model::AppModel,
    widgets::Card,
};
use astra_client::{i18n::t, protocol::Action, state::Input};
use gpui::{
    div, prelude::*, px, App, Context, Entity, EventEmitter, FocusHandle, Focusable, KeyDownEvent,
    Render, Window,
};
use gpui_component::button::{Button, ButtonVariants};
use std::sync::{atomic::Ordering, Arc};

pub struct ElevationHost {
    model: Entity<AppModel>,
    focus: FocusHandle,
    control: Option<Arc<Control>>,
    prepared: Option<Prepared>,
    installation: bool,
    phase: &'static str,
    origin: Option<backend::Origin>,
}
impl EventEmitter<ViewEvent> for ElevationHost {}
impl Focusable for ElevationHost {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl ElevationHost {
    pub fn new(model: Entity<AppModel>, _: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            model,
            focus: cx.focus_handle(),
            control: None,
            prepared: None,
            installation: false,
            phase: "",
            origin: None,
        }
    }
    pub fn visible(&self) -> bool {
        self.control.is_some()
    }
    /// Returns true only for an interactive elevation input, including duplicates.
    pub fn start(&mut self, input: Input, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let request = match input {
            Input::Run(Action::PrepareElevation { name, expected_pid }, _) => {
                Request::Tun { name, expected_pid }
            }
            Input::Install(request) if request.elevate => Request::Install(request),
            _ => return false,
        };
        if self.visible() {
            window.focus(&self.focus, cx);
            return true;
        }
        let model = self.model.read(cx);
        if !model.ready
            || model
                .elevation_active
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            self.report(
                t("An administrator request is already active or the manager is not ready").into(),
                true,
                cx,
            );
            return true;
        }
        let control = Arc::new(Control::new(
            model.handle.clone(),
            model.socket.clone(),
            model.elevation_active.clone(),
        ));
        self.origin = Some(model.origin());
        self.installation = matches!(request, Request::Install(_));
        let task = model
            .handle
            .spawn(elevation::prepare(request, control.clone()));
        self.control = Some(control.clone());
        self.phase = "Preparing administrator request";
        window.focus(&self.focus, cx);
        cx.notify();
        cx.spawn_in(window, async move |host, cx| {
            let result = task
                .await
                .map_err(anyhow::Error::from)
                .and_then(|value| value);
            let _ = host.update_in(cx, |host, window, cx| {
                if !host.current(&control) {
                    return;
                }
                match result {
                    Ok(prepared) => {
                        let warning = prepared.warning.is_some();
                        host.prepared = Some(prepared);
                        if warning {
                            host.phase = "Confirm administrator request";
                            window.focus(&host.focus, cx);
                            cx.notify();
                        } else {
                            host.execute(window, cx);
                        }
                    }
                    Err(error) => host.failed(format!("{error:#}"), cx),
                }
            });
        })
        .detach();
        true
    }
    fn current(&self, control: &Arc<Control>) -> bool {
        self.control
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, control))
    }
    fn execute(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(prepared) = self.prepared.take() else {
            return;
        };
        let Some(control) = self.control.clone() else {
            return;
        };
        self.phase = "Waiting for administrator permission and completion";
        let task = self
            .model
            .read(cx)
            .handle
            .spawn(elevation::execute(prepared, control.clone()));
        cx.notify();
        cx.spawn_in(window, async move |host, cx| {
            let result = task
                .await
                .map_err(anyhow::Error::from)
                .and_then(|value| value);
            let _ = host.update_in(cx, |host, _, cx| {
                if !host.current(&control) {
                    return;
                }
                match result {
                    Ok(Outcome::Install(request)) => {
                        host.control.take();
                        host.origin.take();
                        cx.emit(ViewEvent::Input(Input::Install(request)));
                        cx.notify();
                    }
                    #[cfg(unix)]
                    Ok(Outcome::Tun { name, snapshot }) => {
                        host.control.take();
                        let origin = host.origin.take();
                        host.model.update(cx, |model, cx| {
                            let action = Action::Runtime {
                                name: name.clone(),
                                expected_pid: None,
                                command: astra_model::runtime::Command::Snapshot,
                            };
                            model.apply(
                                backend::BackendEvent::JobDone {
                                    id: 0,
                                    action: Box::new(action),
                                    origin: origin.unwrap_or_else(|| model.origin()),
                                    result: Box::new(
                                        astra_client::operations::JobResult::Runtime {
                                            name,
                                            snapshot,
                                            message: t("TUN enabled by the elevated core").into(),
                                        },
                                    ),
                                },
                                cx,
                            );
                            cx.notify();
                        });
                        cx.emit(ViewEvent::Input(Input::Idle));
                        cx.notify();
                    }
                    Err(error) => host.failed(format!("{error:#}"), cx),
                }
            });
        })
        .detach();
    }
    fn report(&self, message: String, failed: bool, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| {
            model.client.tasks.notice_result(&message, failed);
            model.client.message = message;
            cx.notify();
        });
    }
    fn failed(&mut self, message: String, cx: &mut Context<Self>) {
        if let Some(control) = self.control.take() {
            control.cancel();
        }
        self.prepared.take();
        self.origin.take();
        if self.installation {
            self.model.update(cx, |model, _| {
                model.client.installation_failed(message.clone())
            });
        }
        self.report(message, true, cx);
        cx.emit(ViewEvent::Input(Input::Idle));
        cx.notify();
    }
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        if !self.visible() {
            return;
        }
        if let Some(control) = self.control.take() {
            control.cancel();
        }
        self.prepared.take();
        self.origin.take();
        self.report(t("Cancelled").into(), false, cx);
        cx.emit(ViewEvent::Input(Input::Idle));
        cx.notify();
    }
}
impl Drop for ElevationHost {
    fn drop(&mut self) {
        if let Some(control) = &self.control {
            control.cancel();
        }
    }
}
impl Render for ElevationHost {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.visible() {
            return div().into_any_element();
        }
        let warning = self
            .prepared
            .as_ref()
            .and_then(|prepared| prepared.warning.clone());
        let confirm = warning.is_some();
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::rgba(0x00000066))
            .key_context("AstraDialog AstraElevation")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    this.cancel(cx);
                }
                cx.stop_propagation();
            }))
            .child(
                div().w(px(620.)).max_w_full().p_4().child(
                    Card::new(t(self.phase))
                        .children(warning.map(|warning| div().text_sm().child(warning)))
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap_2()
                                .child(
                                    Button::new("cancel-elevation")
                                        .label(t("Cancel"))
                                        .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                                )
                                .when(confirm, |row| {
                                    row.child(
                                        Button::new("confirm-elevation")
                                            .primary()
                                            .label(t("Continue"))
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.execute(window, cx)
                                            })),
                                    )
                                }),
                        ),
                ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
#[path = "../test/elevation_view_test.rs"]
mod tests;
