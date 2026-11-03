use super::*;
#[test]
fn established_identity_database_cannot_be_silently_recreated() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    std::fs::create_dir(&state).unwrap();
    std::fs::write(state.join("management-state-v2"), b"2\n").unwrap();
    let result = AgentService::new(ManagerConfig {
        state_dir: state.clone(),
        config_dir: root.path().join("config"),
        ..Default::default()
    });
    assert!(result.is_err());
    assert!(!state.join("management.sqlite").exists());
}
#[tokio::test]
async fn in_process_actor_has_no_legacy_endpoint_and_remote_elevation_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let config = ManagerConfig {
        state_dir: root.path().join("state"),
        config_dir: root.path().join("config"),
        bin_dir: root.path().into(),
        ..Default::default()
    };
    let agent = AgentService::new(config.clone()).unwrap();
    let native = agent.start_native().await.unwrap();
    assert!(!config.state_dir.join("manager.sock").exists());
    let reply = agent
        .handle(Request::new("list", RequestBody::Query(Query::Instances)))
        .await;
    assert!(matches!(reply.result,Ok(ResponseBody::Instances(items))if items.is_empty()));
    let reply = agent
        .handle_remote(Request::new(
            "local",
            RequestBody::Local(LocalCommand::EnableTun {
                instance: "untrusted".into(),
                expected_pid: None,
            }),
        ))
        .await;
    assert!(matches!(
        reply.result,
        Err(ApiError {
            code: ErrorCode::RequiresLocalAction,
            ..
        })
    ));
    let reply = agent
        .handle(Request::new(
            "shutdown",
            RequestBody::Execute {
                operation_id: "stop-once".into(),
                command: Command::Shutdown,
            },
        ))
        .await;
    assert!(reply.result.is_ok());
    tokio::time::timeout(std::time::Duration::from_secs(3), native)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn draft_schema_uses_current_document_and_stale_apply_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let config = ManagerConfig {
        state_dir: root.path().join("state"),
        config_dir: root.path().join("config"),
        bin_dir: root.path().into(),
        ..Default::default()
    };
    let agent = AgentService::new(config.clone()).unwrap();
    let text = astra_app::Document::template(astra_model::CoreKind::Server);
    let path = config.config_dir.join("source.toml");
    std::fs::write(&path, &text).unwrap();
    agent.store.add_server("server", &path).await.unwrap();
    let opened = agent
        .draft(DraftCommand::Open {
            instance: "server".into(),
            draft_id: "shared".into(),
        })
        .await
        .unwrap();
    let ResponseBody::Draft(draft) = opened else {
        panic!()
    };
    let updated = agent
        .draft(DraftCommand::Edit {
            draft_id: draft.id.clone(),
            expected_revision: draft.revision.clone(),
            text: format!("# retained comment\n{text}"),
        })
        .await
        .unwrap();
    assert!(matches!(updated, ResponseBody::Draft(_)));
    assert!(agent
        .draft(DraftCommand::Edit {
            draft_id: draft.id.clone(),
            expected_revision: draft.revision,
            text: "bad".into()
        })
        .await
        .unwrap_err()
        .to_string()
        .starts_with("Conflict:"));
    assert!(matches!(
        agent
            .draft(DraftCommand::Schema {
                draft_id: draft.id.clone(),
                path: vec![]
            })
            .await
            .unwrap(),
        ResponseBody::Schema(_)
    ));
    assert_eq!(std::fs::read_to_string(path).unwrap(), text);
    let identity = draft.instance;
    drop(agent);
    let restored = AgentService::new(config).unwrap();
    assert_eq!(restored.store.identity("server").await.unwrap(), identity);
    assert!(restored
        .store
        .draft("shared")
        .await
        .unwrap()
        .text
        .starts_with("# retained comment"));
}
