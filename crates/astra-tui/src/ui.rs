mod animation;
mod commands;
mod connections;
mod dns;
mod dns_query;
mod drafts;
mod event_loop;
mod focus;
mod installation;
mod server;
pub(crate) fn wake() {
    animation::wake();
}
mod background;
mod chain;
mod dashboard;
mod detached;
mod live;
mod monitor;
mod node_transfer;
mod operations;
mod package_choice;
mod pages;
pub(crate) use package_choice::choose_package;
mod settings;
pub(crate) fn preferred_terminal(home: &std::path::Path) -> String {
    settings::load(&home.join(settings::FILE), &Session::default())
        .0
        .terminal
}
mod shell;
mod startup;
pub(crate) use startup::{log_startup, run_silent};
#[cfg(test)]
#[path = "test/ui_stack_test.rs"]
mod stack_tests;
mod terminal_export;
#[cfg(test)]
#[path = "test/ui_test_rpc_test.rs"]
mod test_rpc;
mod update;
use crate::editor::Editor;
use crate::logs;
use crate::protocol::*;
use crate::{
    diagnostics::Render as _, editor::Render as _, explanation::Render as _, logs::Render as _,
};
use crate::{i18n::t, tr};
use anyhow::{Context, Result};
use astra_tui_kit::{TerminalOptions, TerminalSession};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use monitor::Render as _;
#[cfg(test)]
use ratatui::Terminal;
use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};
#[cfg(test)]
use serde::Deserialize;
use shell::{LOGS, PROFILES};
use std::{
    collections::HashMap,
    path::PathBuf,
    time::{Duration, Instant},
};
use update::ViewRender as _;

use astra_client::state::*;

struct Ui {
    client: Client,
    focus: focus::Focus,
    animations: animation::Motion,
    live_view: live::Viewports,
    dns_viewport: std::cell::Cell<usize>,
    dns_detail_max: std::cell::Cell<u16>,
    dns_query_max: std::cell::Cell<u16>,
    dns_status_max: std::cell::Cell<u16>,
    palette: Option<shell::Palette>,
    /// Reject late view-opening results after the user has switched overlays.
    profile_viewport: std::cell::Cell<usize>,
    /// Items reachable by Overview scrolling; narrow layouts include proxy groups.
    overview_cursor: usize,
    chain_focus: std::cell::RefCell<Option<(String, u32, chain::Source)>>,
    overview_item_count: std::cell::Cell<usize>,
    /// Proxy group unfolded in place in the Overview's group column, and the
    /// node row under the cursor inside it (None while the group row is focused).
    overview_unfolded: Option<String>,
    overview_member: Option<usize>,
    /// First time each transient notice was shown; errors remain until resolved.
    /// Next expiry from the last rendered notices, consumed once to wake an idle UI.
    hits: std::cell::RefCell<astra_tui_kit::HitMap<dashboard::Target>>,
    menu: Option<usize>,
    /// The pending Diagnostics action was requested as network tools.
    vim_g: bool,
    operation_steps: Vec<String>,
    operation_result: Option<String>,
    task_view: background::TaskView,
    monitor_max_scroll: std::cell::Cell<u16>,
    /// Open the editor's search as soon as a requested editor arrives.
    settings_cursor: usize,
    settings_view: settings::View,
    /// Ctrl+Home held on the accent setting: (first press, latest press, done).
    /// Mouse capture change for the event loop, which owns the terminal.
    mouse_request: Option<bool>,
    /// A "Not applied" update leaves the running version unchanged, so only a
    /// failure recorded after this UI started alerts, until F6 acknowledges it.
    update_alert_since_ms: u64,
    acknowledged_update: Option<String>,
}
impl std::ops::Deref for Ui {
    type Target = Client;
    fn deref(&self) -> &Client {
        &self.client
    }
}
impl std::ops::DerefMut for Ui {
    fn deref_mut(&mut self) -> &mut Client {
        &mut self.client
    }
}

impl Ui {
    fn key_binding_picker(&mut self, key: event::KeyEvent) -> Input {
        use astra_client::intents::BindingAction;
        let action = match key.code {
            KeyCode::Esc => BindingAction::Cancel,
            KeyCode::Up | KeyCode::Char('k') => BindingAction::Previous,
            KeyCode::Down | KeyCode::Char('j') => BindingAction::Next,
            KeyCode::Char(' ') => BindingAction::Toggle,
            KeyCode::Enter => BindingAction::Confirm,
            _ => return Input::Idle,
        };
        self.client.binding_action(action)
    }

    fn submit_prompt_with_recovery(&mut self, input: String) -> Input {
        match self.client.submit_prompt(input) {
            Ok(input) => input,
            Err(error) => {
                self.prompt = None;
                self.fail(
                    format!("{:#}", error.error),
                    Resume::Prompt(error.prompt, error.input),
                );
                Input::Idle
            }
        }
    }

    fn recover_key(&mut self, key: KeyCode) -> Input {
        use astra_client::prompts::RecoveryAction;
        match key {
            KeyCode::Esc | KeyCode::Char('b') => self.client.recover(RecoveryAction::Back),
            KeyCode::Char('c') => self.client.recover(RecoveryAction::Continue),
            KeyCode::Up => {
                self.scroll = self.scroll.saturating_sub(1);
                Input::Idle
            }
            KeyCode::Down => {
                self.scroll = self.scroll.saturating_add(1);
                Input::Idle
            }
            _ => Input::Idle,
        }
    }

