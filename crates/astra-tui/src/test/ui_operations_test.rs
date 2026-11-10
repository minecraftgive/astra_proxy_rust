use super::*;

#[tokio::test]
async fn force_start_skips_configuration_preparation_that_blocks_unverified_sessions() {
    use astra_management::v2 as v;
    let fixture = crate::import::tests::Fixture::new();
    let socket = fixture.0.join("management.sock");
    let listener = astra_platform::platform::LocalListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let mut peers = tokio::task::JoinSet::new();
        loop {
            let (mut stream, _) = tokio::select! {
                peer = listener.accept() => peer.unwrap(),
                completed = peers.join_next(), if !peers.is_empty() => {
                    completed.unwrap().unwrap();
                    continue;
                }
            };
            peers.spawn(async move {
                let request: v::Request = astra_management::client::receive(&mut stream)
                    .await
                    .unwrap();
                let response = match &request.body {
                v::RequestBody::Execute {
                    command: v::Command::ForceStart { instance }, ..
                } if instance == "proxy" => v::ResponseBody::Done {
                    message: "Started after replacing previous session".into(),
                },
                v::RequestBody::Query(v::Query::Events { .. }) => {
                    v::ResponseBody::Events(v::EventBatch {
                        events: vec![],
                        cursor: "0".into(),
                        resync_required: false,
                    })
                }
                other => panic!("Force start must reach the manager without preparing the blocked instance: {other:?}"),
                };
                test_rpc::reply(stream, request, response).await;
            });
        }
    });
    let ui = Ui::new(Session::default());
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        perform(
            &socket,
            OperationInput::capture(&ui),
            Action::ForceStart("proxy".into()),
            Some((Prompt::ForceStart("proxy".into()), "yes".into())),
        ),
    )
    .await;
    server.abort();
    let server_result = server.await;
    assert!(server_result.unwrap_err().is_cancelled());
    let result = result.unwrap();
    assert!(
        matches!(result, JobResult::Done { .. }),
        "{}",
        result.message()
    );
    astra_platform::platform::remove_socket(&socket);
}

#[tokio::test]
async fn server_logs_read_chunks_without_requesting_capture_control() {
    use astra_management::v2 as v;
    let fixture = crate::import::tests::Fixture::new();
    let socket = fixture.0.join("management.sock");
    let listener = astra_platform::platform::LocalListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        // The session list is read first; a server's sessions are its own.
        let (stream, request) = test_rpc::next(&listener).await;
        assert!(
            matches!(
                &request.body,
                v::RequestBody::Query(v::Query::LogSessions { instance }) if instance == "edge"
            ),
            "Server logs must not issue a capture-level request: {:?}",
            request.body
        );
        test_rpc::reply(
            stream,
            request,
            v::ResponseBody::LogSessions(vec![v::LogSession {
                id: "20260101T000000.000Z".into(),
                started_ms: "1767225600000".into(),
                size: "20".into(),
                current: true,
            }]),
        )
        .await;
        let (stream, request) = test_rpc::next(&listener).await;
        assert!(
            matches!(
                &request.body,
                v::RequestBody::Query(v::Query::LogChunk { instance, session: None, .. }) if instance == "edge"
            ),
            "Server logs must not issue a capture-level request: {:?}",
            request.body
        );
        test_rpc::reply(
            stream,
            request,
            v::ResponseBody::LogChunk(v::LogChunk::Data {
                bytes: b"INFO server started\n".to_vec(),
                cursor: v::LogCursor {
                    device: "1".into(),
                    inode: "2".into(),
                    offset: "20".into(),
                },
                reset: true,
                skip_prefix: false,
                more: false,
            }),
        )
        .await;
    });
    let mut ui = Ui::new(Session::default());
    // The target kind wins even when a server's log is opened from the
    // client perspective or a detached log window.
    ui.kind = Kind::Proxy;
    ui.rows.push(Row {
        instance: Instance {
            name: "edge".into(),
            kind: Kind::Server,
            config: fixture.0.join("server.toml"),
        },
        pid: None,
        state: "Stopped".into(),
        uptime: 0,
        changed: false,
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        let result = perform(
            &socket,
            OperationInput::capture(&ui),
            Action::Logs("edge".into()),
            None,
        )
        .await;
        let JobResult::Logs(mut viewer) = result else {
            panic!("Server log viewer did not open: {}", result.message());
        };
        server.await.unwrap();
        loop {
            assert!(viewer.poll().is_none());
            if let Some(cursor) = viewer.cursor {
                assert_eq!(cursor.offset, 20);
                assert!(viewer
                    .source
                    .ends_with("logs/server-edge/20260101T000000.000Z.log"));
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(matches!(
            viewer.key(crate::input::key(event::KeyEvent::new(
                KeyCode::Char('L'),
                KeyModifiers::NONE
            ))),
            logs::Command::None
        ));
        assert!(viewer.browsing());
        viewer.stop();
    })
    .await
    .unwrap();
    astra_platform::platform::remove_socket(&socket);
}

#[test]
fn late_document_is_cached_without_replacing_a_newer_view_or_save_baseline() {
    let mut ui = Ui::new(Session::default());
    ui.open_draft("proxy".into(), "standalone=true\n".into());
    ui.remember_draft();
    let prepared = OperationInput::capture(&ui)
        .prepare("proxy".into(), "standalone=true\n# external\n".into());
    ui.editor.as_mut().unwrap().original = "standalone=true\n# newly saved\n".into();
    merge_observation(&mut ui, &prepared);
    assert!(!ui.drafts.values().next().unwrap().conflict);
    let newer = ui.editor.as_ref().unwrap().original.clone();
    ui.apply_operation(
        JobResult::Document {
            prepared,
            binding: None,
        },
        &Action::Read("proxy".into()),
        ApplyContext {
            same: false,
            log_target_current: true,
            background: true,
        },
    );
    assert_eq!(ui.editor.as_ref().unwrap().original, newer);
    assert!(ui.ready_editors.contains_key("proxy"));
    ui.apply_operation(
        JobResult::Done {
            message: "saved".into(),
            rows: None,
            quit: false,
        },
        &Action::Save {
            name: "proxy".into(),
            original: "standalone=true\n".into(),
            text: "standalone=false\n".into(),
        },
        ApplyContext {
            same: true,
            log_target_current: true,
            background: true,
        },
    );
    assert_eq!(ui.editor.as_ref().unwrap().original, newer);
}
