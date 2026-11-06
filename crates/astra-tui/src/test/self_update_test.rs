use super::*;
use astra_update::{
    CurrentRelease, ImportOptions, ProtocolRange, ReleaseFile, ReleaseManifest, ReleaseStore,
    HOST_PROTOCOL_VERSION, TARGET,
};
use sha2::{Digest, Sha256};
use std::{fs, path::PathBuf};

fn completed() -> updates::UpdateStatus {
    updates::UpdateStatus {
        next_launch: Some(CurrentRelease {
            release_id: "new-release".into(),
            previous_release_id: None,
        }),
        operation: Some(updates::UpdateOperation {
            request: updates::UpdateRequest::new(updates::UpdateIntent::Activate {
                release_id: "new-release".into(),
            }),
            phase: updates::UpdatePhase::Complete,
            before_directory: "bundle".into(),
            target_directory: Some("updated".into()),
            release_id: Some("new-release".into()),
            runtime_committed: true,
            next_launch_saved: true,
            pending_record_saved: true,
            participants: Vec::new(),
            uncertain_preparations: Vec::new(),
            error: None,
            updated_at_ms: 1,
        }),
        ..Default::default()
    }
}

#[test]
fn only_a_completed_selected_transaction_can_restart_the_interface() {
    assert_eq!(completed_release(&completed()), Some("new-release"));
    assert!(completed_release(&updates::UpdateStatus::default()).is_none());
    for phase in [
        updates::UpdatePhase::Validating,
        updates::UpdatePhase::Staged,
        updates::UpdatePhase::Preparing,
        updates::UpdatePhase::Activating,
        updates::UpdatePhase::Resuming,
        updates::UpdatePhase::SavingSelection,
        updates::UpdatePhase::Failed,
        updates::UpdatePhase::NeedsRecovery,
        updates::UpdatePhase::Uncertain,
        updates::UpdatePhase::RestoringPrevious,
    ] {
        let mut status = completed();
        status.operation.as_mut().unwrap().phase = phase;
        assert!(completed_release(&status).is_none(), "{phase:?}");
    }
    for change in [
        |s: &mut updates::UpdateStatus| s.operation.as_mut().unwrap().runtime_committed = false,
        |s: &mut updates::UpdateStatus| s.operation.as_mut().unwrap().next_launch_saved = false,
        |s: &mut updates::UpdateStatus| {
            s.operation.as_mut().unwrap().error = Some("save failed".into())
        },
        |s: &mut updates::UpdateStatus| s.capabilities.operation_running = true,
        |s: &mut updates::UpdateStatus| {
            s.capabilities.recovery_operation_id = Some("unfinished".into())
        },
        |s: &mut updates::UpdateStatus| s.operation.as_mut().unwrap().release_id = None,
        |s: &mut updates::UpdateStatus| s.next_launch = None,
        |s: &mut updates::UpdateStatus| {
            s.next_launch.as_mut().unwrap().release_id = "another-release".into()
        },
        |s: &mut updates::UpdateStatus| {
            s.operation.as_mut().unwrap().request.intent = updates::UpdateIntent::Stage {
                package: "package.zip".into(),
            }
        },
    ] {
        let mut status = completed();
        change(&mut status);
        assert!(completed_release(&status).is_none());
    }
    let mut rollback = completed();
    rollback.operation.as_mut().unwrap().request.intent = updates::UpdateIntent::Rollback {
        target_directory: "previous".into(),
    };
    assert_eq!(completed_release(&rollback), Some("new-release"));
}