    fn new(session: Session) -> Self {
        let animations = animation::Motion::new(session.motion);
        Self {
            client: Client::from_session(session).with_waker(astra_client::waker::Waker::new(wake)),
            focus: Default::default(),
            animations,
            live_view: Default::default(),
            dns_viewport: Default::default(),
            dns_detail_max: Default::default(),
            dns_query_max: Default::default(),
            dns_status_max: Default::default(),
            palette: None,
            profile_viewport: std::cell::Cell::new(1),
            overview_cursor: 0,
            chain_focus: Default::default(),
            overview_item_count: Default::default(),
            overview_unfolded: None,
            overview_member: None,
            hits: Default::default(),
            menu: None,
            vim_g: false,
            operation_steps: Vec::new(),
            operation_result: None,
            task_view: Default::default(),
            monitor_max_scroll: Default::default(),
            settings_cursor: 0,
            settings_view: Default::default(),
            mouse_request: None,
            update_alert_since_ms: astra_ipc::progress::now_ms(),
            acknowledged_update: None,
        }
    }
    /// Switches the Profiles list between proxy instances and rule cores.
    fn paste(&mut self, text: &str) {
        if self.monitor.is_some() {
            return;
        }
        if self.background_paste(text) {
            return;
        }
        if self.help_scroll.is_some() {
            return;
        }
        if let Some(palette) = &mut self.palette {
            palette.insert(text);
            return;
        }
        if self.recovery.is_some()
            || self.menu.is_some()
            || self.binding_picker.is_some()
            || self.diagnostics.is_some()
            || self.explanation.is_some()
            || self.prompt.as_ref().is_some_and(Prompt::confirmation)
        {
            return;
        }
        if self.page == shell::DNS && self.dns_state.query_open {
            if self.dns_state.query_field == 0 {
                let room = 253usize.saturating_sub(self.dns_state.target.len());
                self.dns_state.target.extend(
                    text.chars()
                        .filter(|c| c.is_ascii() && !c.is_control())
                        .take(room),
                );
            }
        } else if self.page == shell::DNS && self.dns_state.filtering {
            self.dns_state
                .filter
                .extend(text.chars().filter(|c| !c.is_control()));
            self.dns_state.selected = None;
        } else {
            self.client.paste_content(text);
        }
    }
    fn key(&mut self, mut key: event::KeyEvent) -> Input {
        if key.kind == KeyEventKind::Release {
            return Input::Idle;
        }
        if self.installation.visible() {
            return self.installation_key(key);
        }
        if key.kind == KeyEventKind::Repeat {
            let navigation = matches!(
                key.code,
                KeyCode::Up
                    | KeyCode::Down
                    | KeyCode::Left
                    | KeyCode::Right
                    | KeyCode::Home
                    | KeyCode::End
                    | KeyCode::PageUp
                    | KeyCode::PageDown
            );
            let text = self.repeat_text_input_active()
                && matches!(
                    key.code,
                    KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Delete
                )
                && !key.modifiers.contains(KeyModifiers::CONTROL);
            if !navigation && !text && !commands::repeatable(key) {
                return Input::Idle;
            }
        }
        if let Some(input) = self.marquee_hold_key(key) {
            return input;
        }
        if self.keymap_notice_pending
            && matches!(key.code, KeyCode::Enter | KeyCode::Esc | KeyCode::F(1))
        {
            self.keymap_notice_pending = false;
            self.help_override = None;
            self.help_scroll = if key.code == KeyCode::F(1) {
                Some(0)
            } else {
                None
            };
            return Input::Idle;
        }
        // NetTool owns its page shortcuts before global navigation. Overlays
        // above it retain their own input and application commands.
        if !self.keymap_notice_pending
            && self.palette.is_none()
            && self.help_scroll.is_none()
            && self.monitor.is_none()
            && !self.client.tasks.showing_details()
            && self.tui_viewer.is_none()
            && self.explanation.is_none()
            && self.recovery.is_none()
            && self.prompt.is_none()
            && self.menu.is_none()
            && self.binding_picker.is_none()
        {
            if let Some(tools) = self
                .diagnostics
                .as_mut()
                .and_then(|view| view.tools.as_mut())
            {
                if matches!(key.code, KeyCode::F(1..=12)) {
                    return Input::Idle;
                }
                if key.modifiers == KeyModifiers::CONTROL
                    && matches!(
                        key.code,
                        KeyCode::Char('d' | 'p' | 'r' | 'o' | 'y' | 'e' | 'l')
                            | KeyCode::Up
                            | KeyCode::Down
                            | KeyCode::Left
                            | KeyCode::Right
                    )
                {
                    tools.key(crate::input::key(key));
                    return Input::Idle;
                }
            }
        }
        // Window shortcuts are global, including when task/log overlays own input.
        if let Some(input) = self.window_key(key) {
            return input;
        }
        // Alt-page chords above are the only Alt bindings. Do not reinterpret
        // other modifier combinations as unmodified page actions or text.
        if key.modifiers.intersects(
            KeyModifiers::ALT | KeyModifiers::SUPER | KeyModifiers::HYPER | KeyModifiers::META,
        ) {
            return Input::Idle;
        }
        if let Some(input) = self.command_key(key) {
            return input;
        }
        let max_scroll = self.monitor_max_scroll.get();
        if let Some(view) = &mut self.monitor {
            if key.code == KeyCode::Esc {
                self.monitor = None;
            } else {
                view.max_scroll = max_scroll;
                view.key(crate::input::key(key));
            }
            return Input::Idle;
        }
        if self.client.tasks.showing_details() || self.tui_viewer.is_some() {
            if let Some(input) = self.background_key(key) {
                return input;
            }
        }
        // Editable palette text must never pass through underlying editor aliases.
        if self.palette.is_some() {
            return self.palette_key(key);
        }
        if self.page == shell::SETTINGS && self.terminals.picker.is_some() && self.live_input() {
            if let Some(input) = self.settings_key(key) {
                return input;
            }
        }
        if self.page == shell::DNS
            && self.live_input()
            && (self.dns_state.filtering || self.dns_state.query_open)
        {
            return self.dns_key(key).unwrap_or(Input::Idle);
        }
        if let Some(input) = self.focus_key(key) {
            return input;
        }
        if !self.text_input_active()
            && !self.vim_keys
            && matches!(key.code, KeyCode::Char('h' | 'j' | 'k' | 'l' | 'g' | 'G'))
        {
            return Input::Idle;
        }
        if self.editor.as_ref().is_some_and(Editor::browsing)
            || (self.page == PROFILES && self.prompt.is_none() && self.menu.is_none())
        {
            if key.code == KeyCode::Insert {
                key.code = KeyCode::Char('n');
            }
            if key.code == KeyCode::Delete {
                key.code = KeyCode::Char('d');
            }
        }
        if let Some(input) = self.background_key(key) {
            return input;
        }
        if self.page == shell::DNS && self.live_input() {
            if let Some(input) = self.dns_key(key) {
                return input;
            }
        }
        // Dialogs and local input modes consume Escape before page history.
        if key.code == KeyCode::Esc
            && self.can_switch_page()
            && !self.connection_filtering
            && self.runtime_picker.is_none()
            && self.terminals.picker.is_none()
            && !self.live_detail_open()
            && self.editor.is_none()
            && !(self.page == shell::CONNECTIONS && !self.connection_filter.is_empty())
        {
            return self.navigate_back();
        }
        if let Some(input) = self.viewer_page_key(key) {
            return input;
        }
        if self.page == shell::SETTINGS && self.live_input() {
            if let Some(input) = self.settings_key(key) {
                return input;
            }
        }
        if self.live_input() {
            if let Some(input) = self.live_key(key) {
                return input;
            }
        }
        if self.overview_input() {
            if let Some(input) = self.page_key(key) {
                return input;
            }
        }
        self.key_context(key)
    }

