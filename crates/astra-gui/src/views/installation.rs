//! Installation controls always target the manager selected by the shared client.
use super::ViewEvent;
use crate::{model::AppModel, widgets::Card};
use astra_client::{
    i18n::t,
    installation::{DetailKind, InstallationAction},
};
use gpui::KeyDownEvent;
use gpui::{
    div, prelude::*, px, uniform_list, App, Context, Entity, EventEmitter, FocusHandle, Focusable,
    Render, Subscription, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants},
    ActiveTheme, Disableable,
};

pub struct InstallationView {
    model: Entity<AppModel>,
    focus: FocusHandle,
    _observations: Vec<Subscription>,
}
impl EventEmitter<ViewEvent> for InstallationView {}
impl Focusable for InstallationView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl InstallationView {
    pub fn new(model: Entity<AppModel>, _: &mut Window, cx: &mut Context<Self>) -> Self {
        let observation = cx.observe(&model, |_, _, cx| cx.notify());
        Self {
            model,
            focus: cx.focus_handle(),
            _observations: vec![observation],
        }
    }
    fn action(&mut self, action: InstallationAction, cx: &mut Context<Self>) {
        let input = self.model.update(cx, |model, cx| {
            let input = model.tokio(|client| client.installation_action(action));
            cx.notify();
            input
        });
        cx.emit(ViewEvent::Input(input));
        cx.notify();
    }
}
impl Render for InstallationView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = &self.model.read(cx).client.installation;
        let notice = state.notice;
        let notice_text = state.notice_text();
        let target = state.target_label();
        let scope = state.scope_label();
        let busy = state.busy();
        let can_install = state.can_install();
        let mut details = state.details();
        let maintenance = &self.model.read(cx).client.maintenance;
        let maintenance_label = maintenance.label();
        let can_recover = maintenance.failed()
            && maintenance
                .record
                .as_ref()
                .is_some_and(|r| !r.phase.terminal());
        let can_cancel = self.model.read(cx).client.update_launch.pending()
            || maintenance
                .record
                .as_ref()
                .map_or(maintenance.active(), |r| r.cancellable());
        if let Some(log) = &maintenance.log {
            details.extend(log.lines().map(|line| astra_client::installation::Detail {
                text: line.into(),
                kind: DetailKind::Plain,
            }));
        }
        let launch = &self.model.read(cx).client.update_launch;
        let launch_label = launch.label();
        let can_launch = launch.pending() && !busy;
        let mut card = Card::new(t(if notice {
            "Install Astra"
        } else {
            "Installation"
        }));
        if notice {
            card = card.child(div().child(notice_text)).child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        Button::new("install-notice-open")
                            .primary()
                            .label(t("Go to installation"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.action(InstallationAction::Open, cx)
                            })),
                    )
                    .child(
                        Button::new("install-notice-later")
                            .label(t("Later"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.action(InstallationAction::Dismiss, cx)
                            })),
                    ),
            );
        } else {
            let view = cx.entity().downgrade();
            let count = details.len();
            card = card
                .child(div().child(maintenance_label))
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .child(
                            Button::new("maintenance-retry")
                                .label(t("Retry maintenance with new version"))
                                .disabled(!can_recover)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.action(InstallationAction::RetryMaintenance, cx)
                                })),
                        )
                        .child(
                            Button::new("maintenance-previous")
                                .label(t("Restore previous version manually"))
                                .disabled(!can_recover)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.action(InstallationAction::RestorePrevious, cx)
                                })),
                        )
                        .child(
                            Button::new("maintenance-log")
                                .label(t("Show maintenance log"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.action(InstallationAction::MaintenanceLog, cx)
                                })),
                        )
                        .child(
                            Button::new("maintenance-cancel")
                                .label(t("Cancel maintenance before shutdown"))
                                .disabled(!can_cancel)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.action(InstallationAction::CancelMaintenance, cx)
                                })),
                        ),
                )
                .child(div().child(launch_label))
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(
                            Button::new("installation-launch")
                                .label(t("Launch new version now"))
                                .disabled(!can_launch)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.action(InstallationAction::LaunchNow, cx)
                                })),
                        )
                        .child(
                            Button::new("installation-auto-launch")
                                .label(t("Toggle automatic launch (30 seconds)"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.action(InstallationAction::ToggleAutoLaunch, cx)
                                })),
                        ),
                )
                .child(div().text_sm().child(target))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(div().child(t("Installation scope")))
                        .child(
                            Button::new("installation-scope")
                                .label(scope)
                                .disabled(busy)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.action(InstallationAction::ToggleChoice, cx)
                                })),
                        )
                        .child(
                            Button::new("installation-refresh")
                                .label(t("Refresh"))
                                .disabled(busy)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.action(InstallationAction::Refresh, cx)
                                })),
                        ),
                )
                .child(
                    uniform_list("installation-details", count, move |range, _, cx| {
                        range
                            .map(|index| {
                                let detail = &details[index];
                                let mut row = div()
                                    .id(("install-detail", index))
                                    .h(px(32.))
                                    .px_2()
                                    .flex()
                                    .items_center()
                                    .child(detail.text.clone());
                                match detail.kind {
                                    DetailKind::Package { index, selected } => {
                                        if selected {
                                            let pal = crate::theme::palette(cx);
                                            row = row.bg(pal.accent_soft).text_color(pal.accent);
                                        }
                                        let view = view.clone();
                                        row = row.cursor_pointer().on_click(move |_, _, cx| {
                                            let _ = view.update(cx, |view, cx| {
                                                view.action(InstallationAction::Select(index), cx)
                                            });
                                        });
                                    }
                                    DetailKind::Success => row = row.text_color(cx.theme().success),
                                    DetailKind::Error => row = row.text_color(cx.theme().danger),
                                    DetailKind::Plain => {}
                                }
                                row
                            })
                            .collect::<Vec<_>>()
                    })
                    .h(px(360.)),
                )
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("installation-close")
                                .label(t("Close"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.action(InstallationAction::Dismiss, cx)
                                })),
                        )
                        .child(
                            Button::new("installation-install")
                                .primary()
                                .label(t(if busy { "Installing…" } else { "Install" }))
                                .disabled(!can_install)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.action(InstallationAction::Confirm, cx)
                                })),
                        ),
                );
        }
        div()
            .id("installation-panel")
            .key_context("AstraInstallation")
            .track_focus(&self.focus)
            .size_full()
            .p_4()
            .child(card)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                let action = match event.keystroke.key.as_str() {
                    "escape" => Some(InstallationAction::Dismiss),
                    "up" => Some(InstallationAction::Previous),
                    "down" => Some(InstallationAction::Next),
                    _ => None,
                };
                if let Some(action) = action {
                    this.action(action, cx);
                    cx.stop_propagation();
                }
            }))
    }
}

#[cfg(test)]
#[path = "../test/installation_view_test.rs"]
mod tests;
