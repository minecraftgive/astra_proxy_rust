use super::*;
use serde_json::json;

fn memory() -> Store {
    Store::open(Path::new(":memory:")).unwrap()
}

fn all_items(store: &Store, id: &str) -> Vec<NetworkResult> {
    let mut cursor = "0".to_owned();
    let mut items = Vec::new();
    loop {
        let page = store.network_report(id, Some(&cursor)).unwrap();
        assert!(
            serde_json::to_vec(&Response::success(
                "request".into(),
                ResponseBody::NetworkReport(page.clone())
            ))
            .unwrap()
            .len()
                <= NETWORK_PAGE_BYTES
        );
        assert!(
            !page.has_more || page.cursor != cursor,
            "pagination stalled"
        );
        cursor = page.cursor.clone();
        items.extend(page.items);
        if !page.has_more {
            return items;
        }
    }
}

#[test]
fn network_restart_preserves_partial_evidence_and_does_not_replay() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("management.sqlite");
    {
        let store = Store::open(&path).unwrap();
        store.begin_network("queued", "queued-input").unwrap();
        store.begin_network("running", "running-input").unwrap();
        store
            .network_state("running", NetworkState::Running, "Running")
            .unwrap();
        store
            .append_network("running", json!({"answer": "retained"}))
            .unwrap();
        store.begin_network("complete", "complete-input").unwrap();
        store
            .network_state("complete", NetworkState::Complete, "Completed")
            .unwrap();
    }
    let store = Store::open(&path).unwrap();
    for id in ["queued", "running"] {
        let report = store.network_report(id, None).unwrap();
        assert_eq!(report.state, NetworkState::Interrupted);
        assert!(report.finished_ms.is_some());
        let task = store
            .network_existing(id, &format!("{id}-input"))
            .unwrap()
            .unwrap();
        assert!(matches!(task.state, TaskState::OutcomeUnknown));
        assert!(task.finished_ms.is_some());
        assert!(matches!(
            store.begin(id, &format!("{id}-input")).unwrap(),
            Some(ResponseBody::Task(_))
        ));
    }
    assert_eq!(all_items(&store, "running")[0].data["answer"], "retained");
    assert_eq!(
        store.network_report("complete", None).unwrap().state,
        NetworkState::Complete
    );
    drop(store);
    let reopened = Store::open(&path).unwrap();
    assert_eq!(
        reopened.network_report("running", None).unwrap().state,
        NetworkState::Interrupted
    );
}

#[test]
fn network_fingerprint_is_checked_before_and_after_completion() {
    let store = memory();
    assert!(store.network_existing("op", "input").unwrap().is_none());
    store.begin_network("op", "input").unwrap();
    for terminal in [false, true] {
        if terminal {
            store
                .network_state("op", NetworkState::Cancelled, "Cancelled")
                .unwrap();
        }
        assert!(store.network_existing("op", "input").unwrap().is_some());
        assert!(matches!(
            crate::error::code(&store.network_existing("op", "changed").unwrap_err()),
            ErrorCode::Conflict
        ));
        assert!(matches!(
            crate::error::code(&store.begin("op", "changed").unwrap_err()),
            ErrorCode::Conflict
        ));
    }
    assert!(store.append_network("op", json!({"late": true})).is_err());
    assert!(store
        .network_state("op", NetworkState::Running, "Restart")
        .is_err());
    assert_eq!(
        store.network_report("op", None).unwrap().state,
        NetworkState::Cancelled
    );
}

#[test]
fn network_admission_rolls_back_if_retention_cleanup_fails() {
    let store = memory();
    store.begin_network("old", "old-input").unwrap();
    store
        .append_network("old", json!({"evidence": "retained"}))
        .unwrap();
    store
        .network_state("old", NetworkState::Complete, "Completed")
        .unwrap();
    store
        .db
        .execute("UPDATE network_reports SET updated=0 WHERE id='old'", [])
        .unwrap();
    store
        .db
        .execute_batch(
            "CREATE TRIGGER fail_retention BEFORE DELETE ON network_items
        BEGIN SELECT RAISE(ABORT, 'fixture retention failure'); END;",
        )
        .unwrap();
    let error = store.begin_network("new", "new-input").unwrap_err();
    assert!(error.to_string().contains("fixture retention failure"));
    assert!(store
        .network_existing("new", "new-input")
        .unwrap()
        .is_none());
    assert!(store.task("new").is_err());
    let (expired, evidence): (bool, usize) = store.db.query_row(
        "SELECT expired,(SELECT COUNT(*) FROM network_items WHERE operation_id='old') FROM network_reports WHERE id='old'",
        [], |row| Ok((row.get(0)?,row.get(1)?))
    ).unwrap();
    assert!(!expired);
    assert_eq!(evidence, 1);
    store
        .db
        .execute_batch("DROP TRIGGER fail_retention")
        .unwrap();
    store.begin_network("new", "new-input").unwrap();
    assert!(store.network_report("old", None).unwrap().expired);
    assert_eq!(
        store.network_report("new", None).unwrap().state,
        NetworkState::Queued
    );
}

