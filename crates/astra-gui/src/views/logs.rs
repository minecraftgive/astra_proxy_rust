use crate::{model::AppModel, views::ViewEvent, widgets::Empty};
use astra_client::{
    i18n::t,
    input::{KeyCode, KeyModifiers},
    logs::{Command, Mode, Viewer},
    protocol::{Action, LogLevel},
};
use gpui::{
    div, list, px, App, AppContext, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render,
    StatefulInteractiveElement, Styled, Subscription, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants},
    input::{Input, InputEvent, InputState},
    ActiveTheme,
};
pub struct View {
    model: Entity<AppModel>,
    focus: FocusHandle,
    frontend: bool,
    filter: Entity<InputState>,
    search: Entity<InputState>,
    limit: Entity<InputState>,
    observations: Vec<Subscription>,
    scroll: gpui::ListState,
    signature: Option<(String, u64, String, Option<LogLevel>, bool)>,
    presentation: astra_client::logs::PresentationCache,
    reveal: bool,
    error: Option<String>,
}
impl EventEmitter<ViewEvent> for View {}
impl Focusable for View {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl View {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::build(model, false, window, cx)
    }
    pub fn frontend(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::build(model, true, window, cx)
    }
    fn build(
        model: Entity<AppModel>,
        frontend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let settings = model
            .read(cx)
            .client
            .log_viewer(frontend)
            .map(|v| v.settings.clone())
            .unwrap_or_default();
        let filter = cx.new(|cx| {
            let mut s = InputState::new(window, cx).placeholder(t("Filter"));
            s.set_value(settings.filter, window, cx);
            s
        });
        let search = cx.new(|cx| {
            let mut s = InputState::new(window, cx).placeholder(t("Search"));
            s.set_value(settings.search, window, cx);
            s
        });
        let limit = cx.new(|cx| {
            let mut s = InputState::new(window, cx).placeholder(t("Memory limit"));
            s.set_value(settings.memory_lines.to_string(), window, cx);
            s
        });
        let observations = vec![
            cx.observe_in(&model, window, |view, _, window, cx| {
                view.sync_inputs(window, cx);
                cx.notify();
            }),
            cx.subscribe_in(
                &filter,
                window,
                |view, input, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::Change) {
                        let text = input.read(cx).value().to_string();
                        view.mutate(|v| v.set_filter(text), cx);
                    } else if matches!(event, InputEvent::PressEnter { .. }) {
                        window.focus(&view.focus, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &search,
                window,
                |view, input, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::Change) {
                        let text = input.read(cx).value().to_string();
                        view.mutate(|v| v.set_search(text), cx);
                    } else if matches!(event, InputEvent::PressEnter { .. }) {
                        window.focus(&view.focus, cx);
                    }
                },
            ),
            cx.subscribe_in(&limit, window, |view, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    view.apply_limit(window, cx);
                }
            }),
        ];
        let view = Self {
            model,
            focus: cx.focus_handle(),
            frontend,
            filter,
            search,
            limit,
            observations,
            scroll: gpui::ListState::new(0, gpui::ListAlignment::Top, px(200.)),
            signature: None,
            presentation: Default::default(),
            reveal: true,
            error: None,
        };
        if view.model.read(cx).client.log_viewer(frontend).is_none() {
            let source = if frontend {
                Some("@gui".into())
            } else {
                view.model
                    .read(cx)
                    .client
                    .active_log
                    .clone()
                    .filter(|n| astra_client::tui_log::source_name(n).is_none())
                    .or_else(|| {
                        view.model
                            .read(cx)
                            .client
                            .selected()
                            .map(|r| r.instance.name.clone())
                    })
            };
            if let Some(source) = source {
                cx.defer_in(window, move |view, _, cx| view.open_source(source, cx));
            }
        }
        let weak = cx.entity().downgrade();
        view.scroll.set_scroll_handler(move |event, _, cx| {
            if !event.is_scrolled {
                return;
            }
            let start = event.visible_range.start;
            let _ = weak.update(cx, |view, cx| {
                let frontend = view.frontend;
                view.model.update(cx, |model, cx| {
                    model.tokio(|c| {
                        if let Some(v) = c.log_viewer_mut(frontend) {
                            v.set_follow(false);
                            v.top = v.visible().iter().take(start).map(|r| v.row_count(r)).sum();
                        }
                    });
                    cx.notify();
                });
                cx.notify();
            });
        });
        view
    }
    fn sync_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(settings) = self
            .model
            .read(cx)
            .client
            .log_viewer(self.frontend)
            .map(|v| v.settings.clone())
        else {
            return;
        };
        for (input, value) in [
            (&self.filter, settings.filter),
            (&self.search, settings.search),
            (&self.limit, settings.memory_lines.to_string()),
        ] {
            if input.read(cx).focus_handle(cx).is_focused(window) {
                continue;
            }
            if input.read(cx).value().as_ref() != value {
                input.update(cx, |input, cx| input.set_value(value, window, cx));
            }
        }
    }
    pub fn open_source(&mut self, source: String, cx: &mut Context<Self>) {
        self.presentation = Default::default();
        self.frontend = astra_client::tui_log::source_name(&source).is_some();
        self.model.update(cx, |model, cx| {
            model.tokio(|c| {
                c.active_log = Some(source.clone());
                c.overlay_epoch = c.overlay_epoch.wrapping_add(1);
            });
            cx.notify();
        });
        cx.emit(ViewEvent::Input(astra_client::state::Input::Run(
            Action::Logs(source),
            None,
        )));
        cx.notify();
    }
    fn mutate(&mut self, action: impl FnOnce(&mut Viewer), cx: &mut Context<Self>) {
        let frontend = self.frontend;
        self.model.update(cx, |model, cx| {
            model.tokio(|c| {
                if let Some(v) = c.log_viewer_mut(frontend) {
                    v.cancel_input();
                    action(v);
                }
            });
            cx.notify();
        });
        self.reveal = true;
        cx.notify();
    }
    fn apply_limit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.limit.read(cx).value().to_string();
        let mut error = None;
        self.mutate(|v| error = v.set_memory_limit(&text).err(), cx);
        self.error = error;
        if self.error.is_none() {
            window.focus(&self.focus, cx);
        }
    }
    fn log_key(&mut self, key: astra_client::input::Key, cx: &mut Context<Self>) {
        let frontend = self.frontend;
        let command = self.model.update(cx, |model, cx| {
            let command = model.tokio(|client| {
                client
                    .log_viewer_mut(frontend)
                    .map_or(Command::None, |v| v.key(key))
            });
            cx.notify();
            command
        });
        self.reveal = true;
        if !matches!(command, Command::None) {
            self.command(command, cx);
        }
        cx.notify();
    }
    pub fn registered_command(
        &mut self,
        id: astra_client::commands::Id,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use astra_client::commands::Id;
        match id {
            Id::Search => self.search.update(cx, |s, cx| s.focus(window, cx)),
            Id::NextMatch => self.mutate(|v| v.find_again(false), cx),
            Id::PreviousMatch => self.mutate(|v| v.find_again(true), cx),
            Id::Pause => self.mutate(Viewer::toggle_pause, cx),
            _ => {}
        }
    }
    pub fn command(&mut self, command: Command, cx: &mut Context<Self>) {
        let close = matches!(command, Command::Close);
        let frontend = self.frontend;
        let input = self.model.update(cx, |model, cx| {
            let input = model.tokio(|c| {
                if frontend {
                    c.frontend_log_command(command)
                } else {
                    c.log_command(command)
                }
            });
            cx.notify();
            input
        });
        cx.emit(ViewEvent::Input(input));
        if close {
            cx.emit(ViewEvent::Dismiss);
        }
        cx.notify();
    }
}
impl Render for View {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _ = &self.observations;
        let client = &self.model.read(cx).client;
        let sources = client.log_sources();
        let picking_session = client
            .log_viewer(self.frontend)
            .is_some_and(|v| matches!(v.settings.mode, Mode::Session(_)));
        let mut root = div()
            .id("logs-view")
            .key_context(if picking_session {
                "AstraLogs AstraLogPicker"
            } else {
                "AstraLogs"
            })
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .gap_2();
        let mut toolbar = div().flex().flex_wrap().gap_2();
        for (index, source) in sources.into_iter().enumerate() {
            toolbar = toolbar.child(
                Button::new(("log-source", index))
                    .label(source.clone())
                    .on_click(
                        cx.listener(move |view, _, _, cx| view.open_source(source.clone(), cx)),
                    ),
            );
        }
        root = root.child(toolbar);
        let Some(viewer) = client.log_viewer(self.frontend) else {
            self.presentation = Default::default();
            return root.child(Empty::new(t("Logs"), t("连接中")));
        };
        self.presentation.update(viewer);
        let rows = self.presentation.rows.clone();
        let follow = viewer.settings.follow;
        let wrap = viewer.settings.wrap;
        let display = viewer.settings.min_level;
        let capture = viewer.capture_level;
        let supported = viewer.capture_supported;
        let status = viewer.status.clone();
        let top = if self.reveal && !follow {
            let mut height = 0;
            rows.iter()
                .position(|record| {
                    height += viewer.row_count(record);
                    height > viewer.top
                })
                .unwrap_or(0)
        } else {
            0
        };
        let signature = (
            viewer.name.clone(),
            viewer.revision,
            viewer.settings.filter.clone(),
            display,
            wrap,
        );
        if self.signature.as_ref() != Some(&signature) {
            let offset = self.scroll.logical_scroll_top();
            self.scroll.reset(rows.len());
            if !follow {
                self.scroll.scroll_to(offset);
            }
            self.signature = Some(signature);
        }
        if follow && !rows.is_empty() {
            self.scroll.scroll_to_end();
        } else if self.reveal {
            self.scroll.scroll_to_reveal_item(top);
        }
        self.reveal = false;
        let mut levels = div().flex().flex_wrap().gap_1().child(t("Display level"));
        for (index, level) in std::iter::once(None)
            .chain(LogLevel::ALL.into_iter().map(Some))
            .enumerate()
        {
            let mut button = Button::new(("display-level", index))
                .label(level.map(|l| l.as_str()).unwrap_or("All"))
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.mutate(|v| v.set_display_level(level), cx)
                }));
            if level == display {
                button = button.primary();
            }
            levels = levels.child(button);
        }
        // Sessions: the current one by default, earlier ones read on demand.
        let selected_session = match viewer.settings.mode {
            Mode::Session(index) => index,
            _ => viewer.selected_session_index(),
        };
        let mut sessions = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_1()
            .child(t("Log session"));
        for (index, (label, session)) in viewer
            .session_choices()
            .into_iter()
            .zip(&viewer.sessions)
            .enumerate()
        {
            let session = (!session.current).then(|| session.id.clone());
            let mut button =
                Button::new(("log-session", index))
                    .label(label)
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.command(Command::Session(session.clone()), cx)
                    }));
            if index == selected_session {
                button = button.primary();
            }
            sessions = sessions.child(button);
        }
        let session_status = viewer.session_label();
        if picking_session {
            sessions = sessions.child(t("↑↓ select · Enter apply · Esc back"));
        }
        if supported {
            levels = levels.child(t("Capture level"));
            for (index, level) in LogLevel::ALL.into_iter().enumerate() {
                let mut button =
                    Button::new(("capture-level", index))
                        .label(level.as_str())
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.command(Command::Capture(level), cx)
                        }));
                if level == capture {
                    button = button.primary();
                }
                levels = levels.child(button);
            }
        }
        root =
            root.child(sessions.child(div().text_xs().child(session_status)))
                .child(levels)
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .child(Input::new(&self.filter))
                        .child(format!("{} (m)", t("Memory limit")))
                        .child(div().w(px(140.)).child(Input::new(&self.limit)))
                        .child(Button::new("log-apply-limit").label(t("Apply")).on_click(
                            cx.listener(|view, _, window, cx| view.apply_limit(window, cx)),
                        ))
                        .child(
                            Button::new("log-wrap")
                                .label(format!("{} {}", if wrap { "☑" } else { "☐" }, t("Wrap")))
                                .on_click(cx.listener(|view, _, _, cx| {
                                    view.mutate(|v| v.set_wrap(!v.settings.wrap), cx)
                                })),
                        )
                        .child(
                            Button::new("log-follow")
                                .label(format!(
                                    "{} {}",
                                    if follow { "☑" } else { "☐" },
                                    t("Follow")
                                ))
                                .on_click(cx.listener(|view, _, _, cx| {
                                    view.mutate(|v| v.set_follow(!v.settings.follow), cx)
                                })),
                        )
                        .child(Button::new("log-reconnect").label(t("Reconnect")).on_click(
                            cx.listener(|view, _, _, cx| view.command(Command::Reconnect, cx)),
                        )),
                );
        let width = self.presentation.width_chars as f32 * 8.;
        let list = list(self.scroll.clone(), move |index, _, cx| {
            let record = &rows[index];
            let color = match record.level {
                Some(LogLevel::Error) => cx.theme().danger,
                Some(LogLevel::Warn) => cx.theme().warning,
                _ => cx.theme().foreground,
            };
            let row = div()
                .min_h(px(20.))
                .text_xs()
                .font_family(crate::theme::mono(cx))
                .text_color(color)
                .child(gpui::SharedString::from(record.text.clone()));
            if wrap {
                row.w_full().into_any_element()
            } else {
                row.whitespace_nowrap().min_w(px(width)).into_any_element()
            }
        })
        .size_full();

        // The web UI's `.log-view`: a bordered monospace surface.
        let pal = crate::theme::palette(cx);
        let surface = |view: gpui::Stateful<gpui::Div>| {
            view.flex_1()
                .min_h_0()
                .px_4()
                .py_3()
                .rounded_lg()
                .border_1()
                .border_color(pal.border)
                .bg(pal.surface)
        };
        root = root
            .child(if wrap {
                surface(div().id("log-wrapped")).child(list)
            } else {
                surface(div().id("log-horizontal"))
                    .overflow_x_scroll()
                    .child(list)
            })
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(Input::new(&self.search))
                    .child(Button::new("log-previous").label("N").on_click(
                        cx.listener(|view, _, _, cx| view.mutate(|v| v.find_again(true), cx)),
                    ))
                    .child(Button::new("log-next").label("n").on_click(
                        cx.listener(|view, _, _, cx| view.mutate(|v| v.find_again(false), cx)),
                    )),
            )
            .child(status);
        if let Some(error) = &self.error {
            root = root.child(div().text_color(cx.theme().danger).child(error.clone()));
        }
        root.on_key_down(cx.listener(|view, event: &KeyDownEvent, window, cx| {
            if event.keystroke.modifiers.alt || event.keystroke.modifiers.platform {
                return;
            }
            if !view.focus.is_focused(window) {
                if event.keystroke.key == "escape" && !event.keystroke.modifiers.control {
                    window.focus(&view.focus, cx);
                    view.error = None;
                    view.sync_inputs(window, cx);
                    cx.notify();
                    cx.stop_propagation();
                }
                return;
            }
            let Some(key) = crate::keys::from_keystroke(&event.keystroke, event.is_held) else {
                return;
            };
            if event.is_held
                && !matches!(
                    key.code,
                    KeyCode::Up
                        | KeyCode::Down
                        | KeyCode::Left
                        | KeyCode::Right
                        | KeyCode::PageUp
                        | KeyCode::PageDown
                        | KeyCode::Home
                        | KeyCode::End
                        | KeyCode::F(3)
                        | KeyCode::Char('n' | 'N' | 'h' | 'j' | 'k' | 'l' | 'g' | 'G' | 'u' | 'd')
                )
            {
                return;
            }
            if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('f') {
                view.search.update(cx, |input, cx| input.focus(window, cx));
                cx.stop_propagation();
                return;
            }
            if event.keystroke.modifiers.control
                && (key.modifiers != KeyModifiers::CONTROL
                    || !matches!(key.code, KeyCode::Char('u' | 'd')))
            {
                return;
            }
            if view
                .model
                .read(cx)
                .client
                .log_viewer(view.frontend)
                .is_some_and(|v| matches!(v.settings.mode, Mode::Session(_)))
            {
                if !event.keystroke.modifiers.control
                    && matches!(
                        key.code,
                        KeyCode::Esc
                            | KeyCode::Enter
                            | KeyCode::Up
                            | KeyCode::Down
                            | KeyCode::Char('j' | 'k')
                    )
                {
                    view.log_key(key, cx);
                    cx.stop_propagation();
                }
                return;
            }
            match key.code {
                KeyCode::Char('/') => {
                    view.search.update(cx, |input, cx| input.focus(window, cx));
                    cx.stop_propagation();
                    return;
                }
                KeyCode::Char('f') => {
                    view.filter.update(cx, |input, cx| input.focus(window, cx));
                    cx.stop_propagation();
                    return;
                }
                KeyCode::Char('m') => {
                    view.error = None;
                    view.limit.update(cx, |input, cx| {
                        input.focus(window, cx);
                        input.select_all(window, cx);
                    });
                    cx.stop_propagation();
                    return;
                }
                KeyCode::Char('v') => {
                    view.mutate(
                        |v| {
                            let next = v
                                .settings
                                .min_level
                                .map(|l| l.severity() as usize + 1)
                                .unwrap_or(0);
                            v.set_display_level(LogLevel::ALL.get(next).copied());
                        },
                        cx,
                    );
                    cx.stop_propagation();
                    return;
                }
                KeyCode::Char('L') => {
                    let level = view
                        .model
                        .read(cx)
                        .client
                        .log_viewer(view.frontend)
                        .filter(|v| v.capture_supported)
                        .map(|v| {
                            LogLevel::ALL
                                [(v.capture_level.severity() as usize + 1) % LogLevel::ALL.len()]
                        });
                    if let Some(level) = level {
                        view.command(Command::Capture(level), cx);
                    }
                    cx.stop_propagation();
                    return;
                }
                KeyCode::Esc
                | KeyCode::Backspace
                | KeyCode::F(3)
                | KeyCode::Up
                | KeyCode::Down
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::Home
                | KeyCode::End
                | KeyCode::PageUp
                | KeyCode::PageDown
                | KeyCode::Char(
                    'r' | 'w' | 's' | 'n' | 'N' | ' ' | 'h' | 'j' | 'k' | 'l' | 'g' | 'G' | 'u'
                    | 'd',
                ) => {}
                _ => return,
            }
            view.log_key(key, cx);
            cx.stop_propagation();
        }))
    }
}