    fn key_context(&mut self, key: event::KeyEvent) -> Input {
        if self.help_scroll.is_some()
            && key.modifiers == KeyModifiers::CONTROL
            && key.code == KeyCode::Char('c')
        {
            return Input::Quit;
        }
        if let Some(scroll) = &mut self.help_scroll {
            match key.code {
                KeyCode::Esc | KeyCode::Char('?') => self.help_scroll = None,
                KeyCode::Down | KeyCode::Char('j') => *scroll = scroll.saturating_add(1),
                KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
                KeyCode::PageDown => *scroll = scroll.saturating_add(10),
                KeyCode::PageUp => *scroll = scroll.saturating_sub(10),
                KeyCode::Home | KeyCode::Char('g') => *scroll = 0,
                _ => {}
            }
            return Input::Idle;
        }
        if key.code == KeyCode::Char('?')
            && self.prompt.is_none()
            && self.recovery.is_none()
            && self.editor.as_ref().is_none_or(Editor::browsing)
            && self.viewer.as_ref().is_none_or(logs::Viewer::browsing)
        {
            self.help_scroll = Some(0);
            return Input::Idle;
        }
        if self.recovery.is_some() && !key.modifiers.contains(KeyModifiers::CONTROL) {
            return self.recover_key(key.code);
        }
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c') {
            // Detach with the draft intact; the next launch restores this exact step.
            return Input::Quit;
        }
        if self.recovery.is_some() {
            return Input::Idle;
        }
        if let Some(view) = &mut self.explanation {
            match key.code {
                KeyCode::Esc => self.explanation = None,
                KeyCode::Down | KeyCode::Char('j') => view.scroll = view.scroll.saturating_add(1),
                KeyCode::Up | KeyCode::Char('k') => view.scroll = view.scroll.saturating_sub(1),
                KeyCode::PageDown => view.scroll = view.scroll.saturating_add(10),
                KeyCode::PageUp => view.scroll = view.scroll.saturating_sub(10),
                _ => {}
            }
            return Input::Idle;
        }
        if let Some(view) = &mut self.diagnostics {
            if key.code == KeyCode::Esc && !view.handles_escape() {
                self.diagnostics = None;
            } else {
                view.key(crate::input::key(key));
                if view.closed() {
                    self.diagnostics = None;
                }
            }
            return Input::Idle;
        }
        if self.binding_picker.is_some() {
            return self.key_binding_picker(key);
        }
        if self.menu.is_some() {
            return self.key_operations_menu(key);
        }
        if let Some(input) = self.key_node_transfer(key) {
            return input;
        }
        if self.prompt.as_ref().is_some_and(Prompt::confirmation) {
            return self.key_confirm_prompt(key);
        }
        if self.prompt.is_some() {
            return self.key_text_prompt(key);
        } else if key.code == KeyCode::Char('t')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && self.kind == Kind::Rule
            && self.editor.as_ref().is_some_and(Editor::browsing)
        {
            let name = self.editor.as_ref().unwrap().name.clone();
            self.prompt(Prompt::Explain(name));
        } else if self.editor.is_some() {
            return self.key_editor_focus(key);
        } else if self.viewer.is_some() {
            return self.key_log_focus(key);
        } else if self.page == PROFILES {
            return self.key_profiles_focus(key);
        }
        Input::Idle
    }

    fn key_operations_menu(&mut self, key: event::KeyEvent) -> Input {
        if let Some(selected) = self.menu {
            let operations = self.operations();
            match key.code {
                KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') => self.menu = None,
                KeyCode::Up | KeyCode::Char('k') => self.menu = Some(selected.saturating_sub(1)),
                KeyCode::Down | KeyCode::Char('j') => {
                    self.menu = Some((selected + 1).min(operations.len().saturating_sub(1)))
                }
                KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                    self.menu = None;
                    if let Some(operation) = operations.get(selected) {
                        return self.operation(*operation);
                    }
                }
                _ => {}
            }
            return Input::Idle;
        }
        Input::Idle
    }

    fn key_confirm_prompt(&mut self, key: event::KeyEvent) -> Input {
        if self.prompt.as_ref().is_some_and(Prompt::confirmation) {
            match key.code {
                KeyCode::Esc => self.prompt = None,
                KeyCode::Tab | KeyCode::BackTab | KeyCode::Left | KeyCode::Right => {
                    self.confirmation_accept = !self.confirmation_accept
                }
                KeyCode::Enter if !self.confirmation_accept => self.prompt = None,
                KeyCode::Enter => {
                    return self.submit_prompt_with_recovery("yes".into());
                }
                KeyCode::Up => self.scroll = self.scroll.saturating_sub(1),
                KeyCode::Down => self.scroll = self.scroll.saturating_add(1),
                _ => {}
            }
            return Input::Idle;
        }
        Input::Idle
    }

    fn key_text_prompt(&mut self, key: event::KeyEvent) -> Input {
        match key.code {
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.clear();
                self.input_cursor = 0;
                self.prefilled_input = false;
            }
            KeyCode::Esc => self.client.cancel_prompt(),
            KeyCode::Left => {
                self.prefilled_input = false;
                self.input_cursor = self.input_cursor.saturating_sub(1);
            }
            KeyCode::Right => {
                self.prefilled_input = false;
                self.input_cursor = (self.input_cursor + 1).min(self.input.chars().count());
            }
            KeyCode::Home => {
                self.prefilled_input = false;
                self.input_cursor = 0;
            }
            KeyCode::End => {
                self.prefilled_input = false;
                self.input_cursor = self.input.chars().count();
            }
            KeyCode::Backspace | KeyCode::Delete if self.prefilled_input => {
                self.input.clear();
                self.input_cursor = 0;
                self.prefilled_input = false;
            }
            KeyCode::Backspace if self.input_cursor > 0 => {
                self.input_cursor -= 1;
                let byte = self.prompt_byte();
                self.input.remove(byte);
            }
            KeyCode::Delete if self.input_cursor < self.input.chars().count() => {
                let byte = self.prompt_byte();
                self.input.remove(byte);
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if self.prefilled_input {
                    self.input.clear();
                    self.input_cursor = 0;
                    self.prefilled_input = false;
                }
                let byte = self.prompt_byte();
                self.input.insert(byte, c);
                self.input_cursor += 1;
            }
            KeyCode::Enter => {
                return self.submit_prompt_with_recovery(self.input.clone());
            }
            _ => {}
        }
        Input::Idle
    }

    fn key_editor_focus(&mut self, key: event::KeyEvent) -> Input {
        use astra_client::intents::EditorAction;
        let editor = self.editor.as_ref().unwrap();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let action = if editor.requests_subscription_update(crate::input::key(key)) {
            Some(EditorAction::UpdateSubscriptions)
        } else if ctrl && key.code == KeyCode::Char('o') {
            Some(EditorAction::Import)
        } else if ctrl && matches!(key.code, KeyCode::Char('i' | 'I')) && editor.browsing() {
            Some(EditorAction::ImportItems {
                rules: key.code == KeyCode::Char('I')
                    || key.modifiers.contains(KeyModifiers::SHIFT),
            })
        } else if ctrl && matches!(key.code, KeyCode::Char('e' | 'E')) && editor.browsing() {
            Some(EditorAction::ExportItems {
                share: key.code == KeyCode::Char('E')
                    || key.modifiers.contains(KeyModifiers::SHIFT),
            })
        } else if ctrl && key.code == KeyCode::Char('s') {
            Some(EditorAction::Save)
        } else if ctrl && key.code == KeyCode::Char('f') && editor.browsing() {
            Some(EditorAction::Search)
        } else if key.code == KeyCode::Esc && editor.can_close() {
            Some(EditorAction::Suspend)
        } else {
            None
        };
        if let Some(action) = action {
            return self.client.editor_action(action);
        }
        if let Err(error) = self.editor.as_mut().unwrap().key(crate::input::key(key)) {
            self.fail(format!("{error:#}"), Resume::Editor);
        }
        Input::Idle
    }

    fn key_log_focus(&mut self, key: event::KeyEvent) -> Input {
        let command = self.viewer.as_mut().unwrap().key(crate::input::key(key));
        self.client.log_command(command)
    }

    fn key_profiles_focus(&mut self, key: event::KeyEvent) -> Input {
        let index = self.index();
        let name = self.selected().map(|r| r.instance.name.clone());
        if key.code == KeyCode::Char('g') && key.modifiers.is_empty() {
            if self.vim_g {
                self.selections[index] = 0;
                self.vim_g = false;
            } else {
                self.vim_g = true;
            }
            return Input::Idle;
        }
        self.vim_g = false;
        match key.code {
            KeyCode::Char('q') => return Input::Quit,
            KeyCode::Tab => {
                let kind = next_kind(self.kind);
                self.client.switch_core(kind);
            }
            KeyCode::Left | KeyCode::Char('h') => self.switch_core(Kind::Proxy),
            KeyCode::Right | KeyCode::Char('l') => self.switch_core(Kind::Rule),
            KeyCode::Char('3') => self.switch_core(Kind::Server),
            KeyCode::Up | KeyCode::Char('k') => {
                self.selections[index] = self.selections[index].saturating_sub(1)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.selections[index] =
                    (self.selections[index] + 1).min(self.rows().len().saturating_sub(1))
            }
            KeyCode::Home => self.selections[index] = 0,
            KeyCode::End | KeyCode::Char('G') => {
                self.selections[index] = self.rows().len().saturating_sub(1)
            }
            KeyCode::PageDown => {
                self.selections[index] = (self.selections[index]
                    + self.profile_viewport.get().max(1))
                .min(self.rows().len().saturating_sub(1))
            }
            KeyCode::PageUp => {
                self.selections[index] =
                    self.selections[index].saturating_sub(self.profile_viewport.get().max(1))
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.selections[index] = (self.selections[index]
                    + self.profile_viewport.get().max(1))
                .min(self.rows().len().saturating_sub(1))
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.selections[index] =
                    self.selections[index].saturating_sub(self.profile_viewport.get().max(1))
            }
            KeyCode::Char('n') => {
                return self
                    .client
                    .profile_action(astra_client::intents::ProfileAction::Create)
            }
            KeyCode::Enter if name.is_some() => self.menu = Some(0),
            KeyCode::Char(c)
                if name.is_some()
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                // Activation is explicit; Space is reserved for local selection/pause.
                let operation = match c {
                    'c' => {
                        return self
                            .client
                            .profile_action(astra_client::intents::ProfileAction::Toggle)
                    }
                    's' => Operation::Start,
                    'x' => Operation::Stop,
                    'r' => Operation::Reload,
                    'e' => Operation::Edit,
                    'L' => Operation::Logs,
                    't' => {
                        return self
                            .client
                            .profile_action(astra_client::intents::ProfileAction::Inspect)
                    }
                    'b' => Operation::Bind,
                    'd' => Operation::Remove,
                    'y' => Operation::Copy,
                    'E' => Operation::ExportConfig,
                    'P' => Operation::ExportTerminal,
                    'v' => {
                        return self
                            .client
                            .profile_action(astra_client::intents::ProfileAction::Validate)
                    }
                    _ => return Input::Idle,
                };
                return self.operation(operation);
            }
            _ => {}
        }
        Input::Idle
    }

    fn operation_text(&self) -> String {
        if self.operation_steps.is_empty()
            || self
                .operation_result
                .as_ref()
                .is_some_and(|result| result != &self.message)
        {
            return t(&self.message).to_owned();
        }
        self.operation_steps.last().cloned().unwrap_or_default()
    }
    #[cfg(test)]
    async fn execute(
        &mut self,
        socket: &std::path::Path,
        action: Action,
        origin: Option<(Prompt, String)>,
    ) -> bool {
        let input = operations::OperationInput::capture(self);
        let result = operations::perform(socket, input, action.clone(), origin).await;
        self.apply_operation(
            result,
            &action,
            operations::ApplyContext {
                same: true,
                log_target_current: true,
                background: false,
            },
        )
    }
    fn draw(&self, terminal: &mut astra_tui_kit::Screen) -> Result<()> {
        astra_tui_kit::frame::draw(terminal, |frame| {
            self.render(frame);
            self.animations.finish_frame(frame);
        })?;
        Ok(())
    }
    /// Details of the selected instance: labels muted, state in its status color.
    fn selection_status(&self) -> Text<'static> {
        let Some(row) = self.selected() else {
            return Text::default();
        };
        let muted = Style::default().fg(dashboard::MUTED);
        let mut text = Text::from(vec![
            Line::styled(row.instance.config.display().to_string(), muted),
            Line::from(vec![
                Span::styled(format!("{}: ", t("State")), muted),
                Span::styled(
                    t(&row.state).to_owned(),
                    Style::default().fg(crate::theme::state_color(&row.state)),
                ),
            ]),
            Line::from(vec![
                Span::styled("PID ", muted),
                Span::raw(
                    row.pid
                        .map(|pid| pid.to_string())
                        .unwrap_or_else(|| "—".into()),
                ),
                Span::styled(format!(" · {} ", t("Uptime")), muted),
                Span::raw(uptime(row.uptime)),
            ]),
        ]);
        let mut lines: Vec<String> = Vec::new();
        if let Some(details) = self.details.get(&row.instance.name) {
            if !details.modes.is_empty() {
                lines.push(
                    details
                        .modes
                        .iter()
                        .map(|mode| t(mode))
                        .collect::<Vec<_>>()
                        .join(" + "),
                );
            }
            lines.extend(details.listeners.iter().cloned());
            if !details.bindings.is_empty() {
                lines.push(format!(
                    "{}: {}",
                    t("Startup bindings"),
                    details.bindings.join(", ")
                ));
            }
            lines.push(format!(
                "{}: {}",
                t("Log level"),
                details.log_level.as_str().to_uppercase()
            ));
            lines.push(if row.instance.kind == Kind::Rule {
                tr!("{} rules", details.rules)
            } else {
                tr!(
                    "{} outbounds: {}",
                    details.outbound_count,
                    details.outbounds.join(", ")
                )
            });
        }
        text.extend(lines.into_iter().map(Line::from));
        text
    }
    fn render(&self, f: &mut Frame<'_>) {
        self.hits.borrow_mut().clear();
        if let Some(view) = &self.monitor {
            self.monitor_max_scroll.set(view.render(f, f.area()));
            return;
        }
        if let Some(view) = &self.diagnostics {
            view.render(f, f.area());
            self.render_palette(f);
            if let Some(scroll) = self.help_scroll {
                let text = self.command_help();
                let area = f.area();
                crate::help::render(
                    f,
                    area,
                    &text,
                    scroll,
                    Block::default().borders(Borders::ALL).title(t("Help")),
                    self.help_override.is_none(),
                );
            }
            self.render_background(f);
            return;
        }
        let content_area = self.render_frame(f);
        let list_page = self.page == PROFILES
            && self.editor.is_none()
            && self.viewer.is_none()
            && self.recovery.is_none();
        let detail_height = if self.prompt.is_some() {
            3
        } else if list_page {
            (Paragraph::new(self.selection_status())
                .wrap(Wrap { trim: false })
                .line_count(content_area.width.max(1)) as u16)
                .min((content_area.height / 2).max(1))
        } else {
            0
        };
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(detail_height)])
            .split(content_area);
        self.render_content(f, chunks[0]);
        self.render_detail(f, chunks[1], list_page);
        self.render_import_confirmation(f);
        self.render_force_start_confirmation(f);
        self.render_node_transfer(f);
        self.render_update_confirmation(f);
        if let Some(view) = &self.diagnostics {
            view.render(f, f.area());
        }
        if let Some(view) = &self.explanation {
            view.render(f, f.area());
        }
        self.render_selection_menu(f);
        self.render_palette(f);
        self.render_page_help(f);
        self.render_background(f);
        self.render_installation(f);
    }

    fn render_content(&self, f: &mut Frame<'_>, area: ratatui::layout::Rect) {
        if let Some(recovery) = &self.recovery {
            f.render_widget(
                Paragraph::new(&*recovery.message)
                    .scroll((self.scroll, 0))
                    .wrap(Wrap { trim: false })
                    .style(Style::default().fg(Color::Yellow))
                    .block(Block::default().title(t("Paused")).borders(Borders::ALL)),
                area,
            );
        } else if let Some(editor) = &self.editor {
            editor.render(f, area);
        } else if self.page == LOGS {
            self.render_logs_page(f, area);
        } else if let Some(viewer) = &self.viewer {
            viewer.render(f, area);
        } else if self.page == shell::SETTINGS {
            self.render_settings(f, area);
        } else if self.kind == Kind::Server && self.page != PROFILES {
            self.render_server(f, area);
        } else {
            match self.page {
                shell::OVERVIEW => self.render_overview(f, area),
                shell::PROXIES => self.render_live_groups(f, area),
                shell::CONNECTIONS => self.render_connections(f, area),
                shell::DNS => self.render_dns(f, area),
                shell::RULES => self.render_rules(f, area),
                _ => self.render_profiles(f, area),
            }
        }
    }

    fn render_detail(&self, f: &mut Frame<'_>, area: ratatui::layout::Rect, list_page: bool) {
        let detail: Text = if let Some(prompt) = &self.prompt {
            if prompt.picker() {
                prompt.title().into()
            } else if prompt.confirmation() {
                format!(
                    "{}\n{}   {} · Tab / ←→ select · Enter apply · Esc cancel",
                    prompt.title(),
                    if self.confirmation_accept {
                        "  Cancel  "
                    } else {
                        "[ Cancel ]"
                    },
                    if self.confirmation_accept {
                        "[ Confirm ]"
                    } else {
                        "  Confirm  "
                    }
                )
                .into()
            } else {
                let byte = self.prompt_byte();
                format!(
                    "{}\n> {}▏{}",
                    prompt.title(),
                    &self.input[..byte],
                    &self.input[byte..]
                )
                .into()
            }
        } else if list_page {
            self.selection_status()
        } else {
            Text::default()
        };
        f.render_widget(
            Paragraph::new(detail)
                .wrap(Wrap { trim: false })
                .style(if self.prompt.is_some() {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default()
                }),
            area,
        );
    }

    fn render_import_confirmation(&self, f: &mut Frame<'_>) {
        if let Some(Prompt::ImportConfirm { notices, .. }) = &self.prompt {
            let popup = crate::help::popup(f.area(), notices);
            f.render_widget(Clear, popup);
            f.render_widget(
                Paragraph::new(notices.as_str())
                    .wrap(Wrap { trim: false })
                    .scroll((self.scroll, 0))
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(t("Replace editor draft?"))
                            .title_bottom(if self.confirmation_accept {
                                t("Cancel   [Confirm] · Tab select · Enter apply · Esc cancel")
                            } else {
                                t("[Cancel]   Confirm · Tab select · Enter apply · Esc cancel")
                            }),
                    ),
                popup,
            );
        }
    }

    fn render_force_start_confirmation(&self, f: &mut Frame<'_>) {
        if let Some(Prompt::ForceStart(name)) = &self.prompt {
            let text = Prompt::ForceStart(name.clone()).body().unwrap();
            let popup = crate::help::popup(f.area(), &text);
            f.render_widget(Clear, popup);
            f.render_widget(
                Paragraph::new(text)
                    .wrap(Wrap { trim: false })
                    .scroll((self.scroll, 0))
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(t("Force start and replace previous sessions?"))
                            .title_bottom(if self.confirmation_accept {
                                t("Cancel   [Confirm] · Tab select · Enter apply · Esc cancel")
                            } else {
                                t("[Cancel]   Confirm · Tab select · Enter apply · Esc cancel")
                            }),
                    ),
                popup,
            );
        }
    }

    fn render_update_confirmation(&self, f: &mut Frame<'_>) {
        if let Some(Prompt::UpdateConfirm(request)) = &self.prompt {
            let text = Prompt::UpdateConfirm(request.clone()).body().unwrap();
            let popup = crate::help::popup(f.area(), &text);
            f.render_widget(Clear, popup);
            f.render_widget(
                Paragraph::new(text).wrap(Wrap { trim: false }).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(t("Confirm update"))
                        .title_bottom(if self.confirmation_accept {
                            t("Cancel   [Confirm] · Tab select · Enter apply · Esc cancel")
                        } else {
                            t("[Cancel]   Confirm · Tab select · Enter apply · Esc cancel")
                        }),
                ),
                popup,
            );
        }
    }

    fn render_selection_menu(&self, f: &mut Frame<'_>) {
        if self.menu.is_some() || self.binding_picker.is_some() {
            let height = if let Some(picker) = &self.binding_picker {
                picker.choices.len() + 2
            } else {
                self.operations().len() + 2
            };
            let area = f.area();
            let width = area.width.saturating_sub(4).min(64);
            let height = (height as u16).min(18).min(area.height.saturating_sub(2));
            let popup = ratatui::layout::Rect::new(
                area.x + area.width.saturating_sub(width) / 2,
                area.y + area.height.saturating_sub(height) / 2,
                width,
                height,
            );
            f.render_widget(Clear, popup);
            self.hits.borrow_mut().clear();
            let (items, selected, title, help): (Vec<ListItem>, usize, String, &str) =
                if let Some(picker) = &self.binding_picker {
                    (
                        picker
                            .choices
                            .iter()
                            .map(|(name, kind)| {
                                ListItem::new(format!(
                                    "[{}] {}  ·  {}",
                                    if picker.targets.contains(name) {
                                        "✓"
                                    } else {
                                        " "
                                    },
                                    name,
                                    t(if *kind == Kind::Proxy {
                                        "Proxy"
                                    } else {
                                        "Rules"
                                    })
                                ))
                            })
                            .collect(),
                        picker.selected,
                        tr!("Startup bindings · {}", picker.name),
                        t("Space select · Enter save · Esc cancel"),
                    )
                } else {
                    (
                        self.operations()
                            .iter()
                            .map(|operation| ListItem::new(operation.label()))
                            .collect(),
                        self.menu.unwrap_or(0),
                        tr!(
                            "Actions · {}",
                            self.selected()
                                .map(|row| row.instance.name.as_str())
                                .unwrap_or_default()
                        ),
                        t("↑↓ select · Enter apply · Esc back"),
                    )
                };
            let mut state = ListState::default().with_selected(Some(selected));
            f.render_stateful_widget(
                List::new(items)
                    .block(
                        Block::default()
                            .title(title)
                            .title_bottom(help)
                            .borders(Borders::ALL),
                    )
                    .highlight_style(crate::theme::selected())
                    .highlight_symbol("› "),
                popup,
                &mut state,
            );
            self.menu_hits(popup, state.offset());
        }
    }

    fn render_page_help(&self, f: &mut Frame<'_>) {
        if let Some(scroll) = self.help_scroll {
            let text = self.command_help();
            let popup = crate::help::popup(f.area(), &text);
            crate::help::render(
                f,
                popup,
                &text,
                scroll,
                Block::default()
                    .title(t("Page operations"))
                    .title_bottom(Line::from(crate::theme::key_hints(t(
                        "↑↓ scroll · ? / Esc close",
                    ))))
                    .borders(Borders::ALL),
                self.help_override.is_none(),
            );
        }
    }
}

