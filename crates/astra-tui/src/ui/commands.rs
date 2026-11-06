//! Shared global command descriptions, dispatch, palette and help bindings.
use super::*;

pub use astra_client::commands::*;
fn shortcut(key: event::KeyEvent) -> Option<Id> {
    astra_client::commands::shortcut(crate::input::key(key))
}
pub(super) fn repeatable(key: event::KeyEvent) -> bool {
    astra_client::commands::repeatable(crate::input::key(key))
}
impl Ui {
    pub(super) fn text_input_active(&self) -> bool {
        self.prompt
            .as_ref()
            .is_some_and(|p| !p.confirmation() && !p.picker())
            || self.connection_filtering
            || self.editor.as_ref().is_some_and(|e| !e.browsing())
            || self.viewer.as_ref().is_some_and(|v| !v.browsing())
            || self.palette.is_some()
    }
    pub(super) fn repeat_text_input_active(&self) -> bool {
        if self.monitor.is_some() {
            return false;
        }
        if self.client.tasks.showing_details()
            || self.help_scroll.is_some()
            || self.recovery.is_some()
        {
            return false;
        }
        if let Some(viewer) = &self.tui_viewer {
            return viewer.accepts_text_input();
        }
        if self.palette.is_some() {
            return true;
        }
        if let Some(prompt) = &self.prompt {
            return !prompt.confirmation() && !prompt.picker();
        }
        if self.binding_picker.is_some() || self.menu.is_some() {
            return false;
        }
        self.connection_filtering
            || self.editor.as_ref().is_some_and(Editor::accepts_text_input)
            || self
                .viewer
                .as_ref()
                .is_some_and(logs::Viewer::accepts_text_input)
    }
    pub(super) fn run_command(&mut self, id: Id) -> Input {
        if let Some(reason) = self.command_available(id) {
            self.message = t(reason).into();
            return Input::Idle;
        }
        let plain = |code| event::KeyEvent::new(code, KeyModifiers::NONE);
        match id {
            Id::Save
            | Id::Undo
            | Id::Redo
            | Id::Quit
            | Id::Shutdown
            | Id::Import
            | Id::Discard
            | Id::CompareDraft
            | Id::ExternalEditor
            | Id::ReadExternal => match self.client.run_command(id) {
                astra_client::state::Input::Idle => Input::Idle,
                astra_client::state::Input::Quit => Input::Quit,
                astra_client::state::Input::Run(action, origin) => Input::Run(action, origin),
                astra_client::state::Input::ExternalEditor => Input::ExternalEditor,
                astra_client::state::Input::Frontend(_) => {
                    unreachable!("model command returned a view effect")
                }
            },
            Id::CopyTerminal => self.copy_terminal_commands(),
            Id::Palette => {
                let was_open = self.palette.is_some();
                self.close_replaceable_overlays();
                if !was_open {
                    self.palette = Some(shell::Palette::default());
                }
                Input::Idle
            }
            Id::Help => {
                let was_open = self.help_scroll.is_some();
                self.close_replaceable_overlays();
                self.help_scroll = (!was_open).then_some(0);
                Input::Idle
            }
            Id::Profiles => {
                self.close_replaceable_overlays();
                self.palette = Some(shell::Palette::profiles(self.kind));
                Input::Idle
            }
            Id::Search => {
                if let Some(editor) = &mut self.editor {
                    let code = if editor.browsing() {
                        plain(KeyCode::Char('/'))
                    } else {
                        event::KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL)
                    };
                    let _ = editor.key(crate::input::key(code));
                } else if let Some(viewer) = &mut self.viewer {
                    viewer.key(crate::input::key(event::KeyEvent::new(
                        KeyCode::Char('f'),
                        KeyModifiers::CONTROL,
                    )));
                } else if matches!(self.page, shell::OVERVIEW | shell::CONNECTIONS) {
                    self.page = shell::CONNECTIONS;
                    self.connection_filtering = true;
                } else if self.page == shell::RULES {
                    return self.rule_search();
                } else {
                    let page = self.page;
                    self.prompt(Prompt::Find(page));
                }
                Input::Idle
            }
            Id::NextMatch | Id::PreviousMatch => {
                let backwards = id == Id::PreviousMatch;
                if let Some(editor) = &mut self.editor {
                    if editor.browsing() {
                        let _ = editor.find_again(backwards);
                    }
                } else if let Some(viewer) = &mut self.viewer {
                    viewer.key(crate::input::key(event::KeyEvent::new(
                        KeyCode::F(3),
                        if backwards {
                            KeyModifiers::SHIFT
                        } else {
                            KeyModifiers::NONE
                        },
                    )));
                } else if self.page == shell::CONNECTIONS && !self.connection_filter.is_empty() {
                    let count = self.connection_rows().len();
                    if count > 0 {
                        let current = self.connection_selection.min(count - 1);
                        self.select_connection(if backwards {
                            (current + count - 1) % count
                        } else {
                            (current + 1) % count
                        });
                    }
                } else {
                    let query = self.searches.get(&self.page).cloned().unwrap_or_default();
                    if !query.is_empty() {
                        self.find_direction(self.page, &query, backwards);
                    }
                }
                Input::Idle
            }
            Id::Tasks => {
                let was_open = self.client.tasks.showing_details();
                self.close_replaceable_overlays();
                if was_open {
                    Input::Idle
                } else {
                    self.background_key(plain(KeyCode::F(6)))
                        .unwrap_or(Input::Idle)
                }
            }
            Id::Logs => {
                let was_open = self.tui_viewer.is_some();
                self.close_replaceable_overlays();
                if was_open {
                    Input::Idle
                } else {
                    self.background_key(plain(KeyCode::F(7)))
                        .unwrap_or(Input::Idle)
                }
            }
            Id::Window => self.window_key(plain(KeyCode::F(8))).unwrap_or(Input::Idle),
            Id::Monitor => {
                let was_open = self.monitor.is_some();
                let name = if self.page == PROFILES || self.kind == Kind::Server {
                    self.selected()
                } else {
                    self.active_proxy().or_else(|| self.selected())
                }
                .map(|row| row.instance.name.clone());
                self.close_replaceable_overlays();
                if !was_open {
                    if let Some(name) = name {
                        self.monitor = Some(monitor::View::new(name));
                    } else {
                        self.message = t("Select an instance to monitor").into();
                    }
                }
                Input::Idle
            }
            Id::Pause => {
                if let Some(viewer) = &mut self.viewer {
                    viewer.key(crate::input::key(plain(KeyCode::Char(' '))));
                    Input::Idle
                } else {
                    self.toggle_live_pause()
                }
            }
            Id::VimKeys => {
                if !self.change_preferences(|p| p.vim_keys = !p.vim_keys) {
                    return Input::Idle;
                }
                self.message = if self.vim_keys {
                    "Vim aliases enabled"
                } else {
                    "Vim aliases disabled"
                }
                .into();
                Input::Idle
            }
            Id::MotionFull | Id::MotionReduced | Id::MotionOff => {
                let mode = match id {
                    Id::MotionFull => animation::MotionMode::Normal,
                    Id::MotionReduced => animation::MotionMode::Reduced,
                    _ => animation::MotionMode::Off,
                };
                self.change_preferences(|p| p.motion = mode);
                Input::Idle
            }
            Id::Settings => {
                self.close_replaceable_overlays();
                self.suspend_editor();
                self.navigate(shell::SETTINGS)
            }
        }
    }
    fn close_replaceable_overlays(&mut self) {
        self.overlay_epoch = self.overlay_epoch.wrapping_add(1);
        self.palette = None;
        self.help_scroll = None;
        self.help_override = None;
        if self.client.tasks.showing_details() {
            self.background_key(event::KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE));
        }
        self.tui_viewer = None;
        self.monitor = None;
    }
    pub(super) fn command_key(&mut self, key: event::KeyEvent) -> Option<Input> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            return Some(self.run_command(Id::Quit));
        }
        if let Some(
            id @ (Id::Help | Id::Palette | Id::Profiles | Id::Tasks | Id::Logs | Id::Monitor),
        ) = shortcut(key)
        {
            return Some(self.run_command(id));
        }
        // Only global view commands cross overlays. Editing shortcuts belong to
        // the visible editor, never a draft underneath a palette or task drawer.
        if self.palette.is_some()
            || self.help_scroll.is_some()
            || self.client.tasks.showing_details()
            || self.tui_viewer.is_some()
            || self.monitor.is_some()
        {
            return None;
        }
        if self
            .prompt
            .as_ref()
            .is_some_and(|prompt| prompt.confirmation() || prompt.picker())
            || self.binding_picker.is_some()
            || self.menu.is_some()
            || self.recovery.is_some()
        {
            return None;
        }
        let id = shortcut(key);
        if let Some(id) = id {
            return Some(self.run_command(id));
        }
        if self.text_input_active()
            || self.help_scroll.is_some()
            || self.diagnostics.is_some()
            || self.explanation.is_some()
        {
            return None;
        }
        if ctrl && matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) {
            let next = if key.code == KeyCode::PageUp {
                (self.page + shell::PAGES.len() - 1) % shell::PAGES.len()
            } else {
                (self.page + 1) % shell::PAGES.len()
            };
            self.suspend_editor();
            return Some(self.navigate(next));
        }
        if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) {
            match key.code {
                KeyCode::Char(c @ '1'..='7') => {
                    self.suspend_editor();
                    self.focus = Default::default();
                    return Some(self.navigate((c as u8 - b'1') as usize));
                }
                KeyCode::Char('p') => return Some(self.run_command(Id::Profiles)),
                KeyCode::Char(':') => return Some(self.run_command(Id::Palette)),
                KeyCode::Char('Q' | 'R' | '0') if self.editor.is_none() => {
                    self.message =
                        t("Use Ctrl-K / F2 for restart, reset or stop-all actions").into();
                    return Some(Input::Idle);
                }
                KeyCode::Char('q') => return Some(self.run_command(Id::Quit)),
                _ => {}
            }
        }
        None
    }
}

#[cfg(test)]
#[path = "../test/ui_commands_test.rs"]
mod tests;
