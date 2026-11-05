use super::*;

fn replacement_manager(root: &Path) -> Manager {
    let home = root.join("state");
    private_dir(&home).unwrap();
    private_dir(&home.join("processes")).unwrap();
    Manager::load_with_runtime(
        home,
        root.join("config"),
        root.into(),
        Some(root.join("runtime")),
    )
    .unwrap()
}

#[tokio::test]
async fn force_session_replacement_retains_records_until_endpoint_is_available() {
    let root = tempfile::tempdir().unwrap();
    let mut manager = replacement_manager(root.path());
    manager.registry.instances.push(Instance {
        name: "proxy".into(),
        kind: Kind::Proxy,
        config: root.path().join("proxy.toml"),
    });
    let record = manager.process_record("proxy");
    fs::write(&record, b"invalid previous session").unwrap();
    manager.recover_processes().await.unwrap();
    assert!(manager.instance("proxy").is_err());

    let socket = manager.socket("proxy");
    let listener = UnixListener::bind(&socket).unwrap();
    let error = manager.replace_session("proxy").unwrap_err();
    assert!(format!("{error:#}").contains("identity record was retained"));
    assert!(manager.blocked.contains_key("proxy"));
    assert_eq!(fs::read(&record).unwrap(), b"invalid previous session");
    assert!(socket.exists());
    #[cfg(unix)]
    {
        fs::remove_file(&socket).unwrap();
        assert!(manager.replace_session("proxy").is_err());
        assert!(manager.blocked.contains_key("proxy"));
        assert_eq!(fs::read(&record).unwrap(), b"invalid previous session");
    }
    drop(listener);

    fs::write(&socket, b"unrelated data").unwrap();
    assert!(manager.replace_session("proxy").is_err());
    assert_eq!(fs::read(&socket).unwrap(), b"unrelated data");
    assert!(record.exists());
    fs::remove_file(&socket).unwrap();

    manager.replace_session("proxy").unwrap();
    assert!(!record.exists());
    assert!(!manager.blocked.contains_key("proxy"));
    assert!(!manager.exits.contains_key("proxy"));
    assert!(manager.instance("proxy").is_ok());
}

#[tokio::test]
async fn force_start_keeps_blocked_session_when_configuration_cannot_be_read() {
    let root = tempfile::tempdir().unwrap();
    let mut manager = replacement_manager(root.path());
    manager.registry.instances.push(Instance {
        name: "rules".into(),
        kind: Kind::Rule,
        config: root.path().join("missing.toml"),
    });
    let record = manager.process_record("rules");
    fs::write(&record, b"invalid previous session").unwrap();
    manager.recover_processes().await.unwrap();
    assert!(manager
        .action(Action::ForceStart("rules".into()))
        .await
        .is_err());
    assert!(manager.blocked.contains_key("rules"));
    assert_eq!(fs::read(record).unwrap(), b"invalid previous session");
}

#[test]
fn force_start_plan_includes_blocked_dependencies_without_forgetting_them() {
    let root = tempfile::tempdir().unwrap();
    let mut manager = replacement_manager(root.path());
    for (name, kind) in [("proxy", Kind::Proxy), ("rules", Kind::Rule)] {
        manager.registry.instances.push(Instance {
            name: name.into(),
            kind,
            config: root.path().join(format!("{name}.toml")),
        });
        manager.blocked.insert(name.into(), "unverified".into());
    }
    manager
        .registry
        .start_bindings
        .insert("proxy".into(), vec!["rules".into()]);
    assert!(manager.start_plan("proxy").is_err());
    assert_eq!(
        manager.start_plan_with("proxy", true).unwrap(),
        ["rules", "proxy"]
    );
    assert_eq!(manager.blocked.len(), 2);
    manager
        .registry
        .start_bindings
        .insert("rules".into(), vec!["proxy".into()]);
    assert!(manager.start_plan_with("proxy", true).is_err());
    assert!(manager.start_plan_with("missing", true).is_err());
}