struct Fixture {
    directory: PathBuf,
    parent: PathBuf,
    store: ReleaseStore,
    context: windows::Context,
}
impl Fixture {
    fn new(server_frontend: bool) -> Self {
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let parent = fs::canonicalize(std::env::temp_dir()).unwrap();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let directory = parent.join(format!(
            "astra-self-update-{}-{nonce}-{sequence}",
            std::process::id()
        ));
        let source = directory.join("source package");
        let bundle = directory.join("old bundle");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir(&bundle).unwrap();
        let mut manifest = ReleaseManifest {
            schema_version: 1,
            role: Default::default(),
            release_id: "new-release".into(),
            target: TARGET.into(),
            host_protocol: ProtocolRange {
                min: HOST_PROTOCOL_VERSION,
                max: HOST_PROTOCOL_VERSION,
            },
            components: Default::default(),
            files: Vec::new(),
            profile: None,
            mode: None,
            cargo_profile: None,
        };
        let mut components = vec!["astra-proxy", "astra-rule", "astra-tui", "astra-host"];
        if server_frontend {
            components.push("astra-server-tui");
        }
        for component in components {
            let filename = astra_platform::platform::executable(component);
            fs::write(source.join(&filename), b"fixture").unwrap();
            fs::write(bundle.join(&filename), b"previous").unwrap();
            manifest.components.insert(component.into(), "0.2.0".into());
            manifest.files.push(ReleaseFile {
                path: filename,
                size: 7,
                sha256: format!("{:x}", Sha256::digest(b"fixture")),
            });
        }
        fs::write(
            source.join(astra_update::MANIFEST_FILE),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let home = directory.join("state");
        let store = ReleaseStore::new(&home);
        store.import(source, &ImportOptions::default()).unwrap();
        store.commit_current("new-release").unwrap();
        let context = windows::Context {
            executable: bundle.join(astra_platform::platform::executable("astra-tui")),
            home,
            config: directory.join("config"),
            bin: bundle,
            language: crate::i18n::Language::Traditional,
        };
        Self {
            directory,
            parent,
            store,
            context,
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Ok(directory) = fs::canonicalize(&self.directory) {
            assert_eq!(directory.parent(), Some(self.parent.as_path()));
            let _ = fs::remove_dir_all(directory);
        }
    }
}

#[test]
fn replacement_uses_verified_release_and_preserves_frontend_and_context() {
    let mut fixture = Fixture::new(true);
    for frontend in ["astra-tui", "astra-server-tui"] {
        fixture.context.executable = fixture
            .context
            .bin
            .join(astra_platform::platform::executable(frontend));
        let next = resolve(&fixture.context, "new-release").unwrap().unwrap();
        let expected = fs::canonicalize(fixture.store.release_dir("new-release").unwrap()).unwrap();
        assert_eq!(next.bin, expected);
        assert_eq!(
            next.executable,
            expected.join(astra_platform::platform::executable(frontend))
        );
        assert_eq!(next.home, fixture.context.home);
        assert_eq!(next.config, fixture.context.config);
        assert_eq!(next.language, fixture.context.language);
        assert!(resolve(&next, "new-release").unwrap().is_none());
    }
}

#[test]
fn resolution_rejects_stale_selection_and_corruption_in_any_component() {
    let fixture = Fixture::new(false);
    assert!(resolve(&fixture.context, "another-release").is_err());
    fs::write(
        fixture
            .store
            .release_dir("new-release")
            .unwrap()
            .join(astra_platform::platform::executable("astra-rule")),
        b"corrupt",
    )
    .unwrap();
    assert!(resolve(&fixture.context, "new-release").is_err());
    fixture.store.clear_current().unwrap();
    assert!(resolve(&fixture.context, "new-release").is_err());
}

#[test]
fn resolution_never_substitutes_another_frontend_or_restarts_the_host() {
    let mut fixture = Fixture::new(false);
    fixture
        .context
        .executable
        .set_file_name(astra_platform::platform::executable("astra-server-tui"));
    let error = resolve(&fixture.context, "new-release").err().unwrap();
    assert!(error.to_string().contains("astra-server-tui"));
    fixture
        .context
        .executable
        .set_file_name(astra_platform::platform::executable("astra-host"));
    assert!(resolve(&fixture.context, "new-release").is_err());
}

#[test]
fn restart_preserves_arguments_as_values_and_marks_a_session_resume() {
    let fixture = Fixture::new(false);
    let next = resolve(&fixture.context, "new-release").unwrap().unwrap();
    let launch = windows::Launch {
        view: Some(windows::View::Connections),
        instance: Some("proxy with spaces & punctuation".into()),
        kind: Some(crate::protocol::Kind::Rule),
        window_id: Some("existing-window".into()),
    };
    let command = restart_command(&next, &launch);
    assert_eq!(command.get_program(), next.executable.as_os_str());
    let args: Vec<_> = command.get_args().collect();
    for (flag, value) in [
        ("--state-dir", next.home.as_os_str()),
        ("--config-dir", next.config.as_os_str()),
        ("--bin-dir", next.bin.as_os_str()),
        ("--lang", std::ffi::OsStr::new("zh-TW")),
        ("--view", std::ffi::OsStr::new("connections")),
        (
            "--instance",
            std::ffi::OsStr::new("proxy with spaces & punctuation"),
        ),
        ("--core", std::ffi::OsStr::new("rule")),
        ("--window-id", std::ffi::OsStr::new("existing-window")),
    ] {
        assert!(args
            .windows(2)
            .any(|pair| pair == [std::ffi::OsStr::new(flag), value]));
    }
    assert_eq!(args.last(), Some(&std::ffi::OsStr::new("--resume-ui")));
    assert!(!args.contains(&std::ffi::OsStr::new("--manager")));
    assert!(!args.contains(&std::ffi::OsStr::new("--shutdown")));
}

#[test]
fn missing_replacement_reports_a_restart_failure() {
    let mut fixture = Fixture::new(false);
    fixture.context.executable = fixture.directory.join("missing-interface");
    let error = restart(&fixture.context, &windows::Launch::default()).unwrap_err();
    assert!(error.to_string().contains("Restart the updated interface"));
}

// The current test binary is a portable child fixture. Environment values are
// set only on Command, never on this multithreaded test process.
#[test]
fn readiness_child_process() {
    let Some(path) = std::env::var_os("ASTRA_TUI_RESTART_TEST_MARKER") else {
        return;
    };
    let path = PathBuf::from(path);
    match std::env::var("ASTRA_TUI_RESTART_TEST_MODE")
        .unwrap()
        .as_str()
    {
        "ready" => signal_ready(&path).unwrap(),
        "ready-exit" => {
            signal_ready(&path).unwrap();
            std::process::exit(0);
        }
        "invalid" => fs::write(&path, b"invalid marker").unwrap(),
        "exit" => std::process::exit(17),
        "timeout" => {}
        mode => panic!("Unknown readiness fixture mode: {mode}"),
    }
    std::thread::sleep(Duration::from_secs(30));
}

fn readiness_child(path: &Path, mode: &str) -> Child {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .arg("--exact")
        .arg("self_update::tests::readiness_child_process")
        .env("ASTRA_TUI_RESTART_TEST_MARKER", path)
        .env("ASTRA_TUI_RESTART_TEST_MODE", mode)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    astra_platform::platform::background_command(&mut command, true);
    command.spawn().unwrap()
}

#[test]
fn restart_waits_for_readiness_and_removes_the_marker() {
    let fixture = Fixture::new(false);
    let path = readiness_path(&fixture.context.home).unwrap();
    assert!(!path.exists());
    assert_ne!(path, readiness_path(&fixture.context.home).unwrap());
    let mut child = readiness_child(&path, "ready");
    let result = wait_for_ready(&mut child, &path, Duration::from_secs(5));
    let running = child.try_wait().unwrap().is_none();
    let _ = child.kill();
    child.wait().unwrap();
    result.unwrap();
    assert!(running, "A ready replacement must remain running");
    assert!(!path.exists(), "Readiness marker must be consumed");
}

#[test]
fn readiness_acknowledgement_survives_a_subsequent_frontend_exit() {
    let fixture = Fixture::new(false);
    let path = readiness_path(&fixture.context.home).unwrap();
    let mut child = readiness_child(&path, "ready-exit");
    assert!(child.wait().unwrap().success());
    wait_for_ready(&mut child, &path, Duration::from_secs(5)).unwrap();
    assert!(!path.exists());
}

#[test]
fn restart_rejects_an_early_exit_and_cleans_the_marker() {
    let fixture = Fixture::new(false);
    let path = readiness_path(&fixture.context.home).unwrap();
    let mut child = readiness_child(&path, "exit");
    let error = wait_for_ready(&mut child, &path, Duration::from_secs(5)).unwrap_err();
    assert!(error.to_string().contains("exited before becoming ready"));
    assert_eq!(child.try_wait().unwrap().unwrap().code(), Some(17));
    assert!(!path.exists());
}

#[test]
fn restart_timeout_terminates_the_unready_candidate_and_cleans_the_marker() {
    let fixture = Fixture::new(false);
    let path = readiness_path(&fixture.context.home).unwrap();
    let mut child = readiness_child(&path, "timeout");
    let error = wait_for_ready(&mut child, &path, Duration::from_millis(100)).unwrap_err();
    assert!(error.to_string().contains("timed out"));
    assert!(child.try_wait().unwrap().is_some());
    assert!(!path.exists());
}

#[test]
fn restart_rejects_invalid_readiness_and_reaps_the_candidate() {
    let fixture = Fixture::new(false);
    let path = readiness_path(&fixture.context.home).unwrap();
    let mut child = readiness_child(&path, "invalid");
    let error = wait_for_ready(&mut child, &path, Duration::from_secs(5)).unwrap_err();
    assert!(error
        .to_string()
        .contains("Invalid interface readiness marker"));
    assert!(child.try_wait().unwrap().is_some());
    assert!(!path.exists());
}

#[test]
fn readiness_signal_does_not_overwrite_an_existing_file() {
    let fixture = Fixture::new(false);
    let path = readiness_path(&fixture.context.home).unwrap();
    fs::write(&path, b"existing file").unwrap();
    assert!(signal_ready(&path).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"existing file");
}
