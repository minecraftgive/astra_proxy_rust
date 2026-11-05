use super::*;
#[test]
fn remote_update_failure_is_visible_without_a_local_job() {
    use crate::protocol::updates::UpdatePhase;
    crate::i18n::with_language(crate::i18n::Language::English, || {
        let mut ui = Ui::new(Session::default());
        let status = serde_json::from_value(serde_json::json!({
            "operation": {
                "request": {"operation_id":"remote-op", "intent":{"kind":"activate", "release_id":"v2"}},
                "phase":"needs_recovery", "before_directory":"old", "target_directory":"new",
                "release_id":"v2", "runtime_committed":true, "next_launch_saved":false,
                "pending_record_saved":true, "participants":[], "error":"save failed", "updated_at_ms":1
            }
        })).unwrap();
        ui.update_status = Some(status);
        let line = ui.status_line(180).to_string();
        assert!(line.contains("remote-op") && line.contains("save failed"));
        assert!(ui.tasks.records.is_empty());
        ui.update_status
            .as_mut()
            .unwrap()
            .operation
            .as_mut()
            .unwrap()
            .phase = UpdatePhase::Complete;
        assert!(ui.update_alert().is_none());
        ui.tasks.update_error = Some("disconnected".into());
        assert!(ui.status_line(180).to_string().contains("disconnected"));
    });
}

