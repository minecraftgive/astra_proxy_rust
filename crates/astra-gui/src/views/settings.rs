//! Preferences are controlled by the shared settings table and persistence methods.
use super::ViewEvent;
use crate::{
    model::AppModel,
    widgets::{Card, TokenText},
};
use astra_client::{
    i18n::t,
    settings::{self, Field},
};
use gpui::{
    div, prelude::*, px, uniform_list, App, Context, Entity, EventEmitter, FocusHandle, Focusable,
    Render, Subscription, Window,
};
use gpui::{AnyElement, FontWeight, KeyDownEvent, KeyUpEvent, Task};
use gpui_component::{
    button::{Button, ButtonVariants},
    switch::Switch,
    ActiveTheme, Disableable, Sizable,
};
use std::time::{Duration, Instant};

pub struct SettingsView {
    model: Entity<AppModel>,
    focus: FocusHandle,
    accent_focus: FocusHandle,
    terminals_open: bool,
    hold: Option<Task<()>>,
    _observations: Vec<Subscription>,
}
impl EventEmitter<ViewEvent> for SettingsView {}
impl Focusable for SettingsView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl SettingsView {
    pub fn new(model: Entity<AppModel>, _: &mut Window, cx: &mut Context<Self>) -> Self {
        let observation = cx.observe(&model, |_, _, cx| cx.notify());
        Self {
            model,
            focus: cx.focus_handle(),
            accent_focus: cx.focus_handle(),
            terminals_open: false,
            hold: None,
            _observations: vec![observation],
        }
    }
    fn choose(&mut self, field: Field, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| {
            model.tokio(|client| client.choose_setting(field, index));
            crate::theme::apply_for_window(&model.client.preferences, window, cx);
            cx.notify();
        });
        cx.notify();
    }
    fn end_hold(&mut self, cx: &mut Context<Self>) {
        self.hold = None;
        self.model
            .update(cx, |model, _| model.client.marquee_hold = None);
    }
    fn start_hold(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.hold.is_some() {
            return;
        }
        self.model.update(cx, |model, cx| {
            model.tokio(|client| {
                client.marquee_hold_at(Instant::now());
            });
            cx.notify();
        });
        self.hold = Some(cx.spawn_in(window, async move |view, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(100))
                .await;
            let keep = view
                .update_in(cx, |view, window, cx| {
                    if !view.accent_focus.contains_focused(window, cx) {
                        view.hold = None;
                        view.model
                            .update(cx, |model, _| model.client.marquee_hold = None);
                        return false;
                    }
                    view.model.update(cx, |model, cx| {
                        model.tokio(|client| {
                            client.marquee_hold_at(Instant::now());
                        });
                        cx.notify();
                    });
                    cx.notify();
                    true
                })
                .unwrap_or(false);
            if !keep {
                break;
            }
        }));
    }
    fn terminal_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut body = div();
        if self.terminals_open {
            let choices = self.model.read(cx).client.terminal_choices();
            let count = choices.len();
            let view = cx.entity().downgrade();
            body = body.child(
                Card::new(t("Terminal")).child(
                    uniform_list("terminal-choices", count, move |range, _, _cx| {
                        range
                            .map(|index| {
                                let choice = &choices[index];
                                let view = view.clone();
                                div().h(px(40.)).child(
                                    Button::new(("terminal", index))
                                        .label(choice.label.clone())
                                        .disabled(!choice.available)
                                        .on_click(move |_, window, cx| {
                                            let _ = view.update(cx, |view, cx| {
                                                view.choose(Field::Terminal, index, window, cx);
                                                view.terminals_open = false;
                                            });
                                        }),
                                )
                            })
                            .collect::<Vec<_>>()
                    })
                    .h(px(240.)),
                ),
            );
        }
        body.into_any_element()
    }
    fn field(
        &mut self,
        field: Field,
        row: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let client = &self.model.read(cx).client;
        let selected = client.setting_index(field);
        let value = if field == Field::Terminal {
            client
                .terminal_choices()
                .get(selected)
                .map(|choice| choice.label.clone())
                .unwrap_or_else(|| t("Auto").into())
        } else {
            field.value_label(&client.preferences)
        };
        let details = client.setting_details(field);
        let save_location = client.setting_save_location(field);
        let control =
            if field == Field::Terminal {
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(Button::new(("terminal-picker", row)).label(value).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.terminals_open = !this.terminals_open;
                            cx.notify();
                        }),
                    ))
                    .child(
                        Button::new("scan-terminals")
                            .label(t("Scan again"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.model.update(cx, |model, cx| {
                                    model.tokio(|client| client.request_terminal_scan());
                                    cx.notify();
                                });
                            })),
                    )
                    .child(
                        Button::new("open-terminal")
                            .label(t("Open TUI in terminal"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.model.update(cx, |model, cx| {
                                    model.tokio(|client| {
                                        if let Err(error) = client.start_terminal_interface() {
                                            client.message = format!("{error:#}");
                                        }
                                    });
                                    cx.notify();
                                });
                            })),
                    )
                    .into_any_element()
            } else if let Some(range) = field.range() {
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new(("setting-decrease", row))
                            .label("−")
                            .disabled(selected <= usize::from(range.min))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.choose(
                                    field,
                                    usize::from(range.nudge(selected as u16, false, false)),
                                    window,
                                    cx,
                                )
                            })),
                    )
                    .child(div().min_w(px(80.)).text_center().child(value))
                    .child(
                        Button::new(("setting-increase", row))
                            .label("+")
                            .disabled(selected >= usize::from(range.max))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.choose(
                                    field,
                                    usize::from(range.nudge(selected as u16, true, false)),
                                    window,
                                    cx,
                                )
                            })),
                    )
                    .child(
                        Button::new(("setting-reset", row))
                            .ghost()
                            .label(t("Reset"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.choose(field, usize::from(range.default), window, cx)
                            })),
                    )
                    .into_any_element()
            } else if field.choices() == ["On", "Off"] {
                Switch::new(("setting-switch", row))
                    .checked(selected == 0)
                    .label(value)
                    .on_click(cx.listener(move |this, enabled: &bool, window, cx| {
                        this.choose(field, usize::from(!*enabled), window, cx)
                    }))
                    .into_any_element()
            } else {
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .children(field.choices().iter().enumerate().map(|(index, _)| {
                        let button = Button::new(("setting-choice", row * 32 + index))
                            .small()
                            .label(field.choice_label(index));
                        let button = if index == selected {
                            button.primary()
                        } else {
                            button
                        };
                        button.on_click(cx.listener(move |this, _, window, cx| {
                            this.choose(field, index, window, cx)
                        }))
                    }))
                    .into_any_element()
            };
        let mut section = div()
            .id(("setting-row", row))
            .flex()
            .flex_col()
            .gap_2()
            .py_3()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(t(field.label())),
            )
            .child(control)
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(t(field.desktop_description())),
            )
            .children(details.into_iter().map(|line| div().text_xs().child(line)))
            .children(save_location.filter(|s| !s.is_empty()).map(|line| {
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(line)
            }));
        if field == Field::Accent {
            section = section
                .track_focus(&self.accent_focus)
                .key_context("AstraSettingsAccent")
                .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                    let modifiers = event.keystroke.modifiers;
                    if event.keystroke.key == "home"
                        && modifiers.control
                        && !modifiers.alt
                        && !modifiers.shift
                    {
                        this.start_hold(window, cx);
                        cx.stop_propagation();
                    } else {
                        this.end_hold(cx);
                    }
                }))
                .on_key_up(cx.listener(|this, _: &KeyUpEvent, _, cx| this.end_hold(cx)));
        }
        if field == Field::Terminal && self.terminals_open {
            section = section.child(self.terminal_picker(cx));
        }
        let _ = window;
        section.into_any_element()
    }
}
impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let information = self
            .model
            .read(cx)
            .client
            .settings_program_info(&astra_platform::build_info!(), None);
        let current_fields = settings::fields(&self.model.read(cx).client.preferences).to_vec();
        let mut body = div().flex().flex_col().gap_4().child(
            Card::new(t("Program information"))
                .action(
                    Button::new("refresh-program-info")
                        .label(t("Refresh"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.model.update(cx, |model, cx| {
                                model.tokio(|client| client.refresh_program_info());
                                cx.notify();
                            });
                        })),
                )
                .children(
                    information
                        .into_iter()
                        .map(|row| div().text_sm().child(TokenText::new(row))),
                ),
        );
        for (title, rows) in settings::SECTIONS {
            let fields = rows
                .filter_map(|row| current_fields.get(row).copied().map(|field| (row, field)))
                .collect::<Vec<_>>();
            if fields.is_empty() {
                continue;
            }
            let mut card = Card::new(t(title));
            for (row, field) in fields {
                card = card.child(self.field(field, row, window, cx));
            }
            body = body.child(card);
        }
        div()
            .id("settings-page")
            .key_context("AstraSettings")
            .track_focus(&self.focus)
            .size_full()
            .overflow_y_scroll()
            .child(body)
    }
}

#[cfg(test)]
#[path = "../test/settings_view_test.rs"]
mod tests;
