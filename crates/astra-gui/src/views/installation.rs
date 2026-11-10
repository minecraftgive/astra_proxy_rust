//! Installation controls always target the manager selected by the shared client.
use super::ViewEvent;
use crate::{model::AppModel, widgets::Card};
use astra_client::{
    i18n::t,
    installation::{DetailKind, InstallationAction, StepState},
};
use gpui::KeyDownEvent;
use gpui::{
    div, prelude::*, px, relative, AnyElement, App, Context, Entity, EventEmitter, FocusHandle,
    Focusable, FontWeight, Render, Subscription, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants},
    Disableable,
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
/// A titled group inside the installation card.
fn section(title: &str, cx: &App) -> gpui::Div {
    let p = crate::theme::palette(cx);
    div().flex().flex_col().gap_2().child(
        div()
            .text_xs()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(p.muted)
            .child(title.to_owned()),
    )
}

impl Render for InstallationView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = crate::theme::palette(cx);
        let client = &self.model.read(cx).client;
        let state = &client.installation;
        let notice = state.notice;
        let notice_text = state.notice_text();
        let target = state.target_label();
        let scope = state.scope_label();
        let busy = state.busy();
        let can_install = state.can_install();
        let details = state.details();
        let progress = state.progress_visible().then(|| {
            (
                state.percent(),
                state.progress_label(),
                state.step_rows(),
                state.error.is_some() && !busy,
            )
        });
        let maintenance = &client.maintenance;
        let maintenance_label = maintenance.label();
        let maintenance_log = maintenance.log.clone();
        let show_maintenance = maintenance.active()
            || maintenance.failed()
            || !maintenance_label.is_empty()
            || maintenance_log.is_some();
        let can_recover = maintenance.failed()
            && maintenance
                .record
                .as_ref()
                .is_some_and(|r| !r.phase.terminal());
        let can_cancel = client.update_launch.pending()
            || maintenance
                .record
                .as_ref()
                .map_or(maintenance.active(), |r| r.cancellable());
        let launch = &client.update_launch;
        let launch_label = launch.label();
        let launch_pending = launch.pending();
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
            card = card
                .action(
                    Button::new("installation-scope")
                        .label(scope)
                        .disabled(busy)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.action(InstallationAction::ToggleChoice, cx)
                        })),
                )
                .action(
                    Button::new("installation-refresh")
                        .label(t("Refresh"))
                        .disabled(busy)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.action(InstallationAction::Refresh, cx)
                        })),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(p.muted)
                        .child(format!("{}: {target}", t("Management target"))),
                );
            // Packages first, then wrapped detail text (paths, warnings, result).
            let mut packages = section(t("Program package"), cx);
            let mut info = div().flex().flex_col().gap_1().text_sm();
            for detail in details {
                match detail.kind {
                    DetailKind::Package { index, selected } => {
                        let mut row = div()
                            .id(("install-package", index))
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .cursor_pointer()
                            .child(detail.text)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.action(InstallationAction::Select(index), cx)
                            }));
                        if selected {
                            row = row.bg(p.accent_soft).text_color(p.accent);
                        }
                        packages = packages.child(row);
                    }
                    DetailKind::Plain if detail.text.is_empty() => {}
                    DetailKind::Plain => info = info.child(div().child(detail.text)),
                    DetailKind::Success => {
                        info = info.child(div().text_color(p.ok).child(detail.text))
                    }
                    DetailKind::Error => {
                        info = info.child(div().text_color(p.err).child(detail.text))
                    }
                }
            }
            card = card.child(packages).child(
                div()
                    .id("installation-details")
                    .max_h(px(260.))
                    .overflow_y_scroll()
                    .child(info),
            );
            if let Some((percent, label, steps, failed)) = progress {
                let fill = if failed {
                    p.err
                } else if percent == Some(100) {
                    p.ok
                } else {
                    p.accent
                };
                let mut checklist = div().flex().flex_wrap().gap_x_4().gap_y_1().text_sm();
                for row in steps {
                    let (mark, color) = match row.state {
                        StepState::Done => ("✓", p.ok),
                        StepState::Active => ("▶", p.accent),
                        StepState::Failed => ("✗", p.err),
                        StepState::Pending => ("·", p.faint),
                    };
                    checklist = checklist.child(
                        div()
                            .text_color(color)
                            .child(format!("{mark} {}", row.label)),
                    );
                }
                card = card.child(
                    section(t("Installation progress"), cx)
                        .child(
                            div()
                                .flex()
                                .justify_between()
                                .text_sm()
                                .child(label)
                                .child(percent.map_or_else(String::new, |p| format!("{p}%"))),
                        )
                        .child(
                            div()
                                .w_full()
                                .h(px(8.))
                                .rounded_full()
                                .overflow_hidden()
                                .bg(p.border)
                                .child(
                                    div()
                                        .h_full()
                                        .rounded_full()
                                        .w(relative(f32::from(percent.unwrap_or(0)) / 100.0))
                                        .bg(fill),
                                ),
                        )
                        .child(checklist),
                );
            }
            let mut launch_row = div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_2()
                .child(div().flex_1().text_sm().child(launch_label))
                .child(
                    Button::new("installation-auto-launch")
                        .label(t("Toggle automatic launch (30 seconds)"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.action(InstallationAction::ToggleAutoLaunch, cx)
                        })),
                );
            if launch_pending {
                launch_row = launch_row.child(
                    Button::new("installation-launch")
                        .primary()
                        .label(t("Launch new version now"))
                        .disabled(busy)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.action(InstallationAction::LaunchNow, cx)
                        })),
                );
            }
            card = card.child(launch_row);
            if show_maintenance {
                let mut group = section(t("Maintenance"), cx)
                    .child(div().text_sm().text_color(p.warn).child(maintenance_label))
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
                    );
                if let Some(log) = maintenance_log {
                    group = group.child(
                        div()
                            .id("maintenance-log-text")
                            .max_h(px(160.))
                            .overflow_y_scroll()
                            .text_xs()
                            .text_color(p.muted)
                            .children(
                                log.lines()
                                    .map(|line| div().child(line.to_owned()).into_any_element())
                                    .collect::<Vec<AnyElement>>(),
                            ),
                    );
                }
                card = card.child(group);
            }
            card = card.child(
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
            .overflow_y_scroll()
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
