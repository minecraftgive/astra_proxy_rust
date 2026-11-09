use super::*;
fn memory() -> Store {
    Store::open(Path::new(":memory:")).unwrap()
}
#[test]
fn contention_has_a_bounded_wait_and_does_not_accept_or_replay_the_write() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("management.sqlite");
    let store = Store::open(&path).unwrap();
    let timeout_ms: u64 = store
        .db
        .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
        .unwrap();
    assert_eq!(timeout_ms, BUSY_TIMEOUT.as_millis() as u64);
    let blocker = Connection::open(&path).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let started = std::time::Instant::now();
    let error = store.begin("blocked-operation", "input").unwrap_err();
    let elapsed = started.elapsed();
    assert_eq!(
        error
            .downcast_ref::<rusqlite::Error>()
            .unwrap()
            .sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy)
    );
    assert!(
        elapsed >= BUSY_TIMEOUT && elapsed < BUSY_TIMEOUT + Duration::from_secs(5),
        "busy wait was {elapsed:?}"
    );
    blocker.execute_batch("ROLLBACK").unwrap();
    assert!(store.task("blocked-operation").is_err());
    assert!(store.begin("blocked-operation", "input").unwrap().is_none());
    assert!(matches!(
        store.begin("blocked-operation", "input").unwrap(),
        Some(ResponseBody::Task(_))
    ));
}
#[test]
fn corrupt_startup_explains_recovery_and_preserves_database_and_sidecars() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("management.sqlite");
    let files = [
        (path.clone(), b"not a SQLite database".as_slice()),
        (
            directory.path().join("management.sqlite-wal"),
            b"retained WAL evidence".as_slice(),
        ),
        (
            directory.path().join("management.sqlite-shm"),
            b"retained SHM evidence".as_slice(),
        ),
    ];
    for (file, bytes) in &files {
        std::fs::write(file, bytes).unwrap();
    }
    let error = Store::open(&path).err().unwrap();
    let message = format!("{error:#}");
    assert!(
        message.contains("corrupt or invalid")
            && message.contains("-wal")
            && message.contains("-shm")
            && message.contains("No automatic repair")
    );
    assert_eq!(
        error
            .downcast_ref::<rusqlite::Error>()
            .unwrap()
            .sqlite_error_code(),
        Some(rusqlite::ErrorCode::NotADatabase)
    );
    for (file, bytes) in files {
        let actual = std::fs::read(&file).unwrap();
        assert!(
            actual == bytes,
            "{} changed: expected {} bytes, found {} bytes",
            file.display(),
            bytes.len(),
            actual.len()
        );
    }
    std::fs::write(&path, b"SQLite format").unwrap();
    assert!(
        Store::open(&path).is_err(),
        "truncated nonempty header was accepted"
    );
    assert!(std::fs::read(&path).unwrap() == b"SQLite format");
}
#[test]
fn header_preflight_accepts_new_empty_memory_and_active_wal_databases() {
    let directory = tempfile::tempdir().unwrap();
    let new = directory.path().join("new.sqlite");
    drop(Store::open(&new).unwrap());
    let empty = directory.path().join("empty.sqlite");
    std::fs::write(&empty, b"").unwrap();
    drop(Store::open(&empty).unwrap());
    drop(memory());
    let path = directory.path().join("active.sqlite");
    let keeper = Connection::open(&path).unwrap();
    keeper.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE retained(value TEXT); INSERT INTO retained VALUES('in active WAL');").unwrap();
    assert!(directory.path().join("active.sqlite-wal").is_file());
    let store = Store::open(&path).unwrap();
    let value: String = store
        .db
        .query_row("SELECT value FROM retained", [], |row| row.get(0))
        .unwrap();
    assert_eq!(value, "in active WAL");
}
#[test]
fn failed_or_uncertain_updates_never_become_successful_tasks() {
    use astra_management::updates::{
        UpdateIntent, UpdateOperation, UpdatePhase, UpdateRequest, UpdateStatus,
    };
    for (phase, expected) in [
        (UpdatePhase::Failed, "failed"),
        (UpdatePhase::NeedsRecovery, "needs_recovery"),
        (UpdatePhase::Uncertain, "outcome_unknown"),
    ] {
        let store = memory();
        store.begin("update", "fingerprint").unwrap();
        let status = UpdateStatus {
            operation: Some(UpdateOperation {
                request: UpdateRequest {
                    operation_id: "update".into(),
                    intent: UpdateIntent::Stage {
                        package: "package.tar.gz".into(),
                    },
                },
                phase,
                before_directory: "before".into(),
                target_directory: None,
                release_id: None,
                runtime_committed: false,
                next_launch_saved: false,
                pending_record_saved: false,
                participants: vec![],
                uncertain_preparations: vec![],
                error: Some("verification failed".into()),
                updated_at_ms: 1,
            }),
            ..Default::default()
        };
        store
            .finish(
                "update",
                Some(&ResponseBody::UpdateStatus(Box::new(status))),
                FinishOutcome::Complete,
            )
            .unwrap();
        let task = serde_json::to_value(store.task("update").unwrap()).unwrap();
        assert_eq!(task["state"], expected);
        assert_eq!(task["message"], "verification failed");
        assert_eq!(
            store.events(None, 10).unwrap().events.last().unwrap().kind,
            "operation_failed"
        );
    }
}
#[test]
fn progress_events_expose_objects_instead_of_nested_json_strings() {
    let store = memory();
    store
        .event(
            Some("operation"),
            "nettool_event",
            r#"{"Ping":{"sequence":"9007199254740993"}}"#,
        )
        .unwrap();
    let events = store.events(None, 10).unwrap();
    let event = serde_json::to_value(&events.events[0]).unwrap();
    assert_eq!(event["payload"]["type"], "net_tool");
    assert_eq!(
        event["payload"]["data"]["Ping"]["sequence"],
        "9007199254740993"
    );
    assert_eq!(event["message"], "Network probe progress");
}
#[test]
fn operation_route_cannot_change_agents() {
    let store = memory();
    store.route("op", "a", "request1", "forwarding").unwrap();
    assert!(matches!(
        crate::error::code(
            &store
                .route("op", "b", "request2", "forwarding")
                .unwrap_err()
        ),
        ErrorCode::Conflict
    ));
    let agent: String = store
        .db
        .query_row(
            "SELECT agent_id FROM controller_routes WHERE operation_id='op'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(agent, "a");
    store.route("op", "a", "request3", "responded").unwrap();
}
#[test]
fn operations_never_reexecute_and_reject_changed_identity() {
    let s = memory();
    assert!(s.begin("op", "a").unwrap().is_none());
    assert!(matches!(
        s.begin("op", "a").unwrap(),
        Some(ResponseBody::Task(_))
    ));
    assert!(s.begin("op", "b").is_err());
    s.finish(
        "op",
        Some(&ResponseBody::Done {
            message: "done".into(),
        }),
        FinishOutcome::Complete,
    )
    .unwrap();
    assert!(matches!(
        s.begin("op", "a").unwrap(),
        Some(ResponseBody::Done { .. })
    ));
}
#[test]
fn stale_frontend_cannot_replace_or_discard_newer_draft() {
    let s = memory();
    let d = s
        .open_draft(
            "d".into(),
            Document {
                instance: "i".into(),
                path: "a.toml".into(),
                text: "a=1".into(),
                revision: "base".into(),
                kind: InstanceKind::Proxy,
            },
        )
        .unwrap();
    let mut newer = d.clone();
    newer.text = "a=2".into();
    s.replace_draft(newer, "0").unwrap();
    assert!(s.replace_draft(d, "0").is_err());
    assert!(s.discard("d", "0").is_err());
    assert_eq!(s.draft("d").unwrap().text, "a=2");
}
#[test]
fn malformed_or_future_drafts_are_rejected_without_replacement() {
    let store = memory();
    let document = Document {
        instance: "i".into(),
        path: "a.toml".into(),
        text: "a=1".into(),
        revision: "base".into(),
        kind: InstanceKind::Proxy,
    };
    let draft = store.open_draft("d".into(), document.clone()).unwrap();
    let mut future = serde_json::to_value(draft).unwrap();
    future["future_field"] = serde_json::json!(true);
    for original in [
        "{broken".to_owned(),
        serde_json::to_string(&future).unwrap(),
    ] {
        store
            .db
            .execute("UPDATE drafts SET body=?1 WHERE id='d'", [&original])
            .unwrap();
        assert!(store.open_draft("d".into(), document.clone()).is_err());
        let retained: String = store
            .db
            .query_row("SELECT body FROM drafts WHERE id='d'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(retained, original);
    }
}
#[test]
fn future_database_version_is_rejected_before_schema_changes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("management.sqlite");
    let db = Connection::open(&path).unwrap();
    db.execute_batch("PRAGMA user_version=99; CREATE TABLE future_state(value TEXT); INSERT INTO future_state VALUES('retain');").unwrap();
    drop(db);
    let original = std::fs::read(&path).unwrap();
    assert!(Store::open(&path).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), original);
}
