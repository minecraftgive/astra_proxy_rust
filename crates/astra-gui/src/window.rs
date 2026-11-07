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
    /// The Spotlight-style palette floating above whatever screen is mounted.
    palette_view: Option<(Entity<views::palette::View>, Subscription)>,
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
            palette_view: None,
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
impl AppWindow {
    fn title(&self, page: usize) -> &'static str {
        if let Some(panel) = self.panel {
            panel.title()
        } else {
            t(PAGES[page].title)
        }
    }
    /// The web UI's sidebar: brand, numbered pages, then tools.
    fn sidebar(&self, page: usize, running: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let p = crate::theme::palette(cx);
        let server = self.model.read(cx).server_mode;
        let online = self.model.read(cx).online;
        let on_page = self.panel.is_none();
        let brand = div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .pt_1()
            .pb_4()
            .child(
                div()
                    .size(px(22.))
                    .rounded_md()
                    .bg(p.accent)
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_xs()
                    .font_weight(FontWeight::BOLD)
                    .text_color(cx.theme().primary_foreground)
                    .child("A"),
            )
            .child(
                div()
                    .text_size(px(15.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(p.text)
                    .child("Astra"),
            )
            .child(widgets::Pill::new(t(if server {
                "Server"
            } else {
                "Proxy"
            })));
        let pages = PAGES
            .iter()
            .enumerate()
            .filter(|(index, _)| !server || *index >= 4)
            .map(|(index, item)| {
                widgets::NavItem::new(("page", index), t(item.title))
                    .shortcut((index + 1).to_string())
                    .active(on_page && page == index)
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.navigate(index, window, cx)),
                    )
            });
        let tools = [
            (Panel::Monitor, Id::Monitor, "F9".to_owned()),
            (
                Panel::Tasks,
                Id::Tasks,
                if running > 0 {
                    format!("{running} · F6")
                } else {
                    "F6".to_owned()
                },
            ),
            (Panel::Updates, Id::Settings, String::new()),
            (Panel::Help, Id::Help, "F1".to_owned()),
        ]
        .into_iter()
        .map(|(panel, id, key)| {
            widgets::NavItem::new(SharedString::from(format!("tool-{panel:?}")), panel.title())
                .shortcut(key)
                .active(self.panel == Some(panel))
                .on_click(cx.listener(move |this, _, window, cx| {
                    if panel == Panel::Updates {
                        this.show_panel(panel, window, cx)
                    } else {
                        this.execute(id, window, cx)
                    }
                }))
        });
        div()
            .w(px(200.))
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .gap(px(2.))
            .px(px(10.))
            .py(px(14.))
            .bg(p.surface)
            .border_r_1()
            .border_color(p.border)
            .child(brand)
            .children(pages)
            .child(
                div()
                    .px_3()
                    .pt_4()
                    .pb_1()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(p.muted)
                    .child(t("Tools")),
            )
            .children(tools)
            .child(
                widgets::NavItem::new("tool-palette", t("Command palette"))
                    .shortcut("Ctrl-K")
                    .active(self.palette)
                    .on_click(
                        cx.listener(|this, _, window, cx| this.execute(Id::Palette, window, cx)),
                    ),
            )
            .child(div().flex_1())
            .child(
                div()
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_xs()
                    .text_color(p.muted)
                    .child(
                        div()
                            .size(px(8.))
                            .rounded_full()
                            .bg(if online { p.ok } else { p.warn }),
                    )
                    .child(t(if online { "Connected" } else { "Connecting…" })),
            )
    }
    /// The web UI's top bar: status, current profile, state, then live rates.
    fn top_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = crate::theme::palette(cx);
        let model = self.model.read(cx);
        let header = model.client.header();
        let kind = model.client.kind;
        let online = model.online;
        let status = crate::views::profiles::status(header.state);
        let state_tone = match status {
            widgets::Status::Running => widgets::Tone::Ok,
            widgets::Status::Starting => widgets::Tone::Warn,
            widgets::Status::Error => widgets::Tone::Err,
            widgets::Status::Stopped => widgets::Tone::Neutral,
        };
        let state_label = match status {
            widgets::Status::Running => t("Running"),
            widgets::Status::Starting => t("Starting"),
            widgets::Status::Error => t("Error"),
            widgets::Status::Stopped => t("Stopped"),
        };
        let profile = div()
            .id("choose-profile")
            .flex()
            .items_center()
            .gap_2()
            .min_w_0()
            .px_3()
            .py(px(5.))
            .rounded_md()
            .border_1()
            .border_color(p.border)
            .bg(p.surface)
            .cursor_pointer()
            .hover(move |style| style.bg(p.surface_2))
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(p.text)
                    .truncate()
                    .child(
                        header
                            .name
                            .clone()
                            .unwrap_or_else(|| t("No profile yet").into()),
                    ),
            )
            .child(div().text_xs().text_color(p.faint).child("▾"))
            .on_click(cx.listener(|this, _, window, cx| this.execute(Id::Profiles, window, cx)));
        div()
            .h(px(56.))
            .flex_none()
            .px_4()
            .flex()
            .items_center()
            .gap_3()
            .bg(p.surface)
            .border_b_1()
            .border_color(p.border)
            .child(
                div()
                    .size(px(8.))
                    .rounded_full()
                    .bg(if online { p.ok } else { p.warn }),
            )
            .child(profile)
            .child(widgets::Pill::new(t(match kind {
                astra_client::protocol::Kind::Proxy => "Proxy",
                astra_client::protocol::Kind::Rule => "Rule core",
                astra_client::protocol::Kind::Server => "Server",
            })))
            .when(header.name.is_some(), |bar| {
                bar.child(widgets::Pill::new(state_label).tone(state_tone))
                    .when(!header.uptime.is_empty() && header.connected, |bar| {
                        bar.child(
                            div()
                                .text_xs()
                                .text_color(p.muted)
                                .child(header.uptime.clone()),
                        )
                    })
                    .when(header.mode.is_some(), |bar| {
                        bar.child(widgets::Pill::new(header.mode_label).tone(widgets::Tone::Accent))
                    })
            })
            .child(div().flex_1())
            .when(!header.exit_chain.is_empty(), |bar| {
                bar.child(
                    div()
                        .min_w_0()
                        .text_sm()
                        .text_color(p.muted)
                        .truncate()
                        .child(header.exit_chain.clone()),
                )
                .child(widgets::ExitProbe::new(&header))
            })
            .when(header.connected, |bar| {
                bar.child(
                    div()
                        .flex_none()
                        .text_xs()
                        .font_family(crate::theme::mono(cx))
                        .text_color(p.muted)
                        .child(format!(
                            "↑ {}/s  ↓ {}/s",
                            astra_client::live::rate_value(header.rates[0], false).trim(),
                            astra_client::live::rate_value(header.rates[1], false).trim()
                        )),
                )
            })
    }
    /// `.page-head`: the title, then the current profile's common actions.
    fn page_head(&self, page: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let p = crate::theme::palette(cx);
        let header = self.model.read(cx).client.header();
        let on_page = self.panel.is_none();
        let mut tools = div().flex().items_center().gap_2();
        if on_page && header.name.is_some() && page != 6 {
            tools = tools
                .child(
                    Button::new("head-edit")
                        .label(t("Edit profile"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            let input = this.model.update(cx, |model, cx| {
                                let input = model.tokio(|client| {
                                    client.active_operation(astra_client::state::Operation::Edit)
                                });
                                cx.notify();
                                input
                            });
                            this.handle_input(input, window, cx);
                        })),
                )
                .child(
                    Button::new("connection-toggle")
                        .primary()
                        .label(header.connect_label)
                        .on_click(cx.listener(|this, _, window, cx| {
                            let input = this.model.update(cx, |model, cx| {
                                let input = model.tokio(|client| client.toggle_current());
                                cx.notify();
                                input
                            });
                            this.handle_input(input, window, cx);
                        })),
                );
        } else if !on_page {
            tools = tools.child(
                Button::new("head-close")
                    .ghost()
                    .label(t("Close"))
                    .on_click(cx.listener(|this, _, window, cx| this.dismiss(window, cx))),
            );
        }
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap_3()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(20.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(p.text)
                    .truncate()
                    .child(self.title(page)),
            )
            .child(tools)
    }
}
impl Render for AppWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let page = self.page.unwrap_or(self.model.read(cx).client.page).min(6);
        let server = self.model.read(cx).server_mode;
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
        let offline = self.model.read(cx).ready && !self.model.read(cx).online;
        let content = self.content(window, cx);
        // Any path that cleared the palette flags (navigation, a result, an effect)
        // also drops the floating view.
        if !self.palette && !self.profile_chooser {
            self.palette_view = None;
        }
        let palette = self.palette_view.as_ref().map(|(view, _)| view.clone());
        if self.model.read(cx).ready && !self.ready_frame {
            self.ready_frame = true;
            cx.on_next_frame(window, |this, _, cx| {
                this.model.update(cx, |model, _| model.mark_window_ready())
            });
        }
        let p = crate::theme::palette(cx);
        let sidebar = self.sidebar(page, running, cx);
        let top_bar = self.top_bar(cx);
        let page_head = self.page_head(page, cx);
        let body = div()
            .relative()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .bg(p.bg)
            .child(
                div()
                    .size_full()
                    .flex()
                    .justify_center()
                    .child(
                        div()
                            .w_full()
                            .max_w(px(1180.))
                            .h_full()
                            .min_h_0()
                            .flex()
                            .flex_col()
                            .gap_4()
                            .px_6()
                            .pt_5()
                            .pb_4()
                            .child(page_head)
                            .when(offline, |column| {
                                column.child(widgets::NoticeBar::new(
                                    t("The manager is offline. Showing the last observed state; running cores keep running."),
                                    widgets::Tone::Warn,
                                ))
                            })
                            .when(!errors.is_empty(), |column| {
                                column.child(widgets::NoticeBar::new(errors, widgets::Tone::Err))
                            })
                            .child(div().flex_1().min_h_0().min_w_0().child(content)),
                    ),
            )
            .child(div().absolute().top_3().right_3().child(self.toast.clone()));
        let task_bar = div()
            .id("task-bar")
            .h(px(34.))
            .flex_none()
            .px_4()
            .flex()
            .items_center()
            .gap_3()
            .bg(p.surface)
            .border_t_1()
            .border_color(p.border)
            .text_xs()
            .text_color(p.muted)
            .cursor_pointer()
            .child(
                div()
                    .size(px(8.))
                    .rounded_full()
                    .bg(if running > 0 { p.accent } else { p.ok }),
            )
            .child(
                div()
                    .id("status-message")
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(status),
            )
            .child(
                div()
                    .id("tasks")
                    .flex_none()
                    .child(format!("{} · {running}  F6", t("Tasks"))),
            )
            .on_click(cx.listener(|this, _, window, cx| this.execute(Id::Tasks, window, cx)));
        div()
            .id("astra-root")
            .key_context("Astra")
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .bg(p.bg)
            .text_color(p.text)
            .text_size(px(13.))
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
                    if this.palette_view.is_some() {
                        this.close_palette(window, cx);
                    } else {
                        this.dismiss(window, cx);
                    }
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
                    .child(top_bar)
                    .child(body)
                    .child(task_bar),
            )
            .when_some(palette, |root, palette| {
                // Spotlight-style: a dimmed backdrop over the whole window and a
                // floating panel near the top; clicking outside closes it.
                root.child(
                    div()
                        .id("palette-backdrop")
                        .absolute()
                        .inset_0()
                        .flex()
                        .justify_center()
                        .pt(px(96.))
                        .bg(gpui::black().opacity(0.4))
                        .on_click(cx.listener(|this, _, window, cx| this.close_palette(window, cx)))
                        .child(
                            div()
                                .id("palette-panel")
                                .w(px(640.))
                                .max_w(relative(0.9))
                                .h(px(440.))
                                .max_h(relative(0.7))
                                .rounded_xl()
                                .border_1()
                                .border_color(p.border)
                                .bg(p.surface)
                                .shadow_xl()
                                .overflow_hidden()
                                // Clicks inside the panel must not reach the backdrop.
                                .on_click(|_, _, cx| cx.stop_propagation())
                                .child(palette),
                        ),
                )
            })
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