#[test]
fn network_pages_are_bounded_and_resume_without_losing_results() {
    let store = memory();
    store.begin_network("pages", "input").unwrap();
    for index in 0..8 {
        store
            .append_network("pages", json!({"index": index, "text": "答".repeat(6000)}))
            .unwrap();
    }
    let first = store.network_report("pages", None).unwrap();
    assert!(first.has_more);
    let items = all_items(&store, "pages");
    assert_eq!(items.len(), 8);
    for (index, item) in items.iter().enumerate() {
        assert_eq!(item.id, (index + 1).to_string());
        assert_eq!(item.data["index"], index);
    }
    let empty = store.network_report("pages", Some("8")).unwrap();
    assert!(empty.items.is_empty());
    assert!(!empty.has_more);
    assert_eq!(empty.cursor, "8");
    store.append_network("pages", json!({"index": 8})).unwrap();
    let resumed = store.network_report("pages", Some(&empty.cursor)).unwrap();
    assert_eq!(resumed.items.len(), 1);
    assert_eq!(resumed.items[0].id, "9");
    for cursor in ["-1", "invalid", "18446744073709551616", "10"] {
        assert!(store.network_report("pages", Some(cursor)).is_err());
    }
}

#[test]
fn network_item_at_page_boundary_never_stalls_pagination() {
    let store = memory();
    store.begin_network("boundary", "input").unwrap();
    let item = NetworkResult {
        id: "1".into(),
        sampled_at_ms: now(),
        data: json!(""),
    };
    let overhead = serde_json::to_vec(&item).unwrap().len();
    // Include the array separator: this item must fit or be marked truncated,
    // never be stored behind a cursor that cannot advance.
    let payload = "x".repeat(NETWORK_PAGE_BYTES - 4096 - overhead);
    store.append_network("boundary", json!(payload)).unwrap();
    let report = store.network_report("boundary", None).unwrap();
    assert!(report.truncated || report.items.len() == 1);
    assert!(!report.has_more || report.cursor != "0");
    all_items(&store, "boundary");
}

