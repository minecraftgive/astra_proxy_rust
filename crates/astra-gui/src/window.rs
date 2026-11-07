use crate::{
    keys,
    model::AppModel,
    pages::{Panel, PAGES},
    views, widgets,
};
#[cfg(test)]
use astra_client::protocol::Kind;
use astra_client::{commands::Id, i18n::t};
use gpui::{prelude::*, *};
use gpui_component::{
    button::{Button, ButtonVariants},
    input::InputState,
    ActiveTheme, Root,
};
mod actions;
mod content;

pub struct AppWindow {
    model: Entity<AppModel>,
    focus: FocusHandle,
    page: Option<usize>,
    panel: Option<Panel>,
    palette: bool,
    profile_chooser: bool,
    search: Entity<InputState>,
    prompt: Entity<widgets::PromptHost>,
    elevation: Entity<views::elevation::ElevationHost>,
    toast: Entity<widgets::ToastHost>,
    marquee: Entity<views::marquee::MarqueeView>,
    mounted: Option<content::Mounted>,
    editor: Option<Entity<views::editor::View>>,
    id: u64,
    ready_frame: bool,
    overlays: (bool, bool, bool, bool),
    last_error: Option<String>,
    _observations: Vec<Subscription>,
}
impl AppWindow {
    fn new(
        model: Entity<AppModel>,
        page: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let weak = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            weak.update(cx, |this, cx| {
                if !this.leave_editor(window, cx) {
                    return false;
                }
                this.elevation.update(cx, |host, cx| host.cancel(cx));
                if cx.windows().len() == 1 {
                    this.model.update(cx, |model, cx| model.request_quit(cx));
                    return false;
                }
                true
            })
            .unwrap_or(true)
        });
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        crate::theme::apply_for_window(&model.read(cx).client.preferences.clone(), window, cx);
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(t("Search commands")));
        let prompt = cx.new(|cx| widgets::PromptHost::for_window(model.clone(), id, window, cx));
        let elevation =
            cx.new(|cx| views::elevation::ElevationHost::new(model.clone(), window, cx));
        let toast = cx.new(|_| widgets::ToastHost::default());
        let marquee = cx.new(|cx| views::marquee::MarqueeView::new(model.clone(), window, cx));
        let page = page.or_else(|| model.read(cx).ready.then_some(model.read(cx).client.page));
        let overlays = Self::overlay_flags(&model.read(cx).client);
        model.update(cx, |model, cx| {
            model.register_window(id, page.unwrap_or(model.client.page));
            model.route_window_effects(id);
            model.take_window_effects(id);
            cx.notify();
        });
        let appearance_model = model.clone();
        let observations = vec![
            cx.observe(&elevation, |_, _, cx| cx.notify()),
            cx.subscribe_in(&elevation, window, |this, _, event, window, cx| {
                if let views::ViewEvent::Input(input) = event {
                    this.handle_input(input.clone(), window, cx);
                }
                if !this.elevation.read(cx).visible() {
                    window.focus(&this.focus, cx);
                }
                cx.notify();
            }),
            cx.subscribe_in(&prompt, window, |this, _, event, window, cx| {
                match event {
                    widgets::PromptEvent::Input(input) => {
                        this.handle_input(input.clone(), window, cx)
                    }
                    widgets::PromptEvent::Cancelled => {
                        this.model.update(cx, |model, cx| {
                            model.route_window_effects(this.id);
                            cx.notify();
                        });
                        cx.notify();
                    }
                }
                if this.model.read(cx).client.prompt.is_none() {
                    window.focus(&this.focus, cx);
                }
            }),
            window.observe_window_appearance(move |window, cx| {
                crate::theme::apply_for_window(
                    &appearance_model.read(cx).client.preferences.clone(),
                    window,
                    cx,
                )
            }),
            cx.observe_in(&model, window, |this, model, window, cx| {
                if this.page.is_none() && model.read(cx).ready {
                    this.page = Some(model.read(cx).client.page);
                }
                let flags = Self::overlay_flags(&model.read(cx).client);
                this.apply_window_effects(window, cx);
                this.overlays = flags;
                let error = model
                    .read(cx)
                    .client
                    .tasks
                    .verdict
                    .as_ref()
                    .filter(|(message, failed)| {
                        *failed && message == &model.read(cx).client.message
                    })
                    .map(|(message, _)| message.clone());
                if error != this.last_error {
                    if let Some(message) = &error {
                        this.toast.update(cx, |host, cx| {
                            host.show(widgets::ToastKind::Error, message.clone(), cx)
                        });
                    }
                    this.last_error = error;
                }
                this.record_page(cx);
                let restored = model.update(cx, |model, _| model.take_restored_windows());
                for page in restored {
                    let model = model.clone();
                    cx.defer(move |cx| {
                        let _ = open_at(model, Some(page), cx);
                    });
                }
                // A disappearing overlay must not leave focus on a retained, hidden input.
                if (!flags.0
                    || !model
                        .read(cx)
                        .owns_overlay(crate::model::Overlay::Editor, this.id))
                    && this.panel == Some(Panel::Editor)
                {
                    this.panel = None;
                    window.focus(&this.focus, cx);
                }
                cx.notify();
            }),
            cx.on_release(|this, cx| {
                this.model
                    .update(cx, |model, _| model.remove_window(this.id))
            }),
        ];
        let mut this = Self {
            model,
            focus,
            page,
            panel: None,
            palette: false,
            profile_chooser: false,
            search,
            prompt,
            elevation,
            toast,
            marquee,
            mounted: None,
            editor: None,
            id,
            ready_frame: false,
            overlays,
            last_error: None,
            _observations: observations,
        };
        this.apply_window_effects(window, cx);
        this
    }
    fn overlay_flags(client: &astra_client::state::Client) -> (bool, bool, bool, bool) {
        (
            client.editor.is_some(),
            client.diagnostics.is_some(),
            client.explanation.is_some(),
            client.binding_picker.is_some(),
        )
    }
    fn record_page(&self, cx: &mut Context<Self>) {
        self.model.update(cx, |model, _| {
            model.update_window(self.id, self.page.unwrap_or(model.client.page))
        });
    }
}
impl Render for AppWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let page = self.page.unwrap_or(self.model.read(cx).client.page).min(6);
        let server = self.model.read(cx).server_mode;
        let header = self.model.read(cx).client.header();
        let status = self.model.read(cx).client.message.clone();
        let running = self
            .model
            .read(cx)
            .client
            .tasks
            .records
            .iter()
            .filter(|task| task.finished.is_none())
            .count();
        let errors = self
            .model
            .read(cx)
            .connection_errors
            .values()
            .cloned()
            .collect::<Vec<_>>()
            .join(" · ");
        let content = self.content(window, cx);
        if self.model.read(cx).ready && !self.ready_frame {
            self.ready_frame = true;
            cx.on_next_frame(window, |this, _, cx| {
                this.model.update(cx, |model, _| model.mark_window_ready())
            });
        }
        let sidebar = div()
            .w(px(186.))
            .h_full()
            .p_3()
            .flex()
            .flex_col()
            .gap_2()
            .border_r_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .p_3()
                    .text_xl()
                    .font_weight(FontWeight::BOLD)
                    .child("ASTRA"),
            )
            .children(
                PAGES
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| !server || *index >= 4)
                    .map(|(index, item)| {
                        Button::new(("page", index))
                            .label(format!("{}   {}", index + 1, t(item.title)))
                            .when(page == index, |button| button.primary())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.navigate(index, window, cx)
                            }))
                    }),
            )
            .child(div().flex_1())
            .children(
                [
                    (Id::Palette, "Command palette"),
                    (Id::Help, "Keyboard help"),
                    (Id::Monitor, "Monitor"),
                ]
                .into_iter()
                .map(|(id, label)| {
                    Button::new(label).label(t(label)).on_click(
                        cx.listener(move |this, _, window, cx| this.execute(id, window, cx)),
                    )
                }),
            )
            .child(Button::new("updates").label(t("Updates")).on_click(
                cx.listener(|this, _, window, cx| this.show_panel(Panel::Updates, window, cx)),
            ));
        div()
            .id("astra-root")
            .key_context("Astra")
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(|this, command: &keys::Command, window, cx| {
                this.execute(command.0, window, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(move |this, page: &keys::Page, window, cx| {
                if page.new_window {
                    if page.index < PAGES.len() && (!server || page.index >= 4) {
                        let _ = open_at(this.model.clone(), Some(page.index), cx);
                    }
                } else {
                    this.navigate(page.index, window, cx);
                }
                cx.stop_propagation();
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    this.dismiss(window, cx);
                    cx.stop_propagation();
                }
            }))
            .child(sidebar)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(self.marquee.clone())
                    .child(
                        div()
                            .min_h(px(64.))
                            .px_4()
                            .py_2()
                            .flex()
                            .items_center()
                            .gap_3()
                            .border_b_1()
                            .border_color(cx.theme().border)
                            .child(
                                Button::new("choose-profile")
                                    .label(
                                        header.name.unwrap_or_else(|| t("No profile yet").into()),
                                    )
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.execute(Id::Profiles, window, cx)
                                    })),
                            )
                            .child(
                                Button::new("connection-toggle")
                                    .label(t(header.connect_label))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        let input = this.model.update(cx, |model, cx| {
                                            let input =
                                                model.tokio(|client| client.toggle_current());
                                            cx.notify();
                                            input
                                        });
                                        this.handle_input(input, window, cx);
                                    })),
                            )
                            .child(div().text_sm().child(t(header.mode_label)))
                            .child(div().flex_1().text_sm().child(header.exit_chain))
                            .child(widgets::DelayChip::new(header.delay_ms))
                            .child(div().text_sm().child(format!(
                                "↑ {}  ↓ {}",
                                astra_client::live::rate_value(header.rates[0], true),
                                astra_client::live::rate_value(header.rates[1], true)
                            ))),
                    )
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .min_h_0()
                            .min_w_0()
                            .child(content)
                            .child(div().absolute().top_3().right_3().child(self.toast.clone())),
                    )
                    .when(!errors.is_empty(), |view| {
                        view.child(
                            div()
                                .px_4()
                                .py_2()
                                .text_sm()
                                .text_color(cx.theme().danger)
                                .child(errors),
                        )
                    })
                    .child(
                        div()
                            .h(px(44.))
                            .px_4()
                            .flex()
                            .items_center()
                            .gap_3()
                            .border_t_1()
                            .border_color(cx.theme().border)
                            .child(
                                div()
                                    .id("status-message")
                                    .flex_1()
                                    .text_sm()
                                    .overflow_hidden()
                                    .child(status)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.execute(Id::Tasks, window, cx)
                                    })),
                            )
                            .child(
                                Button::new("tasks")
                                    .label(format!("{} · {running}  F6", t("Tasks")))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.execute(Id::Tasks, window, cx)
                                    })),
                            ),
                    ),
            )
    }
}
pub fn open(model: Entity<AppModel>, cx: &mut App) -> anyhow::Result<()> {
    open_at(model, None, cx)
}
pub(crate) fn open_at(
    model: Entity<AppModel>,
    page: Option<usize>,
    cx: &mut App,
) -> anyhow::Result<()> {
    let bounds = Bounds::centered(None, size(px(1120.), px(760.)), cx);
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("Astra".into()),
                ..Default::default()
            }),
            window_min_size: Some(size(px(760.), px(540.))),
            ..Default::default()
        },
        |window, cx| {
            let view = cx.new(|cx| AppWindow::new(model, page, window, cx));
            cx.new(|cx| Root::new(view, window, cx))
        },
    )?;
    cx.activate(true);
    Ok(())
}
#[cfg(test)]
#[path = "test/window_routing_test.rs"]
mod routing_tests;
#[cfg(test)]
#[path = "test/window_test.rs"]
mod tests;
