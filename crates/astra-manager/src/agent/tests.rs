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

#[tokio::test]
async fn server_logs_remain_readable_without_log_level_control() {
    let root = tempfile::tempdir().unwrap();
    let config = ManagerConfig {
        state_dir: root.path().join("state"),
        config_dir: root.path().join("config"),
        bin_dir: root.path().into(),
        ..Default::default()
    };
    let agent = AgentService::new(config.clone()).unwrap();
    let native = agent.start_native().await.unwrap();
    let path = config.config_dir.join("server.toml");
    std::fs::write(&path, "# registered server fixture").unwrap();
    agent.store.add_server("server", &path).await.unwrap();
    let identity = agent.store.identity("server").await.unwrap();
    let ResponseBody::Capabilities(capabilities) = agent
        .query(Query::Capabilities {
            instance: identity.clone(),
        })
        .await
        .unwrap()
    else {
        panic!("Expected server capabilities");
    };
    assert!(capabilities.iter().any(|c| c == "logs"));
    assert!(!capabilities.iter().any(|c| c == "log_level"));
    for body in [
        RequestBody::Query(Query::LogLevel {
            instance: identity.clone(),
        }),
        RequestBody::Execute {
            operation_id: "server-capture-level".into(),
            command: Command::SetLogLevel {
                instance: identity.clone(),
                level: old::LogLevel::Debug,
            },
        },
    ] {
        let response = agent.handle(Request::new("unsupported-level", body)).await;
        let error = response.result.unwrap_err();
        assert!(matches!(error.code, ErrorCode::Unsupported));
        assert!(error.message.contains("log capture level"));
        assert!(!error.message.contains("Unknown instance"));
    }
    let text = b"Server ready: saved log remains available\n";
    std::fs::write(config.state_dir.join("server-server.log"), text).unwrap();
    let ResponseBody::LogChunk(LogChunk::Data { bytes, .. }) = agent
        .query(Query::LogChunk {
            instance: identity,
            cursor: None,
        })
        .await
        .unwrap()
    else {
        panic!("Stopped servers must retain access to their log file");
    };
    assert_eq!(bytes.as_slice(), text);
    agent.call(old::Action::Shutdown).await.unwrap();
    native.await.unwrap().unwrap();
}

#[tokio::test]
#[ignore = "requires freshly built server core binary"]
async fn live_server_prepare_starts_existing_profile_and_streams_logs() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let binaries = std::env::var_os("ASTRA_UPDATE_TEST_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_exe()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .to_owned()
        });
    assert!(binaries
        .join(astra_platform::platform::executable("astra-server-core"))
        .is_file());
    #[cfg(unix)]
    let root = tempfile::Builder::new().prefix("asl-").tempdir_in("/tmp");
    #[cfg(not(unix))]
    let root = tempfile::Builder::new().prefix("asl-").tempdir();
    let root = root.unwrap();
    let config = ManagerConfig {
        state_dir: root.path().join("state"),
        config_dir: root.path().join("config"),
        bin_dir: binaries,
        runtime_dir: Some(root.path().join("runtime")),
        ..Default::default()
    };
    let agent = AgentService::new(config.clone()).unwrap();
    let native = agent.start_native().await.unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let path = config.config_dir.join("server.toml");
    let text = format!("shutdown_timeout_secs=0\n[[listeners]]\ntag='local'\nprotocol='socks5'\nlisten='{address}'\n[[listeners.users]]\nname='test'\npassword='local-test-password'\n");
    std::fs::write(&path, &text).unwrap();
    drop(listener);
    agent.store.add_server("server", &path).await.unwrap();
    let instance = agent.store.identity("server").await.unwrap();
    let result: Result<()> = async {
        let prepared = agent
            .execute(
                "prepare-server".into(),
                Command::PrepareConfig {
                    instance: instance.clone(),
                },
            )
            .await?;
        anyhow::ensure!(
            matches!(prepared, ResponseBody::Done { message } if message.is_empty()),
            "A valid existing server profile must not be returned as a new document"
        );
        anyhow::ensure!(
            std::fs::read_to_string(&path)? == text,
            "Preparation changed the profile"
        );
        agent
            .execute(
                "start-server".into(),
                Command::Start {
                    instance: instance.clone(),
                },
            )
            .await?;
        let mut connection = tokio::net::TcpStream::connect(address).await?;
        connection.write_all(&[5, 1, 2]).await?;
        let mut method = [0; 2];
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            connection.read_exact(&mut method),
        )
        .await??;
        anyhow::ensure!(
            method == [5, 2],
            "Server did not negotiate SOCKS authentication"
        );
        drop(connection);
        let logs = agent
            .query(Query::LogChunk {
                instance: instance.clone(),
                cursor: None,
            })
            .await?;
        anyhow::ensure!(
            matches!(logs, ResponseBody::LogChunk(LogChunk::Data { bytes, .. })
            if String::from_utf8_lossy(&bytes).contains("Server ready: 1 listeners")),
            "Server startup log was not streamed"
        );
        Ok(())
    }
    .await;
    // Cleanup runs even when an assertion above fails, so a failed live test
    // does not leave an isolated core listening after the test process exits.
    let stop = agent
        .execute("stop-server".into(), Command::Stop { instance })
        .await;
    let shutdown = agent.call(old::Action::Shutdown).await;
    native.await.unwrap().unwrap();
    result.unwrap();
    stop.unwrap();
    shutdown.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while agent.server_endpoint("server").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
