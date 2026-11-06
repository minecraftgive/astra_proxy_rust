//! Shared global command descriptions, dispatch, palette and help bindings.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Id {
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
pub enum Scope {
    Global,
    Editor,
    Live,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Repeat {
    Once,
    Navigation,
}
pub struct Spec {
    pub id: Id,
    pub label: &'static str,
    pub key: &'static str,
    pub scope: Scope,
    pub repeat: Repeat,
}
pub const SPECS: &[Spec] = &[
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

use crate::{
    i18n::t,
    input::{KeyCode, KeyModifiers},
};
pub fn shortcut(key: crate::input::Key) -> Option<Id> {
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
pub fn repeatable(key: crate::input::Key) -> bool {
    shortcut(key).is_some_and(|id| {
        SPECS
            .iter()
            .any(|spec| spec.id == id && spec.repeat == Repeat::Navigation)
    })
}
pub fn help() -> String {
    let mut text = String::from(t("Tab / Shift-Tab  Focus region\n↑↓ / Home / End / PgUp / PgDn  Navigate\nEnter  Activate focused item\nEsc  Back; configuration drafts are retained\n1–7  Pages (outside text input)\nCtrl-Z / Ctrl-Y  Undo / redo draft\nInsert / Delete  Add / remove selected item\n\n"));
    for spec in SPECS.iter().filter(|s| !s.key.is_empty()) {
        text.push_str(&format!("{}  {}\n", spec.key, t(spec.label)));
    }
    text.push_str(t("\nAll other actions are in the command palette. Q, R and 0 no longer execute destructive actions.\n"));
    text
}

impl Spec {
    pub fn chords(&self) -> Vec<crate::input::Key> {
        self.key
            .split(" / ")
            .filter_map(|text| {
                let mut modifiers = KeyModifiers::NONE;
                let mut text = text;
                loop {
                    if let Some(rest) = text.strip_prefix("Ctrl-") {
                        modifiers.insert(KeyModifiers::CONTROL);
                        text = rest;
                    } else if let Some(rest) = text.strip_prefix("Shift-") {
                        modifiers.insert(KeyModifiers::SHIFT);
                        text = rest;
                    } else if let Some(rest) = text.strip_prefix("Alt-") {
                        modifiers.insert(KeyModifiers::ALT);
                        text = rest;
                    } else {
                        break;
                    }
                }
                let code = if let Some(number) =
                    text.strip_prefix('F').and_then(|n| n.parse::<u8>().ok())
                {
                    KeyCode::F(number)
                } else if text == "Space" {
                    KeyCode::Char(' ')
                } else {
                    let mut chars = text.chars();
                    let first = chars.next()?;
                    if chars.next().is_some() {
                        return None;
                    }
                    KeyCode::Char(first.to_ascii_lowercase())
                };
                Some(crate::input::Key::new(code, modifiers))
            })
            .collect()
    }
}