pub use astra_client::operations::action_description;

fn uptime(seconds: u64) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

pub async fn run(
    socket: PathBuf,
    context: crate::windows::Context,
    launch: crate::windows::Launch,
    resume_ui: bool,
    restart_ready: Option<PathBuf>,
) -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(event_loop::run_local(
            socket,
            context,
            launch,
            resume_ui,
            restart_ready,
        ))
        .await
}
/// Ctrl-C is normally a raw-mode key event, but sudo needs a real terminal.
/// Ignore the signal in this process while sudo runs so it reaches sudo only.
#[cfg(unix)]
struct IgnoreInterrupt(libc::sighandler_t);
#[cfg(unix)]
impl IgnoreInterrupt {
    fn install() -> Self {
        Self(unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) })
    }
}
#[cfg(unix)]
impl Drop for IgnoreInterrupt {
    fn drop(&mut self) {
        unsafe {
            libc::signal(libc::SIGINT, self.0);
        }
    }
}

/// Ask the manager for a hand-off, run sudo on the real terminal, and wait for
/// the elevated core as a background job so the UI stays responsive.
#[cfg(unix)]
async fn elevate(
    ui: &mut Ui,
    jobs: &mut background::Jobs,
    socket: &std::path::Path,
    name: String,
    expected_pid: Option<u32>,
    terminal: &mut astra_tui_kit::Screen,
    screen: &mut TerminalSession,
) {
    let reply = match call(
        socket,
        Action::PrepareElevation {
            name: name.clone(),
            expected_pid,
        },
    )
    .await
    {
        Ok(reply) => reply,
        Err(error) => {
            ui.message = format!("{error:#}");
            return;
        }
    };
    let offer: astra_platform::unix_handoff::Offer = match serde_json::from_str(&reply.text) {
        Ok(offer) => offer,
        Err(error) => {
            ui.message = format!("{error:#}");
            return;
        }
    };
    // Re-check the paths immediately before launch. Installation trust is
    // advisory for Astra; the system sudo helper must remain root-controlled.
    let (program, sudo) = match astra_platform::unix_handoff::inspect_program(&offer.binary)
        .and_then(|program| {
            astra_platform::unix_handoff::ensure_root_controlled(&offer.sudo)
                .map(|sudo| (program, sudo))
        }) {
        Ok(paths) => paths,
        Err(error) => {
            ui.message = format!("{error:#}");
            return;
        }
    };
    let launched = (|| -> Result<std::process::ExitStatus> {
        screen.suspend()?;
        if let Some(warning) = &program.warning {
            use std::io::Write;
            let _ = writeln!(std::io::stderr().lock(), "WARN: {warning}");
        }
        #[cfg(unix)]
        let _interrupt = IgnoreInterrupt::install();
        std::process::Command::new(&sudo)
            .arg(&program.path)
            .args(["--unix-elevated"])
            .arg(&offer.socket)
            .arg(&offer.ticket)
            .status()
            .map_err(Into::into)
    })();
    // Always restore the TUI, including when sudo or the terminal setup fails.
    let restored = (|| -> Result<()> {
        screen.resume()?;
        terminal.backend_mut().writer_mut().invalidate();
        Ok(())
    })();
    if let Err(error) = restored {
        ui.message = format!("{error:#}");
        return;
    }
    match launched {
        Ok(status) if status.success() => {
            ui.message =
                t("Administrator permission granted; waiting for the elevated core").into();
            jobs.start(
                ui,
                Action::WaitElevation {
                    name,
                    ticket: offer.ticket,
                },
                None,
            );
        }
        Ok(_) => {
            let _ = call(
                socket,
                Action::CancelElevation {
                    name: name.clone(),
                    ticket: offer.ticket,
                },
            )
            .await;
            ui.message = t("Administrator permission was not granted").into();
        }
        Err(error) => {
            let _ = call(
                socket,
                Action::CancelElevation {
                    name,
                    ticket: offer.ticket,
                },
            )
            .await;
            ui.message = format!("{error:#}");
        }
    }
}

#[cfg(not(unix))]
async fn elevate(
    ui: &mut Ui,
    _jobs: &mut background::Jobs,
    _socket: &std::path::Path,
    _name: String,
    _expected_pid: Option<u32>,
    _terminal: &mut astra_tui_kit::Screen,
    _screen: &mut TerminalSession,
) {
    ui.message = t("TUN elevation is only available on Linux").into();
}

#[cfg(test)]
#[path = "test/ui_test.rs"]
mod tests;
