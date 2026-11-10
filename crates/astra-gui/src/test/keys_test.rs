use super::*;
use gpui::TestAppContext;
#[gpui::test]
fn every_shared_spec_shortcut_has_a_gpui_action(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let keys = bindings(cx);
        let count: usize = SPECS
            .iter()
            .map(|spec| spec.key.split(" / ").filter(|key| !key.is_empty()).count())
            .sum();
        assert_eq!(keys.len(), count + 13 + PLATFORM_CHORDS.len()); // `7` is supplied by Settings.
        for (index, binding) in keys.iter().enumerate() {
            assert!(keys[..index]
                .iter()
                .all(|other| other.keystrokes() != binding.keystrokes()));
        }
        for spec in SPECS.iter().filter(|spec| !spec.key.is_empty()) {
            assert!(
                keys.iter().any(|binding| binding
                    .action()
                    .as_any()
                    .downcast_ref::<Command>()
                    .is_some_and(|command| command.0 == spec.id)),
                "{}",
                spec.label
            );
        }
    });
}

#[gpui::test]
fn bindings_respect_native_input_and_editor_contexts(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let keys = bindings(cx);
        let shell = gpui::KeyContext::parse("Astra").unwrap();
        let editor = gpui::KeyContext::parse("AstraEditor").unwrap();
        let input = gpui::KeyContext::parse("Input").unwrap();
        for binding in &keys {
            let predicate = binding.predicate().unwrap();
            if let Some(command) = binding.action().as_any().downcast_ref::<Command>() {
                let spec = SPECS.iter().find(|spec| spec.id == command.0).unwrap();
                if spec.scope == Scope::Editor {
                    assert!(!predicate.eval(&[shell.clone(), input.clone()]));
                    assert!(!predicate.eval(&[editor.clone(), input.clone()]));
                    assert!(!predicate.eval(std::slice::from_ref(&shell)));
                    assert!(predicate.eval(std::slice::from_ref(&editor)));
                }
            }
        }
        for chord in ["q", "p", "/", "space", "1", "2", "3", "4", "5", "6", "7"] {
            let stroke = Keystroke::parse(chord).unwrap();
            let matches: Vec<_> = keys
                .iter()
                .filter(|binding| {
                    binding.match_keystrokes(std::slice::from_ref(&stroke)) == Some(false)
                })
                .collect();
            assert_eq!(matches.len(), 1, "{chord} must have exactly one binding");
            let predicate = matches[0].predicate().unwrap();
            assert!(
                predicate
                    .depth_of(&[shell.clone(), gpui::KeyContext::parse("AstraLive").unwrap()])
                    .is_some(),
                "{chord}"
            );
            assert!(!predicate.eval(std::slice::from_ref(&editor)));
            assert!(!predicate.eval(&[shell.clone(), input.clone()]));
            assert!(!predicate.eval(&[editor.clone(), input.clone()]));
            if chord.len() == 1 && chord.as_bytes()[0].is_ascii_digit() {
                let logs = gpui::KeyContext::parse("AstraLogs").unwrap();
                let picker = gpui::KeyContext::parse("AstraLogs AstraLogPicker").unwrap();
                assert!(predicate.depth_of(&[shell.clone(), logs.clone()]).is_some());
                assert!(predicate
                    .depth_of(&[shell.clone(), logs, input.clone()])
                    .is_none());
                assert!(predicate.depth_of(&[shell.clone(), picker]).is_none());
            }
        }
    });
}
#[test]
fn editor_keystrokes_preserve_vim_case_modifiers_and_repeat() {
    let stroke = Keystroke::parse("ctrl-s").unwrap();
    let key = from_keystroke(&stroke, false).unwrap();
    assert_eq!(key.code, KeyCode::Char('s'));
    assert!(key.modifiers.contains(KeyModifiers::CONTROL));
    let stroke = Keystroke::parse("shift-j").unwrap();
    let key = from_keystroke(&stroke, true).unwrap();
    assert_eq!(key.code, KeyCode::Char('J'));
    assert_eq!(key.kind, KeyEventKind::Repeat);
}
