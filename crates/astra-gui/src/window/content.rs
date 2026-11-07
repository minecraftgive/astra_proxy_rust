use super::*;
#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Page(usize),
    Panel(Panel),
    Installation,
    Recovery,
}
pub(super) struct Mounted {
    key: Screen,
    pub(super) view: AnyView,
    pub(super) focus: FocusHandle,
    _subscription: Subscription,
}
impl AppWindow {
    fn mount<V: Render + Focusable + EventEmitter<views::ViewEvent>>(
        &mut self,
        key: Screen,
        view: Entity<V>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let focus = view.focus_handle(cx);
        let subscription = cx.subscribe_in(&view, window, |this, _, event, window, cx| {
            match event {
                views::ViewEvent::Input(input) => {
                    this.palette = false;
                    this.profile_chooser = false;
                    this.handle_input(input.clone(), window, cx);
                }
                views::ViewEvent::Navigate(page) => this.navigate(*page, window, cx),
                views::ViewEvent::Panel(panel) => this.show_panel(*panel, window, cx),
                views::ViewEvent::Dismiss => this.dismiss(window, cx),
                views::ViewEvent::Cancel(id) => this
                    .model
                    .update(cx, |model, cx| model.cancel_task(*id, cx)),
            }
            cx.notify();
        });
        window.focus(&focus, cx);
        self.mounted = Some(Mounted {
            key,
            view: view.into(),
            focus,
            _subscription: subscription,
        });
    }
    pub(super) fn content(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if let Some(error) = &self.model.read(cx).startup_error {
            return div()
                .p_6()
                .text_color(cx.theme().danger)
                .child(error.clone())
                .into_any_element();
        }
        if self
            .model
            .read(cx)
            .owns_overlay(crate::model::Overlay::Prompt, self.id)
            && self.model.read(cx).client.prompt.is_some()
            && self.model.read(cx).client.recovery.is_none()
        {
            return div()
                .size_full()
                .child(self.prompt.clone())
                .into_any_element();
        }
        if self.elevation.read(cx).visible() {
            return div()
                .relative()
                .size_full()
                .child(self.elevation.clone())
                .into_any_element();
        }
        let client = &self.model.read(cx).client;
        let key = if client.recovery.is_some()
            && self
                .model
                .read(cx)
                .owns_overlay(crate::model::Overlay::Recovery, self.id)
        {
            Screen::Recovery
        } else if client.installation.visible()
            && self
                .model
                .read(cx)
                .owns_overlay(crate::model::Overlay::Installation, self.id)
        {
            Screen::Installation
        } else if let Some(panel) = self.panel {
            Screen::Panel(
                if panel == Panel::Diagnostics
                    && client
                        .diagnostics
                        .as_ref()
                        .is_some_and(|view| view.tools.is_some())
                {
                    Panel::NetTools
                } else {
                    panel
                },
            )
        } else {
            Screen::Page(self.page.unwrap_or(client.page).min(6))
        };
        if self
            .mounted
            .as_ref()
            .is_none_or(|mounted| mounted.key != key)
        {
            let model = self.model.clone();
            macro_rules! mount {
                ($ty:path) => {{
                    let view = cx.new(|cx| <$ty>::new(model.clone(), window, cx));
                    self.mount(key, view, window, cx);
                }};
            }
            match key {
                Screen::Page(0) => mount!(views::overview::View),
                Screen::Page(1) => mount!(views::proxies::View),
                Screen::Page(2) => mount!(views::connections::View),
                Screen::Page(3) => mount!(views::rules::View),
                Screen::Page(4) => mount!(views::logs::View),
                Screen::Page(5) if model.read(cx).server_mode => {
                    mount!(views::server::View)
                }
                Screen::Page(5) => mount!(views::profiles::View),
                Screen::Page(_) => mount!(views::settings::SettingsView),
                Screen::Panel(Panel::Editor) => {
                    let editor = self
                        .editor
                        .get_or_insert_with(|| {
                            cx.new(|cx| views::editor::View::new(model.clone(), window, cx))
                        })
                        .clone();
                    self.mount(key, editor, window, cx);
                }
                Screen::Panel(Panel::Help) => mount!(views::help::View),
                Screen::Panel(Panel::Updates) => mount!(views::updates::UpdatesView),
                Screen::Panel(Panel::Tasks) => mount!(views::tasks::View),
                Screen::Panel(Panel::Monitor) => mount!(views::monitor::View),
                Screen::Panel(Panel::Diagnostics) => mount!(views::diagnostics::View),
                Screen::Panel(Panel::NetTools) => mount!(views::nettools::View),
                Screen::Panel(Panel::GuiLog) => {
                    let view = cx.new(|cx| views::logs::View::frontend(model, window, cx));
                    self.mount(key, view, window, cx);
                }
                Screen::Panel(Panel::Recovery) | Screen::Recovery => mount!(views::recovery::View),
                Screen::Installation => mount!(views::installation::InstallationView),
            }
        }
        let mounted = self.mounted.as_ref().unwrap();
        // The floating palette keeps focus while it is open over this view.
        if self.palette_view.is_none()
            && (self.focus.is_focused(window) || window.focused(cx).is_none())
        {
            window.focus(&mounted.focus, cx);
        }
        mounted.view.clone().into_any_element()
    }
}
