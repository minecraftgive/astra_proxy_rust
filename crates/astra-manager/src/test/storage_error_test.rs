use super::*;

fn memory() -> Store {
    Store::open(Path::new(":memory:")).unwrap()
}

fn document() -> Document {
    Document {
        instance: "proxy".into(),
        path: "config.toml".into(),
        text: "value = 1".into(),
        revision: "source".into(),
        kind: InstanceKind::Proxy,
    }
}

#[test]
fn malformed_request_ids_cursors_and_revisions_have_explicit_invalid_request_codes() {
    let store = memory();
    for id in ["".to_owned(), "x".repeat(161)] {
        for error in [
            store.begin(&id, "input").unwrap_err(),
            store.begin_network(&id, "input").unwrap_err(),
            store.network_existing(&id, "input").unwrap_err(),
            store
                .route(&id, "agent", "request", "forwarding")
                .unwrap_err(),
            store.open_draft(id, document()).unwrap_err(),
        ] {
            assert!(matches!(
                crate::error::code(&error.context("input boundary")),
                ErrorCode::InvalidRequest
            ));
        }
    }
    assert!(store.tasks(10).unwrap().is_empty());
    store.begin_network("report", "input").unwrap();
    for cursor in ["garbage", "-1", "18446744073709551616"] {
        assert!(matches!(
            crate::error::code(&store.events(Some(cursor), 10).unwrap_err()),
            ErrorCode::InvalidRequest
        ));
        assert!(matches!(
            crate::error::code(&store.network_report("report", Some(cursor)).unwrap_err()),
            ErrorCode::InvalidRequest
        ));
    }
    assert!(matches!(
        crate::error::code(&store.network_report("report", Some("1")).unwrap_err()),
        ErrorCode::InvalidRequest
    ));
    // A syntactically valid maximum event cursor cannot overflow resync checking.
    store.events(Some(&i64::MAX.to_string()), 10).unwrap();
    let draft = store.open_draft("draft".into(), document()).unwrap();
    for revision in ["bad", "-1", "9223372036854775808"] {
        assert!(matches!(
            crate::error::code(&store.replace_draft(draft.clone(), revision).unwrap_err()),
            ErrorCode::InvalidRequest
        ));
        assert!(matches!(
            crate::error::code(&store.discard(&draft.id, revision).unwrap_err()),
            ErrorCode::InvalidRequest
        ));
    }
    assert!(matches!(
        crate::error::code(
            &store
                .replace_draft(draft, &i64::MAX.to_string())
                .unwrap_err()
        ),
        ErrorCode::InvalidRequest
    ));
    assert_eq!(store.draft("draft").unwrap().revision, "0");
}

#[test]
fn missing_records_are_not_found_but_sql_failures_remain_internal() {
    let store = memory();
    for error in [
        store.task("absent").unwrap_err(),
        store.draft("absent").unwrap_err(),
        store.network_report("absent", None).unwrap_err(),
    ] {
        assert!(matches!(crate::error::code(&error), ErrorCode::NotFound));
    }
    store
        .db
        .execute_batch(
            "ALTER TABLE operations RENAME COLUMN task TO unavailable;
        ALTER TABLE drafts RENAME COLUMN body TO unavailable;
        ALTER TABLE network_reports RENAME COLUMN body TO unavailable;",
        )
        .unwrap();
    for error in [
        store.task("absent").unwrap_err(),
        store.draft("absent").unwrap_err(),
        store.network_report("absent", None).unwrap_err(),
    ] {
        assert!(
            matches!(crate::error::code(&error), ErrorCode::Internal),
            "{error:#}"
        );
        assert!(error.chain().any(|cause| cause.is::<rusqlite::Error>()));
    }
}

#[test]
fn malformed_persisted_json_never_uses_request_or_message_classification() {
    let store = memory();
    store.begin_network("corrupt", "input").unwrap();
    store.open_draft("draft".into(), document()).unwrap();
    let malformed = "Conflict: Unsupported: not found; outcome unknown";
    for sql in [
        "UPDATE operations SET task=?1",
        "UPDATE network_reports SET body=?1",
        "UPDATE drafts SET body=?1",
    ] {
        store.db.execute(sql, [malformed]).unwrap();
    }
    for error in [
        store.task("corrupt").unwrap_err(),
        store.network_existing("corrupt", "input").unwrap_err(),
        store.network_report("corrupt", None).unwrap_err(),
        store.draft("draft").unwrap_err(),
        store.open_draft("draft".into(), document()).unwrap_err(),
    ] {
        assert!(
            matches!(crate::error::code(&error), ErrorCode::Internal),
            "{error:#}"
        );
        assert!(error.chain().any(|cause| cause.is::<serde_json::Error>()));
    }
}

#[test]
fn typed_finish_state_survives_misleading_text_and_preserves_failed_idempotence() {
    let store = memory();
    for (id, error, unknown) in [
        (
            "failed",
            anyhow::anyhow!("outcome unknown; not replayed"),
            false,
        ),
        (
            "unknown",
            crate::error::message(ErrorCode::OutcomeUnknown, "确认尚未收到"),
            true,
        ),
        (
            "save",
            crate::save::failed(
                SaveState::OutcomeUnknown,
                Some(true),
                None,
                "保存状态待确认",
            )
            .context("checkpoint"),
            true,
        ),
    ] {
        store.begin(id, "input").unwrap();
        store
            .finish(id, None, FinishOutcome::from_error(&error))
            .unwrap();
        let task = store.task(id).unwrap();
        assert_eq!(matches!(task.state, TaskState::OutcomeUnknown), unknown);
        assert!(matches!(
            task.state,
            TaskState::Failed | TaskState::OutcomeUnknown
        ));
        assert_eq!(task.message, format!("{error:#}"));
        assert!(task.finished_ms.is_some());
        let before = store.events(None, 100).unwrap().events.len();
        let Some(ResponseBody::Task(replayed)) = store.begin(id, "input").unwrap() else {
            panic!("a terminal failed operation must not execute again");
        };
        assert_eq!(
            serde_json::to_value(replayed).unwrap(),
            serde_json::to_value(task).unwrap()
        );
        assert_eq!(store.events(None, 100).unwrap().events.len(), before);
    }
}

#[test]
fn completed_draft_retains_save_details_and_task_message() {
    let store = memory();
    let mut draft = store.open_draft("draft".into(), document()).unwrap();
    draft.last_save = Some(crate::save::outcome(
        SaveState::Applied,
        Some(true),
        Some(true),
        "保存完成",
    ));
    store.begin("save", "input").unwrap();
    store
        .finish(
            "save",
            Some(&ResponseBody::Draft(draft.clone())),
            FinishOutcome::Complete,
        )
        .unwrap();
    let task = store.task("save").unwrap();
    assert!(matches!(task.state, TaskState::Complete));
    assert_eq!(task.message, "保存完成");
    let Some(ResponseBody::Draft(replayed)) = store.begin("save", "input").unwrap() else {
        panic!("saved response must be retained");
    };
    assert_eq!(
        serde_json::to_value(replayed).unwrap(),
        serde_json::to_value(draft).unwrap()
    );
}