#[tokio::test]
async fn restart_adopts_verified_boot_and_rejects_changed_boot_without_killing_it() {
    // Control's applied snapshot is process-global. Isolate this fixture
    // from every other manager test instead of racing their control state.
    if std::env::var_os("ASTRA_IDENTITY_FIXTURE_CHILD").is_none() {
        let output = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "manager::recovery::tests::restart_adopts_verified_boot_and_rejects_changed_boot_without_killing_it", "--nocapture"])
            .env("ASTRA_IDENTITY_FIXTURE_CHILD", "1").kill_on_drop(true).output();
        let output = timeout(Duration::from_secs(30), output)
            .await
            .unwrap()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("state");
    private_dir(&home).unwrap();
    let config_home = root.path().join("config");
    let runtime = root.path().join("runtime");
    let config = root.path().join("proxy.toml");
    fs::write(&config, "standalone=true\nmixed_listen='127.0.0.1:0'\n").unwrap();
    let config = fs::canonicalize(config).unwrap();
    let mut manager = Manager::load_with_runtime(
        home.clone(),
        config_home.clone(),
        root.path().into(),
        Some(runtime.clone()),
    )
    .unwrap();
    manager.registry.instances.push(Instance {
        name: "proxy".into(),
        kind: Kind::Proxy,
        config: config.clone(),
    });
    manager.persist().unwrap();
    let socket = manager.socket("proxy");
    let control = control::Control::bind(Some(socket.clone())).unwrap();
    control::applied(&config, "standalone=true");
    let config2 = config.clone();
    let task = tokio::spawn(async move {
        control
            .run_with_snapshot(
                |_| async { Ok(()) },
                move || {
                    let path = config2.clone();
                    async move {
                        Ok(astra_model::control::ConfigSnapshot {
                            api_version: astra_ipc::API_VERSION,
                            pid: std::process::id(),
                            path,
                            text: "standalone=true".into(),
                        })
                    }
                },
            )
            .await
    });
    let identity = control::identity(&socket).await.unwrap();
    let mut record = Record {
        version: 1,
        name: "proxy".into(),
        config,
        endpoint: socket.clone(),
        identity,
    };
    astra_platform::storage::write_json(&manager.process_record("proxy"), &record).unwrap();
    drop(manager);
    let mut restored =
        Manager::load_with_runtime(home, config_home, root.path().into(), Some(runtime)).unwrap();
    restored.recover_processes().await.unwrap();
    assert!(matches!(
        &restored.running["proxy"].child,
        ManagedChild::Reattached(_)
    ));
    // A second, live process record with an unresponsive endpoint must
    // quarantine only that instance and forbid duplicate starts.
    let stalled = restored.socket("stalled");
    let _listener = UnixListener::bind(&stalled).unwrap();
    restored.registry.instances.push(Instance {
        name: "stalled".into(),
        kind: Kind::Proxy,
        config: record.config.clone(),
    });
    let stalled_record = Record {
        version: 1,
        name: "stalled".into(),
        config: record.config.clone(),
        endpoint: stalled,
        identity: record.identity.clone(),
    };
    astra_platform::storage::write_json(&restored.process_record("stalled"), &stalled_record)
        .unwrap();
    restored.recover_processes().await.unwrap();
    assert!(restored.running.contains_key("proxy"));
    assert!(restored.blocked.contains_key("stalled"));
    assert!(restored.instance("stalled").is_err());
    assert!(restored.process_record("stalled").exists());
    record.identity.boot_id = "another-boot".into();
    astra_platform::storage::write_json(&restored.process_record("proxy"), &record).unwrap();
    restored.recover_processes().await.unwrap();
    assert!(restored.blocked["proxy"].contains("boot identity mismatch"));
    assert!(!restored.running.contains_key("proxy"));
    assert!(restored.instance("proxy").is_err());
    assert_eq!(
        control::identity(&socket).await.unwrap().process.pid,
        std::process::id()
    );
    control::request(&socket, Command::Stop).await.unwrap();
    task.await.unwrap().unwrap();
}
