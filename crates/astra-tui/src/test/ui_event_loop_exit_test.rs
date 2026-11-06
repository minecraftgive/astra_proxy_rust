use super::*;

#[test]
fn replacement_rejects_unreadable_session_without_overwriting_the_draft() {
    let id = crate::protocol::updates::UpdateRequest::new(
        crate::protocol::updates::UpdateIntent::Activate {
            release_id: "next".into(),
        },
    )
    .operation_id;
    let path = std::env::temp_dir().join(format!("astra-restart-invalid-session-{id}.json"));
    assert!(restore_session(&path, true).is_err());
    let invalid = b"{\"session_version\":999,\"drafts\":{\"important\":\"retained\"}}";
    std::fs::write(&path, invalid).unwrap();
    assert!(restore_session(&path, true).is_err());
    assert!(restore_session(&path, false).unwrap().recovery.is_some());
    assert_eq!(std::fs::read(&path).unwrap(), invalid);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn restart_waits_for_recovery_and_never_treats_paused_persistence_as_saved() {
    let mut ui = Ui::new(Session::default());
    assert!(ui.restart_ready());
    ui.persistence_paused = true;
    assert!(!ui.restart_ready());
    ui.persistence_paused = false;
    ui.fail("disk full", Resume::Session);
    assert!(!ui.restart_ready());
    ui.recovery = None;
    ui.prompt = Some(Prompt::UpdateConfirm(
        crate::protocol::updates::UpdateRequest::new(
            crate::protocol::updates::UpdateIntent::Activate {
                release_id: "next".into(),
            },
        ),
    ));
    assert!(!ui.restart_ready());
    ui.prompt = None;
    assert!(ui.restart_ready());
}

#[test]
fn restart_checkpoint_retains_draft_input_page_and_window_identity() {
    let mut ui = Ui::new(Session::default());
    ui.page = shell::PROFILES;
    ui.kind = Kind::Rule;
    ui.editor = Some(Editor::new("rules".into(), "# unsaved\n".into()).with_kind(Kind::Rule));
    ui.prompt = Some(Prompt::Import);
    ui.input = "unfinished package path".into();
    ui.input_cursor = 8;
    let id = crate::protocol::updates::UpdateRequest::new(
        crate::protocol::updates::UpdateIntent::Activate {
            release_id: "next".into(),
        },
    )
    .operation_id;
    let path = std::env::temp_dir().join(format!("astra-restart-session-{id}.json"));
    assert!(ui.checkpoint(&path));
    let restored = Session::load(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert_eq!(restored.editor.as_ref().unwrap().text(), "# unsaved\n");
    assert_eq!(restored.input, "unfinished package path");
    assert_eq!(restored.input_cursor, 8);
    assert_eq!(restored.drafts.len(), 1);
    for window in [None, Some("existing-window".into())] {
        let launch = ui.restart_launch(window.clone());
        assert_eq!(launch.view, Some(crate::windows::View::Profiles));
        assert_eq!(launch.kind, Some(Kind::Rule));
        assert_eq!(launch.window_id, window);
    }
}

#[test]
fn error_exit_attempts_both_cleanup_steps_and_preserves_original_failure() {
    let calls = std::cell::RefCell::new(Vec::new());
    let result = finish_session(
        Err(anyhow::anyhow!("draw failed")),
        || {
            calls.borrow_mut().push("checkpoint");
            anyhow::bail!("disk full")
        },
        || {
            calls.borrow_mut().push("restore");
            anyhow::bail!("terminal closed")
        },
    );
    assert_eq!(*calls.borrow(), ["checkpoint", "restore"]);
    let text = result.unwrap_err().to_string();
    assert!(
        text.contains("draw failed")
            && text.contains("disk full")
            && text.contains("terminal closed")
    );
}
#[test]
fn clean_exit_checkpoints_before_restoring_terminal() {
    let calls = std::cell::RefCell::new(Vec::new());
    finish_session(
        Ok(()),
        || {
            calls.borrow_mut().push("checkpoint");
            Ok(())
        },
        || {
            calls.borrow_mut().push("restore");
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(*calls.borrow(), ["checkpoint", "restore"]);
}
