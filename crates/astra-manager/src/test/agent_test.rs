use super::*;

#[cfg(windows)]
#[tokio::test]
async fn execute_tun_carries_no_uac_policy_and_accepts_the_service_replacement_pid() {
    let root = tempfile::tempdir().unwrap();
    let agent = AgentService::new(ManagerConfig {
        state_dir: root.path().join("state"),
        config_dir: root.path().join("config"),
        bin_dir: root.path().join("bin"),
        ..Default::default()
    })
    .unwrap();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = seen.clone();
    let (client, actor) = crate::manager::NativeClient::with_test_runtime_actor(
        root.path().join("actor"),
        move |action, allow_uac| {
            let old::Action::Runtime {
                name,
                command: astra_model::runtime::Command::Tun(true),
                expected_pid,
            } = action
            else {
                anyhow::bail!("unexpected actor action");
            };
            recorded
                .lock()
                .unwrap()
                .push((name.clone(), expected_pid, allow_uac));
            if name == "missing-service" {
                anyhow::bail!("Windows privilege service is not registered");
            }
            Ok(old::Reply::text(serde_json::to_string(
                &astra_model::runtime::Snapshot {
                    pid: 456,
                    tun: true,
                    ..Default::default()
                },
            )?))
        },
    )
    .unwrap();
    assert!(agent.native.set(client).is_ok());

    for (id, instance) in [("enabled", "proxy"), ("missing", "missing-service")] {
        let response = agent
            .handle_remote(Request::new(
                id,
                RequestBody::Execute {
                    operation_id: format!("tun-{id}"),
                    command: Command::Runtime {
                        instance: instance.into(),
                        command: RuntimeCommand::Tun(true),
                        expected_pid: Some(123),
                    },
                },
            ))
            .await;
        if instance == "proxy" {
            let ResponseBody::Runtime(RuntimeSnapshot::Client(snapshot)) = response.result.unwrap()
            else {
                panic!("expected runtime snapshot");
            };
            assert_eq!(snapshot["pid"], "456");
            assert_eq!(snapshot["tun"], true);
        } else {
            assert!(matches!(
                response.result,
                Err(ApiError {
                    code: ErrorCode::RequiresLocalAction,
                    ..
                })
            ));
        }
    }

    let local = || {
        Request::new(
            "local-tun",
            RequestBody::Local(LocalCommand::EnableTun {
                instance: "proxy".into(),
                expected_pid: Some(123),
            }),
        )
    };
    assert!(agent.handle(local()).await.result.is_ok());
    assert!(matches!(
        agent.handle_remote(local()).await.result,
        Err(ApiError {
            code: ErrorCode::RequiresLocalAction,
            ..
        })
    ));
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            ("proxy".into(), Some(123), false),
            ("missing-service".into(), Some(123), false),
            ("proxy".into(), Some(123), true),
        ]
    );
    drop(agent);
    tokio::time::timeout(std::time::Duration::from_secs(1), actor)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn remote_instance_metadata_refreshes_per_config_auto_start_for_every_core_kind() {
    let root = tempfile::tempdir().unwrap();
    let config = ManagerConfig {
        state_dir: root.path().join("state"),
        config_dir: root.path().join("config"),
        bin_dir: root.path().into(),
        runtime_dir: Some(root.path().join("runtime")),
        ..Default::default()
    };
    let agent = AgentService::new(config.clone()).unwrap();
    #[derive(serde::Serialize)]
    struct RegistryFixture {
        instances: Vec<old::Instance>,
    }
    let instances: Vec<_> = [("proxy", old::Kind::Proxy), ("rule", old::Kind::Rule)]
        .into_iter()
        .map(|(name, kind)| old::Instance {
            name: name.into(),
            kind,
            config: config.config_dir.join(format!("{name}.toml")),
        })
        .collect();
    for instance in &instances {
        std::fs::write(&instance.config, "auto_start = true\n").unwrap();
    }
    let mut paths: Vec<_> = instances.iter().map(|i| i.config.clone()).collect();
    std::fs::write(
        config.config_dir.join("instances.toml"),
        toml::to_string(&RegistryFixture { instances }).unwrap(),
    )
    .unwrap();
    let native = agent.start_native().await.unwrap();
    let server_path = config.config_dir.join("server.toml");
    std::fs::write(&server_path, "auto_start = true\n").unwrap();
    agent
        .store
        .add_server("server", &server_path)
        .await
        .unwrap();
    paths.push(server_path);

    for (text, expected, unavailable) in [
        (Some("auto_start = true\n"), true, false),
        (Some("auto_start = false\n"), false, false),
        (Some("# no opt-in\n"), false, false),
        (Some("auto_start = 'true'\n"), false, false),
        (Some("[nested]\nauto_start = true\n"), false, false),
        (Some("auto_start = true\ninvalid = [\n"), false, true),
        (None, false, true),
    ] {
        for path in &paths {
            if let Some(text) = text {
                std::fs::write(path, text).unwrap();
            } else {
                std::fs::remove_file(path).unwrap();
            }
        }
        let response = agent
            .handle_remote(Request::new(
                "instances",
                RequestBody::Query(Query::Instances),
            ))
            .await;
        // Controllers and Web clients receive the same v2 JSON envelope.
        let json = serde_json::to_vec(&response).unwrap();
        let response: Response = serde_json::from_slice(&json).unwrap();
        let ResponseBody::Instances(rows) = response.result.unwrap() else {
            panic!("Expected instance list");
        };
        assert_eq!(rows.len(), 3);
        for row in rows {
            let details = row.details.unwrap();
            assert_eq!(details.auto_start, expected, "{}: {text:?}", row.name);
            assert_eq!(
                details.config_unavailable, unavailable,
                "{}: {text:?}",
                row.name
            );
            assert!(row.pid.is_none());
        }
    }
    agent.call(old::Action::Shutdown).await.unwrap();
    native.await.unwrap().unwrap();
}

