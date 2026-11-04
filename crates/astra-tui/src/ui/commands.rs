//! Shared global command descriptions, dispatch, palette and help bindings.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Id {
    Palette,
    Help,
    Profiles,
    CopyTerminal,
    Settings,
    Search,
    NextMatch,
    PreviousMatch,
    Save,
    Tasks,
    Logs,
    Monitor,
    Window,
    Quit,
    Shutdown,
    Import,
    ExternalEditor,
    ReadExternal,
    Discard,
    CompareDraft,
    Undo,
    Redo,
    Pause,
    VimKeys,
    MotionFull,
    MotionReduced,
    MotionOff,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Scope {
    Global,
    Editor,
    Live,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Repeat {
    Once,
    Navigation,
}
pub(super) struct Spec {
    pub id: Id,
    pub label: &'static str,
    pub key: &'static str,
    pub scope: Scope,
    pub repeat: Repeat,
}
pub(super) const SPECS: &[Spec] = &[
    Spec {
        id: Id::Palette,
        label: "Command palette",
        key: "Ctrl-K / F2",
        scope: Scope::Global,
        repeat: Repeat::Once,
    },
    Spec {
        id: Id::Help,
        label: "Keyboard help",
        key: "F1",
        scope: Scope::Global,
        repeat: Repeat::Once,
    },
    Spec {
        id: Id::Profiles,
        label: "Switch current profile",
        key: "Ctrl-P / p",
        scope: Scope::Global,
        repeat: Repeat::Once,
    },
    Spec {
        id: Id::CopyTerminal,
        label: "Copy terminal proxy commands to clipboard",
        key: "F4",
        scope: Scope::Global,
        repeat: Repeat::Once,
    },
    Spec {
        id: Id::Search,
        label: "Find in current view",
        key: "Ctrl-F / /",
        scope: Scope::Global,
        repeat: Repeat::Once,
    },
    Spec {
        id: Id::NextMatch,
        label: "Next match",
        key: "F3",
        scope: Scope::Global,
        repeat: Repeat::Navigation,
    },
    Spec {
        id: Id::PreviousMatch,
        label: "Previous match",
        key: "Shift-F3",
        scope: Scope::Global,
        repeat: Repeat::Navigation,
    },
    Spec {
        id: Id::Save,
        label: "Save and apply draft",
        key: "Ctrl-S",
        scope: Scope::Editor,
        repeat: Repeat::Once,
    },
    Spec {
        id: Id::Tasks,
        label: "Task details",
        key: "F6",
        scope: Scope::Global,
        repeat: Repeat::Once,
    },
    Spec {
        id: Id::Logs,
        label: "TUI log",
        key: "F7",
        scope: Scope::Global,
        repeat: Repeat::Once,
    },
    Spec {
        id: Id::Window,
        label: "Open page in new window",
        key: "F8",
        scope: Scope::Global,
        repeat: Repeat::Once,
    },
    Spec {
        id: Id::Monitor,
        label: "Performance / memory / network monitor",
        key: "F9",
        scope: Scope::Global,
        repeat: Repeat::Once,
    },
    Spec {
        id: Id::Quit,
        label: "Leave interface (cores keep running)",
        key: "Ctrl-C / q",
        scope: Scope::Global,
        repeat: Repeat::Once,
    },
    Spec {
        id: Id::Shutdown,
        label: "Stop all instances and manager",
        key: "",
        scope: Scope::Global,
        repeat: Repeat::Once,
    },
    Spec {
        id: Id::Import,
        label: "Import into draft",
        key: "",
        scope: Scope::Editor,
        repeat: Repeat::Once,
    },
    Spec {
        id: Id::ExternalEditor,
        label: "Open external editor",
        key: "",
        scope: Scope::Editor,
        repeat: Repeat::Once,
    },
    Spec {
        id: Id::ReadExternal,
        label: "Read external draft",
        key: "",
        scope: Scope::Editor,
        repeat: Repeat::Once,
    },
    Spec {
        id: Id::Discard,
        label: "Discard current draft",
        key: "",
        scope: Scope::Editor,
        repeat: Repeat::Once,
    },
    Spec {
        id: Id::CompareDraft,
        label: "Compare draft with observed disk",
        key: "",
        scope: Scope::Editor,
        repeat: Repeat::Once,
    },
    Spec {
        id: Id::Undo,
        label: "Undo draft edit",
        key: "Ctrl-Z",
        scope: Scope::Editor,
        repeat: Repeat::Once,
    },
    Spec {
        id: Id::Redo,
        label: "Redo draft edit",
        key: "Ctrl-Y",
        scope: Scope::Editor,
        repeat: Repeat::Once,
    },
    Spec {
        id: Id::Pause,
        label: "Pause / resume live display",
        key: "Space",
        scope: Scope::Live,
        repeat: Repeat::Once,
    },
    Spec {
        id: Id::Settings,
        label: "Open settings",
        key: "7",
        scope: Scope::Global,
        repeat: Repeat::Once,
    },
    Spec {
        id: Id::VimKeys,
        label: "Toggle optional Vim navigation aliases",
        key: "",
        scope: Scope::Global,
        repeat: Repeat::Once,
    },
    Spec {
        id: Id::MotionFull,
        label: "Motion: normal",
        key: "",
        scope: Scope::Global,
        repeat: Repeat::Once,
    },
    Spec {
        id: Id::MotionReduced,
        label: "Motion: reduced",
        key: "",
        scope: Scope::Global,
        repeat: Repeat::Once,
    },
    Spec {
        id: Id::MotionOff,
        label: "Motion: off",
        key: "",
        scope: Scope::Global,
        repeat: Repeat::Once,
    },
];
fn shortcut(key: event::KeyEvent) -> Option<Id> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::F(1) => Some(Id::Help),
        KeyCode::F(2) => Some(Id::Palette),
        KeyCode::F(4) if key.modifiers.is_empty() => Some(Id::CopyTerminal),
        KeyCode::F(6) => Some(Id::Tasks),
        KeyCode::F(7) => Some(Id::Logs),
        KeyCode::F(9) => Some(Id::Monitor),
        KeyCode::Char('k') if ctrl => Some(Id::Palette),
        KeyCode::Char('p') if ctrl => Some(Id::Profiles),
        KeyCode::Char('f') if ctrl => Some(Id::Search),
        KeyCode::Char('s') if ctrl => Some(Id::Save),
        KeyCode::F(3) => Some(if key.modifiers.contains(KeyModifiers::SHIFT) {
            Id::PreviousMatch
        } else {
            Id::NextMatch
        }),
        _ => None,
    }
}
pub(super) fn repeatable(key: event::KeyEvent) -> bool {
    shortcut(key).is_some_and(|id| {
        SPECS
            .iter()
            .any(|spec| spec.id == id && spec.repeat == Repeat::Navigation)
    })
}
pub(super) fn help() -> String {
    let mut text = String::from(t("Tab / Shift-Tab  Focus region\n↑↓ / Home / End / PgUp / PgDn  Navigate\nEnter  Activate focused item\nEsc  Back; configuration drafts are retained\n1–7  Pages (outside text input)\nCtrl-Z / Ctrl-Y  Undo / redo draft\nInsert / Delete  Add / remove selected item\n\n"));
    for spec in SPECS.iter().filter(|s| !s.key.is_empty()) {
        text.push_str(&format!("{}  {}\n", spec.key, t(spec.label)));
    }
    text.push_str(t("\nAll other actions are in the command palette. Q, R and 0 no longer execute destructive actions.\n"));
    text
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
        if self.tasks.showing_details() || self.help_scroll.is_some() || self.recovery.is_some() {
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
    pub(super) fn command_available(&self, id: Id) -> Option<&'static str> {
        let scope = SPECS
            .iter()
            .find(|spec| spec.id == id)
            .map_or(Scope::Global, |spec| spec.scope);
        if scope == Scope::Editor && self.editor.is_none() {
            return Some("Open a configuration first");
        }
        if scope == Scope::Live
            && !matches!(
                self.page,
                shell::OVERVIEW | shell::CONNECTIONS | shell::LOGS
            )
        {
            return Some("Open connections or logs first");
        }
        match id {
            Id::CopyTerminal if self.prompt.is_some() => {
                Some("Finish or cancel the current prompt first")
            }
            Id::CopyTerminal if self.terminal_export_profile().is_none() => Some("No profile yet"),
            Id::ReadExternal | Id::Import
                if self
                    .editor
                    .as_ref()
                    .is_some_and(|editor| !editor.browsing()) =>
            {
                Some("Confirm or cancel the field before importing changes")
            }
            Id::Save
                if self
                    .update_status
                    .as_ref()
                    .and_then(|status| status.operation.as_ref())
                    .is_some_and(|operation| !operation.phase.terminal()) =>
            {
                Some("Update running or requires recovery; draft retained, retry save afterward")
            }
            Id::Profiles if self.prompt.is_some() => {
                Some("Finish or cancel the current prompt first")
            }
            _ => None,
        }
    }
    pub(super) fn run_command(&mut self, id: Id) -> Input {
        if let Some(reason) = self.command_available(id) {
            self.message = t(reason).into();
            return Input::Idle;
        }
        let plain = |code| event::KeyEvent::new(code, KeyModifiers::NONE);
        match id {
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
                    let _ = editor.key(code);
                } else if let Some(viewer) = &mut self.viewer {
                    viewer.key(event::KeyEvent::new(
                        KeyCode::Char('f'),
                        KeyModifiers::CONTROL,
                    ));
                } else if matches!(self.page, shell::OVERVIEW | shell::CONNECTIONS) {
                    self.page = shell::CONNECTIONS;
                    self.connection_filtering = true;
                } else if self.page == shell::RULES {
                    return self.rule_search();
                } else {
                    self.prompt(Prompt::Find(self.page));
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
                    viewer.key(event::KeyEvent::new(
                        KeyCode::F(3),
                        if backwards {
                            KeyModifiers::SHIFT
                        } else {
                            KeyModifiers::NONE
                        },
                    ));
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
            Id::Save => {
                let editor = self.editor.as_mut().unwrap();
                match editor.prepare_save() {
                    Ok(text) => Input::Run(
                        Action::Save {
                            name: editor.name.clone(),
                            original: editor.original.clone(),
                            text,
                        },
                        None,
                    ),
                    Err(error) => {
                        self.fail(format!("{error:#}"), Resume::Editor);
                        Input::Idle
                    }
                }
            }
            Id::Tasks => {
                let was_open = self.tasks.showing_details();
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
            Id::Quit => {
                self.remember_draft();
                Input::Quit
            }
            Id::Shutdown => {
                self.prompt(Prompt::Shutdown);
                Input::Idle
            }
            Id::Import => {
                self.prompt(Prompt::Import);
                Input::Idle
            }
            Id::ExternalEditor => Input::ExternalEditor,
            Id::ReadExternal => {
                if let Err(error) = self.read_external() {
                    self.fail(format!("{error:#}"), Resume::Editor);
                }
                Input::Idle
            }
            Id::Discard => {
                self.prompt(Prompt::Discard);
                Input::Idle
            }
            Id::CompareDraft => {
                self.compare_draft();
                Input::Idle
            }
            Id::Undo | Id::Redo => {
                let key = event::KeyEvent::new(
                    KeyCode::Char(if id == Id::Undo { 'z' } else { 'y' }),
                    KeyModifiers::CONTROL,
                );
                if let Some(editor) = &mut self.editor {
                    if let Err(error) = editor.key(key) {
                        self.fail(format!("{error:#}"), Resume::Editor);
                    }
                }
                Input::Idle
            }
            Id::Pause => {
                if let Some(viewer) = &mut self.viewer {
                    viewer.key(plain(KeyCode::Char(' ')));
                    Input::Idle
                } else {
                    self.toggle_live_pause()
                }
            }
            Id::VimKeys => {
                self.change_preferences(|p| p.vim_keys = !p.vim_keys);
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
        if self.tasks.showing_details() {
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
            || self.tasks.showing_details()
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
mod tests {
    use super::*;
    fn press(ui: &mut Ui, code: KeyCode) -> Input {
        ui.key(event::KeyEvent::new(code, KeyModifiers::NONE))
    }
    fn rendered(ui: &Ui) -> String {
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| ui.render(frame)).unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }
    #[test]
    fn global_overlay_keys_replace_visible_layers_and_preserve_underlying_input() {
        crate::i18n::with_language(crate::i18n::Language::English, || {
            let mut ui = Ui::new(Session::default());
            ui.editor = Some(Editor::new("draft".into(), "standalone=true\n".into()));
            ui.prompt(Prompt::AddName);
            ui.paste_prompt("pending name");
            let editor = serde_json::to_value(ui.editor.as_ref().unwrap()).unwrap();
            press(&mut ui, KeyCode::F(2));
            assert!(rendered(&ui).contains("Commands"));
            press(&mut ui, KeyCode::F(6));
            assert!(ui.palette.is_none());
            assert!(rendered(&ui).contains("Task details · F6"));
            press(&mut ui, KeyCode::F(1));
            assert!(!ui.tasks.showing_details());
            assert!(rendered(&ui).contains("Page operations"));
            ui.key(event::KeyEvent::new(
                KeyCode::Char('p'),
                KeyModifiers::CONTROL,
            ));
            assert!(ui.help_scroll.is_some());
            assert!(ui.palette.is_none()); // A pending prompt prevents changing configuration.
            press(&mut ui, KeyCode::F(2));
            assert!(ui.help_scroll.is_none());
            assert!(rendered(&ui).contains("Commands"));
            for ch in ['s', 'z', 'y'] {
                assert!(matches!(
                    ui.key(event::KeyEvent::new(
                        KeyCode::Char(ch),
                        KeyModifiers::CONTROL
                    )),
                    Input::Idle
                ));
            }
            assert!(
                matches!(press(&mut ui,KeyCode::F(7)),Input::Run(Action::Logs(name),_) if name=="@tui")
            );
            assert!(ui.palette.is_none());
            assert!(ui.help_scroll.is_none());
            // Jobs installs this placeholder before the next frame while its subscription opens.
            ui.tui_viewer = Some(logs::Viewer::new("@tui".into(), Default::default()));
            assert!(rendered(&ui).contains("@tui"));
            press(&mut ui, KeyCode::F(1));
            assert!(ui.tui_viewer.is_none());
            assert!(rendered(&ui).contains("Page operations"));
            press(&mut ui, KeyCode::F(2));
            press(&mut ui, KeyCode::Esc);
            assert_eq!(ui.input, "pending name");
            assert!(matches!(ui.prompt, Some(Prompt::AddName)));
            assert_eq!(
                serde_json::to_value(ui.editor.as_ref().unwrap()).unwrap(),
                editor
            );
            ui.prompt = None;
            press(&mut ui, KeyCode::F(6));
            ui.key(event::KeyEvent::new(
                KeyCode::Char('p'),
                KeyModifiers::CONTROL,
            ));
            assert!(!ui.tasks.showing_details());
            assert!(rendered(&ui).contains("Switch profile"));
            ui.key(event::KeyEvent::new(
                KeyCode::Char('k'),
                KeyModifiers::CONTROL,
            ));
            assert!(ui.palette.is_none());
            assert_eq!(
                serde_json::to_value(ui.editor.as_ref().unwrap()).unwrap(),
                editor
            );
        });
    }
    #[test]
    fn ordinary_text_edits_at_unicode_cursor_and_palette_paste_stays_local() {
        let mut ui = Ui::new(Session::default());
        ui.prompt(Prompt::AddName);
        ui.paste_prompt("a界c");
        press(&mut ui, KeyCode::Left);
        press(&mut ui, KeyCode::Backspace);
        press(&mut ui, KeyCode::Char('b'));
        assert_eq!(ui.input, "abc");
        press(&mut ui, KeyCode::Home);
        press(&mut ui, KeyCode::Delete);
        assert_eq!(ui.input, "bc");
        press(&mut ui, KeyCode::Esc);
        ui.editor = Some(Editor::new("a".into(), "standalone=true\n".into()));
        press(&mut ui, KeyCode::F(2));
        ui.paste("Motion");
        assert_eq!(ui.palette.as_ref().unwrap().query, "Motion");
        assert!(ui.prompt.is_none());
        assert_eq!(ui.editor.as_ref().unwrap().text(), "standalone=true\n");
        press(&mut ui, KeyCode::Home);
        press(&mut ui, KeyCode::Delete);
        assert_eq!(ui.palette.as_ref().unwrap().query, "otion");
    }
    #[test]
    fn focus_navigation_and_quit_do_not_apply_or_lose_a_draft() {
        let mut ui = Ui::new(Session::default());
        ui.editor = Some(Editor::new("a".into(), "standalone=true\n".into()));
        ui.editor
            .as_mut()
            .unwrap()
            .import("standalone=false\n".into())
            .unwrap();
        press(&mut ui, KeyCode::Tab);
        assert_eq!(ui.focus.region, focus::Region::Controls);
        assert_eq!(ui.page, 0);
        assert!(matches!(press(&mut ui, KeyCode::Char('q')), Input::Quit));
        assert_eq!(
            ui.drafts.values().next().unwrap().editor.text(),
            "standalone=false\n"
        );
        let before = ui.editor.as_ref().unwrap().text();
        let mut key = event::KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        key.kind = KeyEventKind::Repeat;
        assert!(matches!(ui.key(key), Input::Idle));
        assert_eq!(ui.editor.as_ref().unwrap().text(), before);
    }
    #[test]
    fn connection_search_cycles_filtered_results_by_identity_and_wraps() {
        use astra_model::runtime::Connection;
        use astra_model::runtime::Snapshot;
        let mut ui = Ui::new(Session::default());
        ui.page = shell::CONNECTIONS;
        ui.rows.push(Row {
            instance: Instance {
                name: "proxy".into(),
                kind: Kind::Proxy,
                config: "proxy.toml".into(),
            },
            pid: Some(42),
            state: "Running".into(),
            uptime: 0,
            changed: false,
        });
        let make = |id, host: &str| Connection {
            id,
            host: host.into(),
            source: None,
            port: 443,
            network: "tcp".into(),
            inbound: "socks".into(),
            outbound: "DIRECT".into(),
            upload: 0,
            download: 0,
            elapsed_ms: 0,
        };
        ui.accept_runtime(
            "proxy".into(),
            Snapshot {
                pid: 42,
                connections: vec![
                    make(11, "match.one"),
                    make(22, "other"),
                    make(33, "match.two"),
                ],
                ..Default::default()
            },
        );
        ui.connection_filter = "match".into();
        ui.select_connection(0);
        press(&mut ui, KeyCode::F(3));
        assert!(ui.connection_is_selected(33));
        press(&mut ui, KeyCode::F(3));
        assert!(ui.connection_is_selected(11));
        ui.key(event::KeyEvent::new(KeyCode::F(3), KeyModifiers::SHIFT));
        assert!(ui.connection_is_selected(33));
        ui.connection_filter = "no matches".into();
        press(&mut ui, KeyCode::F(3));
        assert!(ui.visible_connections().is_empty());
    }
    #[test]
    fn repeat_space_does_not_toggle_member_picker_but_repeated_text_inserts() {
        let mut ui = Ui::new(Session::default());
        let mut value =
            serde_json::to_value(Editor::new("draft".into(), "standalone=true\n".into())).unwrap();
        value["form"]["mode"] = serde_json::json!({"Members":{"options":["DIRECT"],"chosen":[],"cursor":0,"find":null}});
        ui.editor = Some(serde_json::from_value(value).unwrap());
        let before = serde_json::to_value(ui.editor.as_ref().unwrap()).unwrap();
        let mut repeated = event::KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE);
        repeated.kind = KeyEventKind::Repeat;
        assert!(matches!(ui.key(repeated), Input::Idle));
        assert_eq!(
            serde_json::to_value(ui.editor.as_ref().unwrap()).unwrap(),
            before
        );
        ui.editor = None;
        ui.prompt(Prompt::AddName);
        repeated.code = KeyCode::Char('a');
        ui.key(repeated);
        ui.key(repeated);
        assert_eq!(ui.input, "aa");
    }
    #[test]
    fn command_registry_has_unique_ids_and_consistent_repeat_policy() {
        for (index, spec) in SPECS.iter().enumerate() {
            assert!(!SPECS[..index].iter().any(|other| other.id == spec.id));
        }
        assert!(repeatable(event::KeyEvent::new(
            KeyCode::F(3),
            KeyModifiers::NONE
        )));
        assert!(!repeatable(event::KeyEvent::new(
            KeyCode::Char('s'),
            KeyModifiers::CONTROL
        )));
    }
}
