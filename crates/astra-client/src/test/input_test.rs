use super::*;

fn key(input: &mut TextInput, code: KeyCode, modifiers: KeyModifiers) {
    input.key(KeyEvent::new(code, modifiers));
}
#[test]
fn combining_marks_and_joined_emoji_are_single_editing_units() {
    let mut input = TextInput::new("e\u{301}👩‍💻国".into());
    key(&mut input, KeyCode::Left, KeyModifiers::NONE);
    assert_eq!(input.cursor, 5);
    key(&mut input, KeyCode::Left, KeyModifiers::NONE);
    assert_eq!(input.cursor, 2);
    key(&mut input, KeyCode::Right, KeyModifiers::NONE);
    key(&mut input, KeyCode::Backspace, KeyModifiers::NONE);
    assert_eq!((input.text.as_str(), input.cursor), ("e\u{301}国", 2));
    key(&mut input, KeyCode::Home, KeyModifiers::NONE);
    key(&mut input, KeyCode::Delete, KeyModifiers::NONE);
    assert_eq!(input.text, "国");
    key(&mut input, KeyCode::Char('z'), KeyModifiers::CONTROL);
    assert_eq!(input.text, "e\u{301}国");
    input.cursor = 1; // Old drafts could leave a cursor inside a grapheme.
    key(&mut input, KeyCode::Right, KeyModifiers::NONE);
    assert_eq!(input.cursor, 2);
}

#[test]
fn unicode_edits_undo_and_redo_preserve_character_cursor() {
    let mut input = TextInput::new("节点🙂".into());
    key(&mut input, KeyCode::Left, KeyModifiers::NONE);
    input.insert("é");
    assert_eq!(
        (input.text.as_str(), input.cursor, input.byte()),
        ("节点é🙂", 3, 8)
    );
    key(&mut input, KeyCode::Backspace, KeyModifiers::NONE);
    assert_eq!(input.text, "节点🙂");
    key(&mut input, KeyCode::Char('z'), KeyModifiers::CONTROL);
    assert_eq!(input.text, "节点é🙂");
    key(&mut input, KeyCode::Char('y'), KeyModifiers::CONTROL);
    assert_eq!(input.text, "节点🙂");
    key(&mut input, KeyCode::Delete, KeyModifiers::NONE);
    assert_eq!(input.text, "节点");
}

#[test]
fn serialized_draft_retains_history_and_accepts_legacy_fields() {
    let mut legacy: TextInput =
        serde_json::from_str(r#"{"text":"old","cursor":3,"replace":true}"#).unwrap();
    legacy.insert("新\n值\t");
    assert_eq!(legacy.text, "新值");
    let encoded = serde_json::to_string(&legacy).unwrap();
    let mut restored: TextInput = serde_json::from_str(&encoded).unwrap();
    key(&mut restored, KeyCode::Char('z'), KeyModifiers::CONTROL);
    assert_eq!((restored.text.as_str(), restored.cursor), ("old", 3));
}