#[tokio::test]
async fn monitor_queries_resolve_instance_ids_and_are_readonly_for_rule_and_server() {
    let root = tempfile::tempdir().unwrap();
    let config = ManagerConfig {
        state_dir: root.path().join("state"),
        config_dir: root.path().join("config"),
        bin_dir: root.path().into(),
        runtime_dir: Some(root.path().join("runtime")),
        ..Default::default()
    };
    let agent = AgentService::new(config.clone()).unwrap();
    let rule_path = config.config_dir.join("rule-monitor.toml");
    std::fs::write(
        &rule_path,
        astra_app::Document::template(astra_model::CoreKind::Rule),
    )
    .unwrap();
    #[derive(serde::Serialize)]
    struct RegistryFixture {
        instances: Vec<old::Instance>,
    }
    std::fs::write(
        config.config_dir.join("instances.toml"),
        toml::to_string(&RegistryFixture {
            instances: vec![old::Instance {
                name: "rule-monitor".into(),
                kind: old::Kind::Rule,
                config: rule_path,
            }],
        })
        .unwrap(),
    )
    .unwrap();
    let native = agent.start_native().await.unwrap();
    let server_path = config.config_dir.join("server-monitor.toml");
    std::fs::create_dir_all(&config.config_dir).unwrap();
    std::fs::write(&server_path, "# monitor fixture").unwrap();
    agent
        .store
        .add_server("server-monitor", &server_path)
        .await
        .unwrap();
    let instances = agent.instances().await.unwrap();
    assert_eq!(instances.len(), 2);
    for instance in instances {
        assert_ne!(
            instance.id, instance.name,
            "Fixture must exercise durable UUID addressing"
        );
        assert!(instance.capabilities.iter().any(|c| c == "monitor"));
        let response = agent
            .handle_remote(Request::new(
                "monitor",
                RequestBody::Query(Query::Monitor {
                    instance: instance.id.clone(),
                }),
            ))
            .await;
        let ResponseBody::Monitor(sample) = response.result.unwrap() else {
            panic!("Expected monitor response");
        };
        assert_eq!(sample.instance, instance.id);
        assert_eq!(sample.pid, None);
        assert_eq!(sample.cpu_percent, None);
        assert_eq!(sample.upload_bytes, None);
        assert_eq!(sample.error, None);
    }
    assert!(
        matches!(agent.query(Query::Tasks { limit: None }).await.unwrap(), ResponseBody::Tasks(tasks) if tasks.is_empty())
    );
    assert!(agent
        .query(Query::Monitor {
            instance: "unknown-monitor".into()
        })
        .await
        .is_err());
    agent.call(old::Action::Shutdown).await.unwrap();
    native.await.unwrap().unwrap();
}

#[tokio::test]
async fn failed_final_checkpoint_is_queryable_as_unknown_and_never_replayed() {
    let root = tempfile::tempdir().unwrap();
    let config = ManagerConfig {
        state_dir: root.path().join("state"),
        config_dir: root.path().join("config"),
        bin_dir: root.path().into(),
        ..Default::default()
    };
    let agent = AgentService::new(config.clone()).unwrap();
    let command = Command::Shutdown;
    let fingerprint = store::revision(&serde_json::to_string(&command).unwrap());
    agent.store.begin("once", &fingerprint).await.unwrap();
    let connection =
        rusqlite::Connection::open(config.state_dir.join("management.sqlite")).unwrap();
    connection.execute_batch("CREATE TRIGGER fail_finish BEFORE UPDATE ON operations BEGIN SELECT RAISE(FAIL, 'injected disk failure'); END;").unwrap();
    let result = Ok(ResponseBody::Done {
        message: "side effect completed".into(),
    });
    assert!(agent
        .finish_operation("once", &result)
        .await
        .unwrap_err()
        .to_string()
        .contains("outcome unknown for operation once"));
    let response = agent
        .handle(Request::new(
            "query",
            RequestBody::Query(Query::Task {
                operation_id: "once".into(),
            }),
        ))
        .await;
    assert!(matches!(
        response.result,
        Ok(ResponseBody::Task(Task {
            state: TaskState::OutcomeUnknown,
            ..
        }))
    ));
    // No native actor exists: replaying Shutdown would fail instead of returning the original uncertainty.
    assert!(matches!(
        agent.execute("once".into(), command).await.unwrap(),
        ResponseBody::Task(Task {
            state: TaskState::OutcomeUnknown,
            ..
        })
    ));
    connection
        .execute_batch("DROP TRIGGER fail_finish;")
        .unwrap();
}
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