#[test]
fn network_report_budget_marks_partial_and_retains_earlier_results() {
    let store = memory();
    store.begin_network("large", "input").unwrap();
    let data = json!({"data": "x".repeat(32 * 1024)});
    for _ in 0..80 {
        store.append_network("large", data.clone()).unwrap();
    }
    let before = store.network_report("large", None).unwrap();
    assert!(before.truncated);
    store.append_network("large", json!("late")).unwrap();
    store
        .network_state("large", NetworkState::Complete, "Completed")
        .unwrap();
    let report = store.network_report("large", None).unwrap();
    assert_eq!(report.state, NetworkState::Partial);
    assert!(report.truncated);
    assert!(report.message.contains("truncated"));
    let items = all_items(&store, "large");
    assert!(!items.is_empty() && items.len() < 80);
    assert!(items.iter().all(|item| item.data == data));
    let stored: usize = store
        .db
        .query_row(
            "SELECT bytes+length(CAST(body AS BLOB)) FROM network_reports WHERE id='large'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(stored <= NETWORK_REPORT_BYTES);
    assert!(matches!(
        store.task("large").unwrap().state,
        TaskState::Failed
    ));
}

#[test]
fn source_truncation_marks_partial_after_retaining_available_record_chunks() {
    let store = memory();
    store.begin_network("dns-truncated", "input").unwrap();
    store
        .append_network(
            "dns-truncated",
            json!({"event":"dns", "detail":{"kind":"response", "truncated":true}}),
        )
        .unwrap();
    store.append_network("dns-truncated", json!({"event":"dns", "detail":{"kind":"records", "records":[{"text":"retained evidence"}]}})).unwrap();
    store.network_truncated("dns-truncated").unwrap();
    store
        .network_state("dns-truncated", NetworkState::Complete, "Completed")
        .unwrap();
    let report = store.network_report("dns-truncated", None).unwrap();
    assert!(report.truncated);
    assert_eq!(report.state, NetworkState::Partial);
    assert_eq!(report.items.len(), 2);
}

#[test]
fn network_events_only_reference_evidence_and_reserve_terminal_slot() {
    let store = memory();
    store.begin_network("events", "input").unwrap();
    store
        .network_state("events", NetworkState::Running, "Running")
        .unwrap();
    for index in 0..400 {
        store
            .append_network("events", json!({"secret_evidence": index}))
            .unwrap();
    }
    store
        .network_state("events", NetworkState::Cancelled, "Cancelled")
        .unwrap();
    let events = store.events(None, 256).unwrap().events;
    assert_eq!(events.len(), NETWORK_EVENT_COUNT);
    for event in &events {
        assert_eq!(event.kind, "network_progress");
        assert!(event.payload.is_none());
        assert!(!event.message.contains("secret_evidence"));
        assert!(serde_json::to_vec(event).unwrap().len() <= NETWORK_EVENT_BYTES);
        let message: serde_json::Value = serde_json::from_str(&event.message).unwrap();
        assert!(message["report_revision"].is_string());
        assert!(message["stage"].is_string());
    }
    let last: serde_json::Value = serde_json::from_str(&events.last().unwrap().message).unwrap();
    assert_eq!(last["stage"], "cancelled");
    assert_eq!(all_items(&store, "events").len(), 400);
}

#[test]
fn network_event_lifetime_budget_survives_global_ring_eviction() {
    let mut store = memory();
    store.set_event_limit(64);
    store.begin_network("ring", "input").unwrap();
    for index in 0..400 {
        store
            .append_network("ring", json!({"index": index}))
            .unwrap();
        store
            .event(None, "unrelated", "advance the global ring")
            .unwrap();
    }
    store
        .network_state("ring", NetworkState::Complete, "Completed")
        .unwrap();
    let emitted: usize = store
        .db
        .query_row(
            "SELECT event_count FROM network_reports WHERE id='ring'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(emitted, NETWORK_EVENT_COUNT);
    let batch = store.events(None, 256).unwrap();
    let diagnostic = batch
        .events
        .iter()
        .filter(|event| event.operation_id.as_deref() == Some("ring"))
        .collect::<Vec<_>>();
    assert_eq!(diagnostic.len(), 1);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&diagnostic[0].message).unwrap()["stage"],
        "complete"
    );
    assert_eq!(all_items(&store, "ring").len(), 400);
}

