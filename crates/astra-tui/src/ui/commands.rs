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
        match id {
            Id::Palette => {
                let was_open = self.palette.is_some();
                self.close_replaceable_overlays();
                if !was_open {
                    self.palette = Some(shell::Palette::default());
                }
            }
            Id::Help => {
                let was_open = self.help_scroll.is_some();
                self.close_replaceable_overlays();
                self.help_scroll = (!was_open).then_some(0);
            }
            Id::Profiles => {
                self.close_replaceable_overlays();
                self.palette = Some(shell::Palette::profiles(self.kind));
            }
            Id::Tasks => {
                let was_open = self.client.tasks.showing_details();
                self.close_replaceable_overlays();
                self.client.tasks.details = (!was_open).then_some(0);
            }
            Id::Logs => {
                let was_open = self.tui_viewer.is_some();
                self.close_replaceable_overlays();
                if !was_open {
                    return Input::Run(Action::Logs("@tui".into()), None);
                }
            }
            Id::Monitor => {
                let was_open = self.monitor.is_some();
                self.close_replaceable_overlays();
                if !was_open {
                    self.client.open_monitor();
                }
            }
            Id::Settings => {
                self.close_replaceable_overlays();
                return self.navigate(shell::SETTINGS);
            }
            Id::CopyTerminal => return self.copy_terminal_commands(),
            _ => {
                let previous = self.client.preferences.clone();
                let input = self.client.run_command(id);
                self.apply_preferences(Some(&previous));
                return input;
            }
        }
        Input::Idle
    }
    fn close_replaceable_overlays(&mut self) {
        self.overlay_epoch = self.overlay_epoch.wrapping_add(1);
        self.palette = None;
        self.help_scroll = None;
        self.help_override = None;
        self.client.tasks.details = None;
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
