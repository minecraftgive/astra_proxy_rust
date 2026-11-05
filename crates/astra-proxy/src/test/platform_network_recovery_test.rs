use super::*;
#[test]
fn disabled_recovery_records_nothing() {
    // The switch defaults off and is never enabled inside this test binary.
    assert!(!enabled());
    ensure().unwrap();
    let lease = Lease::prepare(Resource::SystemProxy {
        path: PathBuf::from("unused"),
        snapshot: None,
    })
    .unwrap();
    assert!(lease.id.is_none());
    lease.commit().unwrap();
    lease.failed(&anyhow::anyhow!("ignored"));
    lease.complete().unwrap();
    assert!(owner().lock().unwrap().is_none());
    cleanup_status().unwrap();
}
#[test]
fn unknown_documents_are_retained() {
    let directory =
        std::env::temp_dir().join(format!("astra-network-journal-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("run.json");
    std::fs::write(&path, br#"{"version":999,"run":"run","resources":[]}"#).unwrap();
    assert!(recover(&path).is_err());
    assert!(path.exists());
    std::fs::remove_dir_all(directory).unwrap();
}
#[test]
fn live_owner_is_never_recovered() {
    let directory =
        std::env::temp_dir().join(format!("astra-network-owner-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("run.json");
    save(
        &path,
        &Journal {
            version: 1,
            run: "run".into(),
            resources: vec![],
            ..Default::default()
        },
    )
    .unwrap();
    let lock = open_lock(&path.with_extension("lock")).unwrap();
    lock.lock().unwrap();
    assert!(recover(&path).is_err());
    assert!(path.exists());
    drop(lock);
    recover(&path).unwrap();
    assert!(!path.exists());
    std::fs::remove_dir_all(directory).unwrap();
}
#[test]
fn recovery_is_reverse_order_and_retains_only_failed_resources() {
    let directory =
        std::env::temp_dir().join(format!("astra-network-steps-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("run.json");
    let mut journal = Journal {
        version: 1,
        run: "run".into(),
        resources: (0..3)
            .map(|i| Item {
                id: i.to_string(),
                committed: true,
                resource: Resource::SystemProxy {
                    path: PathBuf::from("unused"),
                    snapshot: None,
                },
                error: None,
            })
            .collect(),
        ..Default::default()
    };
    save(&path, &journal).unwrap();
    let mut attempted = vec![];
    restore_items(&path, &mut journal, |item| {
        attempted.push(item.id.clone());
        ensure!(item.id != "1", "injected restore error");
        Ok(())
    })
    .unwrap();
    assert_eq!(attempted, ["2", "1", "0"]);
    let persisted: Journal = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(persisted.resources.len(), 1);
    assert_eq!(persisted.resources[0].id, "1");
    assert!(persisted.resources[0]
        .error
        .as_ref()
        .unwrap()
        .contains("injected"));
    restore_items(&path, &mut journal, |_| Ok(())).unwrap();
    assert!(journal.resources.is_empty());
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
#[ignore = "subprocess entrypoint"]
fn guardian_test_child() {
    if let Some(path) = std::env::var_os("ASTRA_GUARDIAN_TEST_PATH") {
        guardian(Path::new(&path)).unwrap();
        std::process::exit(0);
    }
}
#[test]
#[ignore = "subprocess entrypoint"]
fn owner_test_child() {
    let Some(path) = std::env::var_os("ASTRA_GUARDIAN_OWNER_TEST_PATH") else {
        return;
    };
    let path = PathBuf::from(path);
    let lock = open_lock(&path.with_extension("lock")).unwrap();
    lock.lock().unwrap();
    let mut journal: Journal = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    journal.owner = Some(astra_platform::lifecycle::capture(std::process::id()).unwrap());
    journal.boot_id = astra_platform::lifecycle::boot_id().to_owned();
    save(&path, &journal).unwrap();
    // Deliberately never waited: this owner process parks until it is killed.
    #[allow(clippy::zombie_processes)]
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "platform::network_recovery::tests::guardian_test_child",
            "--ignored",
            "--nocapture",
        ])
        .env("ASTRA_GUARDIAN_TEST_PATH", &path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let input = child.stdin.take().unwrap();
    let output = child.stdout.take().unwrap();
    for line in std::io::BufReader::new(output).lines() {
        if line.unwrap().contains("READY") {
            println!("OWNER_READY");
            std::io::stdout().flush().unwrap();
            break;
        }
    }
    std::hint::black_box((&lock, &input, &child));
    loop {
        std::thread::park();
    }
}
#[test]
fn guardian_survives_forced_owner_exit_without_network_mutation() {
    let directory =
        std::env::temp_dir().join(format!("astra-network-crash-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("run.json");
    save(
        &path,
        &Journal {
            version: 1,
            run: "run".into(),
            resources: vec![],
            ..Default::default()
        },
    )
    .unwrap();
    let mut owner = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "platform::network_recovery::tests::owner_test_child",
            "--ignored",
            "--nocapture",
        ])
        .env("ASTRA_GUARDIAN_OWNER_TEST_PATH", &path)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let output = owner.stdout.take().unwrap();
    let (sender, ready) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(output).lines() {
            if line.unwrap_or_default().contains("OWNER_READY") {
                let _ = sender.send(());
                break;
            }
        }
    });
    let started = ready.recv_timeout(Duration::from_secs(10));
    let _ = owner.kill();
    let _ = owner.wait();
    started.unwrap();
    let until = std::time::Instant::now() + Duration::from_secs(5);
    while path.exists() && std::time::Instant::now() < until {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !path.exists(),
        "independent guardian did not finish recovery after owner was killed"
    );
    std::fs::remove_dir_all(directory).unwrap();
}
#[test]
fn hard_linked_lock_is_rejected_without_modifying_target() {
    let directory =
        std::env::temp_dir().join(format!("astra-network-links-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    let target = directory.join("target");
    let link = directory.join("lock");
    std::fs::write(&target, b"keep").unwrap();
    std::fs::hard_link(&target, &link).unwrap();
    assert!(open_lock(&link).is_err());
    assert_eq!(std::fs::read(&target).unwrap(), b"keep");
    std::fs::remove_dir_all(directory).unwrap();
}
#[test]
fn failed_checkpoints_do_not_skip_remaining_network_restoration() {
    let mut journal = Journal {
        version: 1,
        run: "run".into(),
        resources: (0..3)
            .map(|i| Item {
                id: i.to_string(),
                committed: true,
                resource: Resource::SystemProxy {
                    path: PathBuf::from("unused"),
                    snapshot: None,
                },
                error: None,
            })
            .collect(),
        ..Default::default()
    };
    let original = serde_json::to_vec(&journal).unwrap();
    let mut restored = vec![];
    let result = restore_items_with(
        &mut journal,
        |item| {
            restored.push(item.id.clone());
            Ok(())
        },
        |_| anyhow::bail!("injected disk full"),
    );
    assert!(result.is_err());
    assert_eq!(restored, ["2", "1", "0"]);
    assert!(journal.checkpoint_error.is_some());
    let durable: Journal = serde_json::from_slice(&original).unwrap();
    assert_eq!(durable.resources.len(), 3);
    restore_items_with(
        &mut journal,
        |_| panic!("already-restored operation was repeated"),
        |_| Ok(()),
    )
    .unwrap();
    assert!(journal.checkpoint_error.is_none());
}
#[test]
fn failed_commit_or_release_checkpoint_retains_resource_ownership() {
    let mut journal = Journal {
        resources: vec![Item {
            id: "route".into(),
            committed: false,
            resource: Resource::SystemProxy {
                path: PathBuf::from("unused"),
                snapshot: None,
            },
            error: None,
        }],
        ..Default::default()
    };
    assert!(change_journal(
        &mut journal,
        |j| {
            j.resources[0].committed = true;
            Ok(())
        },
        |_| anyhow::bail!("injected commit failure")
    )
    .is_err());
    assert!(!journal.resources[0].committed);
    assert!(journal.checkpoint_error.is_some());
    assert!(change_journal(
        &mut journal,
        |j| {
            j.resources.clear();
            Ok(())
        },
        |_| anyhow::bail!("injected cleanup failure")
    )
    .is_err());
    assert_eq!(journal.resources.len(), 1);
    change_journal(
        &mut journal,
        |j| {
            j.resources.clear();
            Ok(())
        },
        |_| Ok(()),
    )
    .unwrap();
    assert!(journal.resources.is_empty());
    assert!(journal.checkpoint_error.is_none());
}