#[test]
fn network_retention_removes_old_terminal_evidence_but_preserves_idempotency() {
    let store = memory();
    // The queued job is older than every terminal job but cannot be evicted.
    store.begin_network("active", "active-input").unwrap();
    for index in 0..NETWORK_RETENTION_COUNT + 2 {
        let id = format!("done-{index:02}");
        store.begin_network(&id, &id).unwrap();
        store.append_network(&id, json!({"index": index})).unwrap();
        store
            .network_state(&id, NetworkState::Complete, "Completed")
            .unwrap();
    }
    let retained: usize = store
        .db
        .query_row(
            "SELECT COUNT(*) FROM network_reports WHERE expired=0",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(retained <= NETWORK_RETENTION_COUNT);
    let active = store.network_report("active", None).unwrap();
    assert_eq!(active.state, NetworkState::Queued);
    assert!(!active.expired);
    let old = store.network_report("done-00", None).unwrap();
    assert!(old.expired);
    assert_eq!(old.state, NetworkState::Complete);
    assert!(old.items.is_empty());
    assert!(!old.has_more);
    assert!(store
        .network_existing("done-00", "done-00")
        .unwrap()
        .is_some());
    assert!(store.network_existing("done-00", "changed").is_err());
    assert!(matches!(
        store.begin("done-00", "done-00").unwrap(),
        Some(ResponseBody::Task(_))
    ));
    assert!(!store
        .events(None, 256)
        .unwrap()
        .events
        .iter()
        .any(|event| event.operation_id.as_deref() == Some("done-00")));
}

#[test]
fn network_seven_day_retention_expires_only_terminal_data() {
    let store = memory();
    for id in ["old", "active", "recent"] {
        store.begin_network(id, id).unwrap();
        store.append_network(id, json!({"retained": id})).unwrap();
    }
    for id in ["old", "recent"] {
        store
            .network_state(id, NetworkState::Complete, "Completed")
            .unwrap();
    }
    let old = astra_ipc::progress::now_ms() - NETWORK_RETENTION_MS - 1;
    store
        .db
        .execute(
            "UPDATE network_reports SET updated=?1 WHERE id IN ('old','active')",
            [old],
        )
        .unwrap();
    store.prune_network().unwrap();
    assert!(store.network_report("old", None).unwrap().expired);
    assert!(!store.network_report("active", None).unwrap().expired);
    assert!(!store.network_report("recent", None).unwrap().expired);
    assert_eq!(all_items(&store, "active").len(), 1);
    assert_eq!(all_items(&store, "recent").len(), 1);
    assert!(store.network_existing("old", "old").unwrap().is_some());
}

#[test]
fn network_retention_cursor_gap_reports_resync_even_with_older_events_present() {
    let store = memory();
    store
        .event(None, "unrelated", "retain this older event")
        .unwrap();
    let cursor = store.events(None, 1).unwrap().cursor;
    store.begin_network("expire", "input").unwrap();
    store
        .append_network("expire", json!({"answer": "private"}))
        .unwrap();
    store
        .network_state("expire", NetworkState::Complete, "Completed")
        .unwrap();
    store
        .event(None, "unrelated", "retain this newer event")
        .unwrap();
    store
        .db
        .execute("UPDATE network_reports SET updated=0 WHERE id='expire'", [])
        .unwrap();
    store.prune_network().unwrap();
    let batch = store.events(Some(&cursor), 256).unwrap();
    assert!(batch.resync_required);
    assert_eq!(batch.events.len(), 1);
    assert_eq!(batch.events[0].message, "retain this newer event");
    assert!(
        !store
            .events(Some(&batch.cursor), 256)
            .unwrap()
            .resync_required
    );
}

#[test]
fn network_aggregate_budget_includes_evidence_events_and_preserves_active_work() {
    let store = memory();
    store.begin_network("active", "active").unwrap();
    for id in ["older", "newer"] {
        store.begin_network(id, id).unwrap();
        store.append_network(id, json!({"retained": id})).unwrap();
        store
            .network_state(id, NetworkState::Complete, "Completed")
            .unwrap();
    }
    // Exercise the aggregate limit independently of the 32-report ceiling.
    // byte counters are storage accounting; no 64 MiB test fixture is necessary.
    store
        .db
        .execute(
            "UPDATE network_reports SET bytes=?1 WHERE id IN ('active','older')",
            [NETWORK_RETENTION_BYTES / 2],
        )
        .unwrap();
    store
        .db
        .execute(
            "UPDATE network_reports SET updated=updated-1 WHERE id='older'",
            [],
        )
        .unwrap();
    store.prune_network().unwrap();
    assert!(store.network_report("older", None).unwrap().expired);
    assert!(!store.network_report("active", None).unwrap().expired);
    assert!(!store.network_report("newer", None).unwrap().expired);
}

#[test]
fn network_states_keep_the_strict_legacy_task_shape() {
    let store = memory();
    for state in [
        NetworkState::Running,
        NetworkState::Complete,
        NetworkState::Partial,
        NetworkState::Failed,
        NetworkState::Cancelled,
        NetworkState::Interrupted,
    ] {
        let id = format!("{state:?}");
        store.begin_network(&id, &id).unwrap();
        store.network_state(&id, state, "status").unwrap();
        let value = serde_json::to_value(store.task(&id).unwrap()).unwrap();
        let mut keys: Vec<_> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["finished_ms", "id", "message", "started_ms", "state"]
        );
        let expected = match state {
            NetworkState::Running => "running",
            NetworkState::Complete => "complete",
            NetworkState::Interrupted => "outcome_unknown",
            _ => "failed",
        };
        assert_eq!(value["state"], expected);
        serde_json::from_value::<Task>(value.clone()).unwrap();
        let mut extended = value;
        extended["network_state"] = json!(state);
        assert!(serde_json::from_value::<Task>(extended).is_err());
    }
}
