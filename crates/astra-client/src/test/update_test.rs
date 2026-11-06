use super::*;
use crate::i18n::Language;
use std::path::PathBuf;
#[test]
fn transactions_require_capabilities_and_freeze_confirmed_target() {
    assert!(Command::Import.unavailable(None).is_some());
    let mut status = UpdateStatus::default();
    assert!(Command::Activate.request(&status).is_err());
    status.capabilities.activation_allowed = true;
    status.staged_release_id = Some("first".into());
    status.staged_compatible = Some(true);
    let request = Command::Activate.request(&status).unwrap();
    status.staged_release_id = Some("second".into());
    assert_eq!(
        request.intent,
        UpdateIntent::Activate {
            release_id: "first".into()
        }
    );
    assert!(Command::Rollback.request(&status).is_err());
    assert!(View::decode("{\"host_version\":\"legacy\"}").is_err());
    assert!(View::decode("{\"schema_version\":2}").is_err());
    assert!(View::decode(&serde_json::to_string(&status).unwrap()).is_ok());
}
fn completed_update(operation_id: &str) -> UpdateStatus {
    serde_json::from_value(serde_json::json!({
        "next_launch": {"release_id": "new-release", "previous_release_id": null},
        "operation": {
            "request": {
                "operation_id": operation_id,
                "intent": {"kind": "activate", "release_id": "new-release"}
            },
            "phase": "complete",
            "before_directory": "bundle",
            "target_directory": "releases/new-release",
            "release_id": "new-release",
            "runtime_committed": true,
            "next_launch_saved": true,
            "pending_record_saved": true,
            "participants": [],
            "error": null,
            "updated_at_ms": 1
        }
    }))
    .unwrap()
}
fn restart_context() -> crate::windows::Context {
    crate::windows::Context {
        executable: PathBuf::from("releases/new-release")
            .join(astra_platform::platform::executable("astra-tui")),
        home: "state".into(),
        config: "config".into(),
        bin: "releases/new-release".into(),
        language: Language::Traditional,
    }
}
fn restart_for_completed_update() -> Restart {
    Restart {
        observed: true,
        attempted: Some(("current-operation".into(), "new-release".into())),
        ..Default::default()
    }
}
#[tokio::test]
async fn restart_does_not_replay_a_historical_completion() {
    let status = completed_update("historical-operation");
    let context = restart_context();
    let mut restart = Restart::default();
    for idle in [false, true, true] {
        assert!(restart
            .poll(Some(&status), &context, idle)
            .await
            .unwrap()
            .is_none());
        assert!(restart.checking.is_none());
        assert!(restart.ready.is_none());
    }
    assert_eq!(
        restart.attempted,
        Some(("historical-operation".into(), "new-release".into()))
    );
}
#[test]
fn replacement_can_follow_an_update_completed_during_its_startup() {
    let mut restart = Restart::new(true);
    let key = restart.candidate(&completed_update("completed-during-restart"));
    assert!(key.is_some());
    assert_ne!(key, restart.attempted);
}
#[test]
fn restart_distinguishes_new_transactions_even_for_the_same_release() {
    let mut restart = Restart::default();
    let first = restart.candidate(&completed_update("historical-operation"));
    assert_eq!(restart.attempted, first);
    let second = restart.candidate(&completed_update("current-operation"));
    assert_eq!(
        second,
        Some(("current-operation".into(), "new-release".into()))
    );
    assert_ne!(second, restart.attempted);

    // A transaction already running when the UI opens is eligible when it
    // later completes; only an initial *completion* is historical.
    let mut preparing = completed_update("current-operation");
    preparing.operation.as_mut().unwrap().phase = crate::protocol::updates::UpdatePhase::Preparing;
    let mut observing = Restart::default();
    assert!(observing.candidate(&preparing).is_none());
    assert_eq!(
        observing.candidate(&completed_update("current-operation")),
        second
    );
    assert!(observing.attempted.is_none());
}
#[tokio::test]
async fn restart_waits_for_validation_and_idle_then_delivers_only_once() {
    let status = completed_update("current-operation");
    let context = restart_context();
    let target = context.clone();
    let (complete, checked) = tokio::sync::oneshot::channel();
    let mut restart = restart_for_completed_update();
    restart.checking = Some(tokio::spawn(async move {
        checked.await.unwrap();
        Ok(Some(target))
    }));
    assert!(restart
        .poll(Some(&status), &context, true)
        .await
        .unwrap()
        .is_none());
    assert!(restart.checking.is_some());
    complete.send(()).unwrap();
    while !restart.checking.as_ref().unwrap().is_finished() {
        tokio::task::yield_now().await;
    }
    assert!(restart
        .poll(Some(&status), &context, false)
        .await
        .unwrap()
        .is_none());
    assert!(restart.checking.is_none());
    assert!(restart.ready.is_some());
    let delivered = restart
        .poll(Some(&status), &context, true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(delivered.executable, context.executable);
    assert!(restart.ready.is_none());
    assert!(restart
        .poll(Some(&status), &context, true)
        .await
        .unwrap()
        .is_none());
    assert!(restart.checking.is_none());
}
#[tokio::test]
async fn restart_discards_validation_when_a_new_operation_starts() {
    let context = restart_context();
    let target = context.clone();
    let mut restart = restart_for_completed_update();
    let (complete, checked) = tokio::sync::oneshot::channel();
    restart.checking = Some(tokio::spawn(async move {
        checked.await.unwrap();
        Ok(Some(target))
    }));
    let mut next = completed_update("next-operation");
    next.operation.as_mut().unwrap().phase = crate::protocol::updates::UpdatePhase::Preparing;
    assert!(restart
        .poll(Some(&next), &context, true)
        .await
        .unwrap()
        .is_none());
    complete.send(()).unwrap();
    while !restart.checking.as_ref().unwrap().is_finished() {
        tokio::task::yield_now().await;
    }
    assert!(restart
        .poll(Some(&next), &context, true)
        .await
        .unwrap()
        .is_none());
    assert!(restart.ready.is_none());
    assert!(restart.checking.is_none());
    assert_eq!(
        restart.attempted,
        Some(("current-operation".into(), "new-release".into()))
    );
}
#[tokio::test]
async fn restart_can_retry_delivery_after_a_failed_session_checkpoint() {
    let status = completed_update("current-operation");
    let context = restart_context();
    let mut restart = restart_for_completed_update();
    restart.ready = Some(context.clone());
    let target = restart
        .poll(Some(&status), &context, true)
        .await
        .unwrap()
        .unwrap();
    // The event loop defers this verified target if saving the draft fails.
    restart.defer(target);
    assert!(restart
        .poll(Some(&status), &context, false)
        .await
        .unwrap()
        .is_none());
    assert!(restart.ready.is_some());
    let retry = restart
        .poll(Some(&status), &context, true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(retry.executable, context.executable);
    assert!(restart
        .poll(Some(&status), &context, true)
        .await
        .unwrap()
        .is_none());
}
