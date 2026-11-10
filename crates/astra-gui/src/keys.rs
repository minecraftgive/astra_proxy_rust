use astra_client::{
    commands::{Id, Scope, SPECS},
    input::{Key, KeyCode, KeyEventKind, KeyModifiers},
};
use gpui::{Action, KeyBinding, Keystroke};

#[derive(Clone, PartialEq, Action)]
#[action(namespace = astra, no_json)]
pub struct Command(pub Id);
#[derive(Clone, PartialEq, Action)]
#[action(namespace = astra, no_json)]
pub struct Page {
    pub index: usize,
    pub new_window: bool,
}

#[cfg(target_os = "macos")]
pub const PLATFORM_CHORDS: &[(&str, Id)] = &[("cmd-k", Id::Palette)];
#[cfg(not(target_os = "macos"))]
pub const PLATFORM_CHORDS: &[(&str, Id)] = &[];

pub fn bindings(cx: &gpui::App) -> Vec<KeyBinding> {
    let context = |source| {
        Some(std::rc::Rc::new(
            gpui::KeyBindingContextPredicate::parse(source).expect("static key context"),
        ))
    };
    // Native inputs own text and editing shortcuts. App navigation (for example
    // F1 and Ctrl-K) remains available. The shared editor also owns printable keys.
    let shell =
        context("Astra && !Input && !AstraDialog && !AstraEditor && !AstraLogs && !AstraPalette");
    let pages = context(
        "Astra && !Input && !AstraDialog && !AstraEditor && !AstraLogPicker && !AstraPalette",
    );
    let live = context("AstraLive && !Input && !AstraDialog");
    let editor = context("AstraEditor && !Input && !AstraDialog");
    let application = context("(Astra || AstraEditor) && !AstraDialog");
    let outside_nettools = context("(Astra || AstraEditor) && !AstraDialog && !AstraNetTools");
    let non_input = context("(Astra || AstraEditor) && !Input && !AstraDialog");
    let mut bindings = Vec::new();
    let mut command_chords = std::collections::HashSet::new();
    for spec in SPECS {
        for chord in spec.key.split(" / ").filter(|key| !key.is_empty()) {
            let chord = chord.to_ascii_lowercase();
            let nettools_reserved = chord.split('-').next_back().is_some_and(|key| {
                key.strip_prefix('f')
                    .and_then(|n| n.parse::<u8>().ok())
                    .is_some_and(|n| (1..=12).contains(&n))
            }) || chord == "ctrl-p";
            let context = match spec.scope {
                Scope::Global if nettools_reserved => &outside_nettools,
                Scope::Editor => &editor,
                Scope::Live => &live,
                Scope::Global if spec.id == Id::Settings => &pages,
                Scope::Global if chord.chars().count() == 1 || chord == "space" => &shell,
                Scope::Global if matches!(spec.id, Id::Quit | Id::Search) => &non_input,
                Scope::Global => &application,
            };
            bindings.push(
                KeyBinding::load(
                    &chord,
                    Box::new(Command(spec.id)),
                    context.clone(),
                    false,
                    None,
                    cx,
                )
                .expect("shared command chord"),
            );
            command_chords.insert(chord);
        }
    }
    for index in 0..7 {
        for new_window in [false, true] {
            let chord = format!("{}{}", if new_window { "alt-" } else { "" }, index + 1);
            // Settings already supplies `7` through the shared command table.
            if command_chords.contains(&chord) {
                continue;
            }
            bindings.push(
                KeyBinding::load(
                    &chord,
                    Box::new(Page { index, new_window }),
                    if new_window {
                        application.clone()
                    } else {
                        pages.clone()
                    },
                    false,
                    None,
                    cx,
                )
                .expect("page chord"),
            );
        }
    }
    // Spotlight's chord on macOS, alongside the shared Ctrl-K / F2; it works from
    // any screen, including inside text fields and the editor.
    for (chord, id) in PLATFORM_CHORDS {
        bindings.push(
            KeyBinding::load(
                chord,
                Box::new(Command(*id)),
                application.clone(),
                false,
                None,
                cx,
            )
            .expect("platform chord"),
        );
    }
    bindings
}

pub fn from_keystroke(stroke: &Keystroke, held: bool) -> Option<Key> {
    let mut modifiers = KeyModifiers::NONE;
    if stroke.modifiers.control {
        modifiers |= KeyModifiers::CONTROL;
    }
    if stroke.modifiers.alt {
        modifiers |= KeyModifiers::ALT;
    }
    if stroke.modifiers.shift {
        modifiers |= KeyModifiers::SHIFT;
    }
    if stroke.modifiers.platform {
        modifiers |= KeyModifiers::SUPER;
    }
    let code = match stroke.key.as_str() {
        "enter" => KeyCode::Enter,
        "escape" => KeyCode::Esc,
        "backspace" => KeyCode::Backspace,
        "tab" if stroke.modifiers.shift => KeyCode::BackTab,
        "tab" => KeyCode::Tab,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        "delete" => KeyCode::Delete,
        "insert" => KeyCode::Insert,
        "space" => KeyCode::Char(' '),
        key if key.starts_with('f') && key[1..].parse::<u8>().is_ok() => {
            KeyCode::F(key[1..].parse().ok()?)
        }
        key => {
            let mut chars = key.chars();
            let first = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            KeyCode::Char(if stroke.modifiers.shift {
                first.to_ascii_uppercase()
            } else {
                first
            })
        }
    };
    Some(Key::new_with_kind(
        code,
        modifiers,
        if held {
            KeyEventKind::Repeat
        } else {
            KeyEventKind::Press
        },
    ))
}

#[cfg(test)]
#[path = "test/keys_test.rs"]
mod tests;
