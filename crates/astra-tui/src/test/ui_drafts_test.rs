use super::*;
#[test]
fn old_session_migrates_once_and_dismissal_persists_atomically() {
    let fixture = crate::import::tests::Fixture::new();
    let path = fixture.0.join("ui-session.json");
    let mut value = serde_json::to_value(Session::default()).unwrap();
    value.as_object_mut().unwrap().remove("session_version");
    value.as_object_mut().unwrap().remove("keymap_version");
    value.as_object_mut().unwrap().remove("drafts");
    value["editor"] =
        serde_json::to_value(Editor::new("legacy".into(), "standalone=true\n".into())).unwrap();
    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    let original = std::fs::read(&path).unwrap();
    let session = Session::load(&path).unwrap();
    assert_eq!(
        std::fs::read(path.with_extension("pre-v2.json")).unwrap(),
        original
    );
    assert_eq!(session.session_version, 1);
    assert!(session.keymap_notice_pending);
    assert_eq!(session.drafts.len(), 1);
    let mut ui = Ui::new(session);
    assert!(ui.help_scroll.is_some());
    ui.key(event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(!ui.keymap_notice_pending);
    ui.session.save(&path).unwrap();
    assert!(!Session::load(&path).unwrap().keymap_notice_pending);
    assert_eq!(
        std::fs::read(path.with_extension("pre-v2.json")).unwrap(),
        original
    );
}
#[test]
fn future_or_unknown_session_state_is_rejected_without_overwrite() {
    let fixture = crate::import::tests::Fixture::new();
    let path = fixture.0.join("ui-session.json");
    for (key, value) in [
        ("session_version", serde_json::json!(999)),
        ("unknown_field", serde_json::json!({"draft":"retain"})),
    ] {
        let mut session = serde_json::to_value(Session::default()).unwrap();
        session[key] = value;
        let original = serde_json::to_vec(&session).unwrap();
        std::fs::write(&path, &original).unwrap();
        assert!(Session::load(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }
}
#[test]
fn two_drafts_survive_navigation_and_external_conflicts() {
    let mut ui = Ui::new(Session::default());
    ui.open_draft("a".into(), "standalone=true\n".into());
    ui.editor
        .as_mut()
        .unwrap()
        .import("standalone=false\n".into())
        .unwrap();
    ui.open_draft("b".into(), "standalone=true\n".into());
    ui.editor
        .as_mut()
        .unwrap()
        .import("standalone=false\n# b\n".into())
        .unwrap();
    ui.open_draft("a".into(), "standalone=true\n# external\n".into());
    assert_eq!(ui.editor.as_ref().unwrap().text(), "standalone=false\n");
    assert_eq!(ui.editor.as_ref().unwrap().original, "standalone=true\n");
    assert!(ui.drafts.values().any(|d| d.conflict));
    ui.remember_draft();
    let restored: Session =
        serde_json::from_str(&serde_json::to_string(&ui.session).unwrap()).unwrap();
    assert_eq!(restored.drafts.len(), 2);
}
