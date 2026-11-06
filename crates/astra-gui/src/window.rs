use crate::{
    keys,
    model::AppModel,
    pages::{Panel, PAGES},
};
use astra_client::{
    commands::{Id, SPECS},
    i18n::t,
    protocol::Kind,
};
use gpui::{div, prelude::*, px, *};
use gpui_component::{
    button::{Button, ButtonVariants},
    input::{Input, InputState},
    ActiveTheme, Disableable, Root, WindowExt,
};

pub struct AppWindow {
    model: Entity<AppModel>,
    focus: FocusHandle,
    page: Option<usize>,
    panel: Option<Panel>,
    palette: bool,
    profile_chooser: bool,
    search: Entity<InputState>,
    _observations: Vec<Subscription>,
}
impl AppWindow {
    fn new(
        model: Entity<AppModel>,
        page: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(t("Search commands")));
        let observations = vec![
            cx.observe(&model, |this, model, cx| {
                if this.page.is_none() && model.read(cx).ready {
                    this.page = Some(model.read(cx).client.page);
                }
                cx.notify();
            }),
            cx.observe(&search, |_, _, cx| cx.notify()),
        ];
        let page = page.or_else(|| model.read(cx).ready.then_some(model.read(cx).client.page));
        window.focus(&focus, cx);
        Self {
            model,
            focus,
            page,
            panel: None,
            palette: false,
            profile_chooser: false,
            search,
            _observations: observations,
        }
    }
    fn shared_editor_active(&self, cx: &App) -> bool {
        let model = self.model.read(cx);
        self.panel == Some(Panel::Editor)
            && !self.palette
            && !self.profile_chooser
            && model.startup_error.is_none()
            && model.client.prompt.is_none()
            && model.client.editor.is_some()
    }
    fn navigate(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= PAGES.len() || (self.model.read(cx).client.kind == Kind::Server && index < 4) {
            return;
        }
        window.focus(&self.focus, cx);
        self.page = Some(index);
        self.model.update(cx, |model, cx| {
            model.client.suspend_editor();
            model.client.page = index;
            model.client.overlay_epoch += 1;
            cx.notify();
        });
        self.panel = None;
        self.palette = false;
        self.profile_chooser = false;
        cx.notify();
    }
    fn execute(&mut self, id: Id, window: &mut Window, cx: &mut Context<Self>) {
        // Search inputs outlive their rendered overlays. Explicitly restore the
        // root focus before hiding them so subsequent shortcuts keep their context.
        window.focus(&self.focus, cx);
        self.palette = false;
        self.profile_chooser = false;
        let page = self.page;
        let input = self.model.update(cx, |model, cx| {
            if let Some(page) = page {
                model.client.page = page;
            }
            let input = model.client.run_command(id);
            cx.notify();
            input
        });
        match input {
            astra_client::state::Input::Idle => {
                cx.notify();
                return;
            }
            astra_client::state::Input::Run(action, prompt) => {
                self.model.update(cx, |model, cx| {
                    model.start_prompt(action, prompt.map(|prompt| *prompt), cx)
                });
                return;
            }
            astra_client::state::Input::Quit => {
                self.model.update(cx, |model, cx| model.request_quit(cx));
                return;
            }
            astra_client::state::Input::ExternalEditor => {
                self.panel = Some(Panel::Editor);
                cx.notify();
                return;
            }
            astra_client::state::Input::Frontend(_) => {}
        }
        match id {
            Id::Quit => {
                self.model.update(cx, |model, cx| model.request_quit(cx));
            }
            Id::Palette => {
                self.palette = true;
                self.profile_chooser = false;
                self.search.update(cx, |input, cx| {
                    input.set_value("", window, cx);
                    input.focus(window, cx);
                });
            }
            Id::Profiles => {
                self.profile_chooser = true;
                self.palette = false;
                self.search.update(cx, |input, cx| {
                    input.set_value("", window, cx);
                    input.focus(window, cx);
                });
            }
            Id::Settings => self.navigate(6, window, cx),
            Id::Help => self.panel = Some(Panel::Help),
            Id::Tasks => self.panel = Some(Panel::Tasks),
            Id::Logs => self.panel = Some(Panel::GuiLog),
            Id::Monitor => self.panel = Some(Panel::Monitor),
            Id::Window => {
                let _ = open_at(self.model.clone(), self.page, cx);
            }
            Id::Pause => self.model.update(cx, |model, cx| {
                model.client.refresh_paused = !model.client.refresh_paused;
                cx.notify();
            }),
            Id::VimKeys => self.model.update(cx, |model, cx| {
                model.client.vim_keys = !model.client.vim_keys;
                cx.notify();
            }),
            Id::MotionFull | Id::MotionReduced | Id::MotionOff => {
                self.model.update(cx, |model, cx| {
                    model.client.preferences.motion = match id {
                        Id::MotionFull => astra_client::motion::MotionMode::Normal,
                        Id::MotionReduced => astra_client::motion::MotionMode::Reduced,
                        _ => astra_client::motion::MotionMode::Off,
                    };
                    cx.notify();
                })
            }
            _ => {
                self.panel = Some(Panel::Editor);
                self.model.update(cx, |model, cx| {
                    model.client.message =
                        t("This action is planned for the next implementation phase").into();
                    cx.notify();
                });
            }
        }
        cx.notify();
    }
    fn panel_button(&self, panel: Panel, cx: &Context<Self>) -> Button {
        Button::new(SharedString::from(format!("panel-{panel:?}")))
            .label(panel.title())
            .on_click(cx.listener(move |this, _, window, cx| {
                window.focus(&this.focus, cx);
                this.panel = Some(panel);
                if panel == Panel::Updates {
                    this.model.update(cx, |model, cx| {
                        model.start(astra_client::protocol::Action::UpdateStatus, cx)
                    });
                }
                this.palette = false;
                this.profile_chooser = false;
                cx.notify();
            }))
    }
    fn content(&self, cx: &mut Context<Self>) -> AnyElement {
        let model = self.model.read(cx);
        let page_index = self.page.unwrap_or(model.client.page).min(6);
        let page = &PAGES[page_index];
        let mut body = div().flex().flex_col().gap_5().p_6();
        if let Some(error) = &model.startup_error {
            return body
                .child(
                    div()
                        .text_2xl()
                        .child(t("Unable to connect to the manager")),
                )
                .child(error.clone())
                .child(t(
                    "Check the manager path or start astra-host, then relaunch the GUI.",
                ))
                .into_any_element();
        }
        if let Some(prompt) = &model.client.prompt {
            return body
                .child(div().text_2xl().child(prompt.title().to_owned()))
                .child(todo("Prompt dialogs and native file pickers", cx))
                .child(
                    Button::new("dismiss-prompt")
                        .label(t("Cancel"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            window.focus(&this.focus, cx);
                            this.model.update(cx, |model, cx| {
                                model.client.prompt = None;
                                cx.notify();
                            });
                        })),
                )
                .into_any_element();
        }
        if self.palette || self.profile_chooser {
            let query = self.search.read(cx).value().to_string().to_lowercase();
            body = body
                .child(div().text_2xl().child(t(if self.profile_chooser {
                    "Switch current profile"
                } else {
                    "Command palette"
                })))
                .child(Input::new(&self.search));
            if self.profile_chooser {
                for row in model.client.rows.iter().filter(|row| {
                    row.instance.kind == model.client.kind
                        && row.instance.name.to_lowercase().contains(&query)
                }) {
                    let name = row.instance.name.clone();
                    body = body.child(
                        Button::new(SharedString::from(format!("profile-{name}")))
                            .label(name.clone())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                window.focus(&this.focus, cx);
                                this.model.update(cx, |model, cx| {
                                    let kind = model.client.kind;
                                    if let Some(index) = model
                                        .client
                                        .rows
                                        .iter()
                                        .filter(|row| row.instance.kind == kind)
                                        .position(|row| row.instance.name == name)
                                    {
                                        model.client.selections
                                            [astra_client::state::kind_index(kind)] = index;
                                    }
                                    if kind == Kind::Proxy {
                                        if model.client.active_profile.as_ref() != Some(&name) {
                                            model.client.reset_runtime_identity();
                                        }
                                        model.client.active_profile = Some(name.clone());
                                    }
                                    model.client.overlay_epoch += 1;
                                    let key = model
                                        .client
                                        .rows
                                        .iter()
                                        .find(|row| row.instance.name == name)
                                        .and_then(|row| row.pid.map(|pid| (name.clone(), pid)));
                                    let _ = model.commands.try_send(
                                        crate::backend::BackendCmd::Runtime(
                                            if kind == Kind::Server { None } else { key },
                                        ),
                                    );
                                    cx.notify();
                                });
                                this.profile_chooser = false;
                                cx.notify();
                            })),
                    );
                }
            } else {
                for spec in SPECS.iter().filter(|spec| {
                    format!("{} {}", t(spec.label), spec.key)
                        .to_lowercase()
                        .contains(&query)
                }) {
                    let id = spec.id;
                    let unavailable = model.client.command_available(id);
                    let label = format!(
                        "{}    {}{}",
                        t(spec.label),
                        spec.key,
                        unavailable
                            .map(|reason| format!(" · {}", t(reason)))
                            .unwrap_or_default()
                    );
                    body =
                        body.child(
                            Button::new(SharedString::from(format!("command-{id:?}")))
                                .label(label)
                                .disabled(unavailable.is_some())
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.execute(id, window, cx)
                                })),
                        );
                }
            }
            return body.into_any_element();
        }
        if let Some(panel) = self.panel {
            body = body.child(
                div()
                    .flex()
                    .justify_between()
                    .child(div().text_2xl().child(panel.title()))
                    .child(
                        Button::new("close-panel")
                            .label(t("Close"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                window.focus(&this.focus, cx);
                                this.panel = None;
                                cx.notify();
                            })),
                    ),
            );
            if panel == Panel::Help {
                for spec in SPECS {
                    body = body.child(
                        div()
                            .flex()
                            .justify_between()
                            .child(t(spec.label))
                            .child(spec.key),
                    );
                }
            } else if panel == Panel::Tasks {
                if model.client.tasks.records.is_empty() {
                    body = body.child(t("No background tasks"));
                }
                for task in &model.client.tasks.records {
                    let id = task.id;
                    if !task.finished.is_some() {
                        body = body.child(
                            Button::new(("cancel-task", id as usize))
                                .label(t("Cancel"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.model.update(cx, |model, cx| {
                                        let _ = model
                                            .commands
                                            .try_send(crate::backend::BackendCmd::Cancel(id));
                                        if let Some(task) = model
                                            .client
                                            .tasks
                                            .records
                                            .iter_mut()
                                            .find(|task| task.id == id)
                                        {
                                            task.finished = Some(std::time::Instant::now());
                                            task.events.push_back(t("Cancelled").into());
                                        }
                                        cx.notify();
                                    });
                                })),
                        );
                    }
                    body = body.child(
                        div()
                            .p_3()
                            .rounded_lg()
                            .bg(cx.theme().muted)
                            .child(format!(
                                "#{} · {} · {}",
                                task.id,
                                task.title,
                                t(if task.finished.is_some() {
                                    "Done"
                                } else {
                                    "Working"
                                })
                            ))
                            .children(
                                task.events
                                    .iter()
                                    .map(|event| div().text_sm().child(event.clone())),
                            ),
                    );
                }
            }
            for pending in panel.pending() {
                body = body.child(todo(pending, cx));
            }
            return body.into_any_element();
        }
        body = body
            .child(
                div()
                    .text_2xl()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(t(page.title)),
            )
            .child(
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child(t(page.subtitle)),
            );
        if page_index == 0 {
            let runtime = model.client.runtime_snapshot.as_ref();
            body = body.child(
                div()
                    .flex()
                    .gap_4()
                    .child(metric(
                        t("Upload"),
                        if model.client.runtime_rates_available() {
                            format!("{} B/s", model.client.rates[0])
                        } else {
                            "—".into()
                        },
                        cx,
                    ))
                    .child(metric(
                        t("Download"),
                        if model.client.runtime_rates_available() {
                            format!("{} B/s", model.client.rates[1])
                        } else {
                            "—".into()
                        },
                        cx,
                    ))
                    .child(metric(
                        t("Connections"),
                        runtime
                            .map(|snapshot| snapshot.active.to_string())
                            .unwrap_or_else(|| "—".into()),
                        cx,
                    )),
            );
        }
        if page_index == 5 {
            if model.client.kind == Kind::Server {
                body = body.child(t("Server status and statistics"));
            }
            if model.client.rows().is_empty() {
                body = body.child(t("No profiles"));
            }
            for row in model.client.rows() {
                body = body.child(
                    div()
                        .flex()
                        .justify_between()
                        .p_3()
                        .rounded_lg()
                        .bg(cx.theme().muted)
                        .child(row.instance.name.clone())
                        .child(row.state.clone()),
                );
            }
        }
        body = body.child(
            div()
                .rounded_lg()
                .border_1()
                .border_color(cx.theme().border)
                .p_4()
                .child(t("Framework preview · planned features")),
        );
        for feature in page.pending {
            body = body.child(todo(feature, cx));
        }
        if page_index == 6 {
            body = body.child(self.panel_button(Panel::Updates, cx));
            if let Some(info) = &model.client.program_info.data {
                body = body.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .p_4()
                        .border_1()
                        .border_color(cx.theme().border)
                        .rounded_lg()
                        .child(
                            div()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .child(t("Program information")),
                        )
                        .child(format!("{}  {}", info.build.name, info.build.version))
                        .child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child(format!(
                                    "{} · {}",
                                    info.system.os, info.system.architecture
                                )),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child(format!(
                                    "{} · {}",
                                    t("Compiled at"),
                                    info.build.compiled_at
                                )),
                        ),
                );
            }
        }
        body.child(
            div()
                .flex()
                .flex_wrap()
                .gap_2()
                .child(self.panel_button(Panel::Editor, cx))
                .child(self.panel_button(Panel::Diagnostics, cx))
                .child(self.panel_button(Panel::NetTools, cx))
                .child(self.panel_button(Panel::Recovery, cx)),
        )
        .into_any_element()
    }
}
fn todo(feature: &str, cx: &App) -> impl IntoElement {
    div()
        .flex()
        .gap_3()
        .child(
            div()
                .text_color(cx.theme().muted_foreground)
                .child(t("Planned")),
        )
        .child(t(feature).to_owned())
}
fn metric(label: &str, value: String, cx: &App) -> impl IntoElement {
    div()
        .flex_1()
        .rounded_lg()
        .border_1()
        .border_color(cx.theme().border)
        .p_4()
        .child(
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(label.to_owned()),
        )
        .child(div().text_2xl().mt_2().child(value))
}
impl Render for AppWindow {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let model = self.model.read(cx);
        let page = self.page.unwrap_or(model.client.page);
        let server = model.client.kind == Kind::Server;
        let online = model.online;
        let profile = if server {
            model.client.selected().map(|row| row.instance.name.clone())
        } else {
            model.client.active_profile.clone()
        }
        .unwrap_or_else(|| t("No profile selected").into());
        let status = model.client.message.clone();
        let errors = model
            .connection_errors
            .iter()
            .map(|(source, error)| format!("{}: {error}", t(source)))
            .collect::<Vec<_>>()
            .join(" · ");
        let running = model
            .client
            .tasks
            .records
            .iter()
            .filter(|task| !task.finished.is_some())
            .count();
        let mut sidebar = div()
            .w(px(210.))
            .flex_shrink_0()
            .border_r_1()
            .border_color(cx.theme().border)
            .p_4()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_2xl()
                    .font_weight(FontWeight::BOLD)
                    .mb_5()
                    .child("ASTRA"),
            );
        for (index, item) in PAGES
            .iter()
            .enumerate()
            .filter(|(index, _)| !server || *index >= 4)
        {
            sidebar = sidebar.child(
                Button::new(("page", index))
                    .label(format!("{}   {}", index + 1, t(item.title)))
                    .when(index == page, |button| button.primary())
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.navigate(index, window, cx)),
                    ),
            );
        }
        sidebar =
            sidebar
                .child(div().flex_1())
                .child(Button::new("palette").label(t("Command palette")).on_click(
                    cx.listener(|this, _, window, cx| this.execute(Id::Palette, window, cx)),
                ))
                .child(Button::new("help").label(t("Keyboard help")).on_click(
                    cx.listener(|this, _, window, cx| this.execute(Id::Help, window, cx)),
                ));
        div()
            .id("astra-root")
            .key_context(if self.shared_editor_active(cx) {
                "AstraEditor"
            } else {
                "Astra"
            })
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(|this, command: &keys::Command, window, cx| {
                this.execute(command.0, window, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, page: &keys::Page, window, cx| {
                if page.new_window {
                    if page.index < PAGES.len()
                        && (this.model.read(cx).client.kind != Kind::Server || page.index >= 4)
                    {
                        let _ = open_at(this.model.clone(), Some(page.index), cx);
                    }
                } else {
                    this.navigate(page.index, window, cx);
                }
                cx.stop_propagation();
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    this.palette = false;
                    this.profile_chooser = false;
                    this.panel = None;
                    window.focus(&this.focus, cx);
                    cx.notify();
                    return;
                }
                // A retained editor is not necessarily visible in this window.
                // Let native inputs finish handling text after event bubbling.
                if !this.shared_editor_active(cx)
                    || !this.focus.is_focused(window)
                    || window.has_focused_input(cx)
                {
                    return;
                }
                if let Some(key) = keys::from_keystroke(&event.keystroke, event.is_held) {
                    this.model.update(cx, |model, cx| {
                        if let Some(editor) = model.client.editor.as_mut() {
                            let _ = editor.key(key);
                            cx.notify();
                        }
                    });
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
                    .child(
                        div()
                            .h(px(64.))
                            .px_6()
                            .flex()
                            .items_center()
                            .justify_between()
                            .border_b_1()
                            .border_color(cx.theme().border)
                            .child(Button::new("choose-profile").label(profile).on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.execute(Id::Profiles, window, cx)
                                }),
                            ))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(if online {
                                        cx.theme().primary
                                    } else {
                                        cx.theme().muted_foreground
                                    })
                                    .child(t(if online { "Connected" } else { "Connecting…" })),
                            ),
                    )
                    .child(
                        div()
                            .id("page-content")
                            .flex_1()
                            .overflow_y_scroll()
                            .child(self.content(cx)),
                    )
                    .when(!errors.is_empty(), |view| {
                        view.child(
                            div()
                                .px_6()
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
                            .justify_between()
                            .border_t_1()
                            .border_color(cx.theme().border)
                            .child(div().text_sm().child(status))
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
fn open_at(model: Entity<AppModel>, page: Option<usize>, cx: &mut App) -> anyhow::Result<()> {
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
#[path = "test/window_test.rs"]
mod tests;
