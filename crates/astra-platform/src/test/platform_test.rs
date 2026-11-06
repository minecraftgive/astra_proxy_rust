use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{timeout, Duration};

#[test]
fn home_directory_resolves_with_service_environment_and_preserves_overrides() {
    const CASE: &str = "ASTRA_TEST_HOME_DIRECTORY_CASE";
    const EXPECTED: &str = "ASTRA_TEST_HOME_DIRECTORY_EXPECTED";
    if let Some(case) = std::env::var_os(CASE) {
        let expected = PathBuf::from(std::env::var_os(EXPECTED).unwrap());
        assert_eq!(home_dir().unwrap(), expected, "case: {case:?}");
        return;
    }

    let root = std::env::temp_dir().join("astra home 中文");
    let local = root.join("local");
    let profile = root.join("profile");
    let known = known_folder_dir(
        &windows_sys::Win32::UI::Shell::FOLDERID_LocalAppData,
        "local application data",
    )
    .unwrap();
    assert!(known.is_absolute());
    for (case, expected) in [
        ("local-preferred", &local),
        ("profile-only", &profile),
        ("service-environment", &known),
    ] {
        // Isolate environment changes and match the privilege service's
        // restricted environment, without inheriting a cached folder lookup.
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        background_command(&mut command, false);
        command.env_clear();
        for key in ["PATH", "SystemRoot", "WINDIR"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        command
            .args([
                "--exact",
                "platform::tests::home_directory_resolves_with_service_environment_and_preserves_overrides",
                "--nocapture",
            ])
            .env(CASE, case)
            .env(EXPECTED, expected)
            // Per-user paths such as the system proxy lock must not depend
            // on which runtime directory a particular manager selected.
            .env("ASTRA_RUNTIME_DIR", root.join("runtime"));
        if case == "local-preferred" {
            command.env("LOCALAPPDATA", &local);
        }
        if matches!(case, "local-preferred" | "profile-only") {
            command.env("USERPROFILE", &profile);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{case}:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[tokio::test]
async fn forced_recovery_preserves_live_pipes_links_and_nonmarkers() {
    let directory = std::env::temp_dir().join(format!(
        "astra-force-recovery-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    crate::elevation::create_private_directory(&directory).unwrap();
    let path = directory.join("control.sock");
    assert!(!force_recover_control_endpoint(&path).unwrap());
    std::fs::write(&path, b"keep").unwrap();
    assert!(force_recover_control_endpoint(&path).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"keep");
    std::fs::remove_file(&path).unwrap();

    let original = directory.join("original");
    std::fs::write(&original, b"").unwrap();
    std::fs::hard_link(&original, &path).unwrap();
    assert!(force_recover_control_endpoint(&path).is_err());
    assert!(path.exists() && original.exists());
    std::fs::remove_file(&path).unwrap();
    std::fs::remove_file(&original).unwrap();

    let listener = LocalListener::bind(&path).unwrap();
    assert!(force_recover_control_endpoint(&path).is_err());
    assert!(path.exists());
    drop(listener);
    std::fs::write(&path, b"").unwrap();
    assert!(force_recover_control_endpoint(&path).unwrap());
    assert!(!path.exists());
    drop(LocalListener::bind(&path).unwrap());
    std::fs::remove_dir(directory).unwrap();
}

#[tokio::test]
async fn forced_recovery_handles_nested_administrator_owned_markers() {
    if !crate::elevation::is_elevated().unwrap() {
        return;
    }
    let root = std::env::temp_dir().join(format!(
        "astra-force-owner-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    crate::elevation::create_private_directory(&root).unwrap();
    let directory = root.join("tui");
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("control.sock");
    std::fs::write(&path, b"").unwrap();
    // UAC's default owner is Administrators. The directory passes the
    // normal user-object policy through its user-owned parent, while the
    // marker's immediate parent is Administrators and fails that policy.
    let file = std::fs::File::open(&path).unwrap();
    let rejected = crate::elevation::validate_user_file(&file, &path).is_err();
    drop(file);
    if rejected {
        assert!(recover_stale_endpoint(&path).is_err());
        assert!(path.exists());
        assert!(force_recover_control_endpoint(&path).unwrap());
        assert!(!path.exists());
    } else {
        // Some elevated tokens retain the concrete user as default owner.
        assert!(recover_stale_endpoint(&path).unwrap());
    }
    let listener = LocalListener::bind(&path).unwrap();
    let file = std::fs::File::open(&path).unwrap();
    crate::elevation::validate_user_file(&file, &path).unwrap();
    drop(file);
    drop(listener);
    std::fs::remove_dir(directory).unwrap();
    std::fs::remove_dir(root).unwrap();
}

#[tokio::test]
async fn recovering_bind_handles_stale_markers_without_replacing_live_endpoints() {
    let directory =
        std::env::temp_dir().join(format!("astra-core-recovery-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("rule.sock");
    std::fs::write(&path, b"").unwrap();
    let listener = bind_listener(&path).unwrap();
    assert!(path.exists());
    assert!(bind_listener(&path).is_err());
    assert!(path.exists());
    drop(listener);
    std::fs::write(&path, b"preserve").unwrap();
    assert!(bind_listener(&path).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"preserve");
    std::fs::remove_file(path).unwrap();
    std::fs::remove_dir(directory).unwrap();
}

#[tokio::test]
async fn manager_recovery_only_removes_abandoned_empty_markers() {
    let directory =
        std::env::temp_dir().join(format!("astra-manager-recovery-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("manager.sock");
    assert!(!recover_stale_manager_endpoint(&path).unwrap());
    std::fs::write(&path, b"preserve this file").unwrap();
    assert!(recover_stale_manager_endpoint(&path).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"preserve this file");
    std::fs::remove_file(&path).unwrap();
    let original = directory.join("original");
    std::fs::write(&original, b"").unwrap();
    std::fs::hard_link(&original, &path).unwrap();
    assert!(recover_stale_manager_endpoint(&path).is_err());
    assert!(path.exists() && original.exists());
    std::fs::remove_file(&path).unwrap();
    std::fs::remove_file(&original).unwrap();
    let listener = LocalListener::bind(&path).unwrap();
    assert!(recover_stale_manager_endpoint(&path).is_err());
    assert!(path.exists());
    drop(listener);
    // An unclean process exit leaves just this marker, with no pipe server.
    std::fs::write(&path, b"").unwrap();
    assert!(recover_stale_manager_endpoint(&path).unwrap());
    assert!(!path.exists());
    let listener = LocalListener::bind(&path).unwrap();
    drop(listener);
    std::fs::remove_dir(directory).unwrap();
}

#[tokio::test]
async fn named_pipe_accept_cancellation_concurrency_and_cleanup() {
    let directory = std::env::temp_dir().join(format!("astra-ipc-中文 {}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("local.sock");
    let listener = LocalListener::bind(&path).unwrap();
    assert!(LocalListener::bind(&path).is_err());
    assert!(timeout(Duration::from_millis(20), listener.accept())
        .await
        .is_err());
    let server = tokio::spawn(async move {
        let mut workers = tokio::task::JoinSet::new();
        for _ in 0..16 {
            let (mut stream, _) = listener.accept().await.unwrap();
            workers.spawn(async move {
                let mut data = vec![0u8; 65536];
                stream.read_exact(&mut data).await.unwrap();
                stream.write_all(&data).await.unwrap();
            });
        }
        while let Some(result) = workers.join_next().await {
            result.unwrap();
        }
    });
    let mut clients = tokio::task::JoinSet::new();
    for value in 0..16u8 {
        let path = path.clone();
        clients.spawn(async move {
            let mut stream = LocalStream::connect(path).await.unwrap();
            let payload = vec![value; 65536];
            stream.write_all(&payload).await.unwrap();
            let mut echoed = vec![0u8; payload.len()];
            stream.read_exact(&mut echoed).await.unwrap();
            assert_eq!(payload, echoed);
        });
    }
    timeout(Duration::from_secs(10), async {
        while let Some(result) = clients.join_next().await {
            result.unwrap();
        }
        server.await.unwrap();
    })
    .await
    .unwrap();
    assert!(!path.exists());
    std::fs::remove_dir(directory).unwrap();
}