#[test]
fn background_disk_observation_preserves_newer_foreground_draft_and_conflict_diff() {
    let mut ui = Ui::new(Session::default());
    ui.open_draft("proxy".into(), "standalone=true\n".into());
    ui.editor
        .as_mut()
        .unwrap()
        .import("standalone=false\n# local\n".into())
        .unwrap();
    ui.remember_draft();
    let observed = operations::OperationInput::capture(&ui)
        .prepare("proxy".into(), "standalone=true\n# external\n".into());
    ui.editor
        .as_mut()
        .unwrap()
        .import("standalone=false\n# newer local\n".into())
        .unwrap();
    operations::merge_observation(&mut ui, &observed);
    assert!(ui.editor.as_ref().unwrap().text().contains("newer local"));
    ui.remember_draft();
    let draft = ui.drafts.values().next().unwrap();
    assert!(draft.conflict);
    assert!(draft.observed_disk.as_ref().unwrap().contains("external"));
    ui.compare_draft();
    assert!(ui
        .help_override
        .as_ref()
        .is_some_and(|text| text.contains("external") && text.contains("newer local")));
}
#[test]
fn unchanged_update_heartbeat_does_not_request_another_frame() {
    let a = Reply::text(
        r#"{"schema_version":1,"observed_at_ms":10,"operation":{"phase":"preparing"}}"#,
    );
    let b = Reply::text(
        r#"{"schema_version":1,"observed_at_ms":20,"operation":{"phase":"preparing"}}"#,
    );
    let c =
        Reply::text(r#"{"schema_version":1,"observed_at_ms":20,"operation":{"phase":"complete"}}"#);
    assert_eq!(
        feed_signature(&a, &Action::WatchUpdates).unwrap(),
        feed_signature(&b, &Action::WatchUpdates).unwrap()
    );
    assert_ne!(
        feed_signature(&b, &Action::WatchUpdates).unwrap(),
        feed_signature(&c, &Action::WatchUpdates).unwrap()
    );
}
#[tokio::test]
async fn slow_save_keeps_navigation_live_and_preserves_newer_draft() {
    tokio::task::LocalSet::new().run_until(async {
        let root=std::env::temp_dir().join(format!("astra-ui-background-{}",std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path=root.join("management.sock");
        let listener=astra_platform::platform::LocalListener::bind(&path).unwrap();
        let started=std::sync::Arc::new(tokio::sync::Notify::new());
        let release=std::sync::Arc::new(tokio::sync::Notify::new());
        let (entered,finish)=(started.clone(),release.clone());
        let server=tokio::spawn(async move {
            let (stream, request)=test_rpc::save_request(&listener,"standalone=false\n","standalone=true\n").await;
            entered.notify_one();finish.notified().await;
            test_rpc::reply(stream,request,astra_management::v2::ResponseBody::Draft(test_rpc::draft("standalone=true\n","standalone=true\n","2"))).await;
        });
        let mut ui=Ui::new(Session::default());
        ui.editor=Some(Editor::new("proxy".into(),"standalone=true\n".into()).with_kind(Kind::Proxy));
        let mut jobs=Jobs::new(&path);
        jobs.start(&mut ui,Action::Save{name:"proxy".into(),original:"standalone=false\n".into(),text:"standalone=true\n".into()},None);
        tokio::time::timeout(Duration::from_secs(2),started.notified()).await.unwrap();
        assert!(!jobs.jobs[0].handle.is_finished());
        ui.key(event::KeyEvent::new(KeyCode::F(6),KeyModifiers::NONE));
        assert!(ui.tasks.details.is_some());
        ui.key(event::KeyEvent::new(KeyCode::Esc,KeyModifiers::NONE));
        ui.editor.as_mut().unwrap().import("standalone=false\n".into()).unwrap();
        release.notify_one();server.await.unwrap();
        tokio::time::timeout(Duration::from_secs(2),async {
            while !jobs.jobs.is_empty(){jobs.poll(&mut ui).await;tokio::time::sleep(Duration::from_millis(5)).await;}
        }).await.unwrap();
        assert!(ui.editor.as_ref().unwrap().text().contains("standalone=false"));
        assert_eq!(ui.editor.as_ref().unwrap().original,"standalone=true\n");
        // The finished task's result is the status line, marked as a success.
        assert!(matches!(&ui.tasks.verdict, Some((_, false))));
        let status: String = ui.status_line(100).spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(status.starts_with(" ✓ ") && status.lines().count() == 1, "{status}");
        assert!(ui.tasks.records[0].events.back().unwrap().contains(t("Completed")));
        assert!(ui.tasks.records[0].events.back().unwrap().chars().nth(2)==Some(':'));
        let draft=ui.editor.as_ref().unwrap().text();
        assert!(matches!(ui.key(event::KeyEvent::new(KeyCode::F(7),KeyModifiers::NONE)),Input::Run(Action::Logs(name),_)if name=="@tui"));
        assert_eq!(ui.editor.as_ref().unwrap().text(),draft);
        drop(jobs);astra_platform::platform::remove_socket(&path);std::fs::remove_dir_all(root).unwrap();
    }).await;
}
#[tokio::test]
async fn state_feed_reconnects_after_server_disconnects() {
    let path = std::env::temp_dir().join(format!("astra-ui-feed-{}.sock", std::process::id()));
    let listener = astra_platform::platform::LocalListener::bind(&path).unwrap();
    let server = tokio::spawn(async move {
        use astra_management::v2::*;
        for name in ["first", "reconnected"] {
            let (stream, request) = test_rpc::next(&listener).await;
            assert!(matches!(request.body, RequestBody::Query(Query::Instances)));
            test_rpc::reply(
                stream,
                request,
                ResponseBody::Instances(vec![Instance {
                    id: name.into(),
                    name: name.into(),
                    kind: InstanceKind::Proxy,
                    config: "proxy.toml".into(),
                    pid: None,
                    state: "Stopped".into(),
                    uptime_secs: "0".into(),
                    capabilities: vec![],
                    changed: false,
                    details: None,
                }]),
            )
            .await;
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    });
    let mut feed = Feed::start(path.clone(), Action::WatchList);
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if matches!(feed.take(),Some(Ok(reply))if reply.rows.first().is_some_and(|r|r.instance.name=="reconnected")) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    drop(feed);
    server.await.unwrap();
    astra_platform::platform::remove_socket(&path);
}
