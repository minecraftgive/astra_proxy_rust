use super::*;
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
