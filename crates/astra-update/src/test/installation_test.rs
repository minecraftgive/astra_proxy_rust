use super::*;
use crate::{ProtocolRange, ReleaseFile};
use std::collections::BTreeMap;

struct Fixture {
    temporary: tempfile::TempDir,
    source: PathBuf,
    paths: InstallPaths,
}
impl Fixture {
    fn new(role: ReleaseRole) -> Self {
        // macOS exposes its temporary directory through /var -> /private/var;
        // installation deliberately rejects symlink ancestors.
        let temporary =
            tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let source = temporary.path().join("package");
        fs::create_dir(&source).unwrap();
        let components: &[&str] = match role {
            ReleaseRole::Client => {
                if cfg!(windows) {
                    &[
                        "astra-proxy",
                        "astra-rule",
                        "astra-tui",
                        "astra-host",
                        "astra-manager",
                        "astra-convert",
                        "astra-service",
                    ]
                } else {
                    &[
                        "astra-proxy",
                        "astra-rule",
                        "astra-tui",
                        "astra-host",
                        "astra-manager",
                        "astra-convert",
                    ]
                }
            }
            ReleaseRole::Server => &[
                "astra-server-core",
                "astra-server-tui",
                "astra-manager",
                "astra-host",
            ],
            ReleaseRole::Controller => &["astra-manager"],
            ReleaseRole::Web => &["astra-web"],
        };
        let mut manifest = ReleaseManifest {
            schema_version: 1,
            role,
            release_id: "fixture-1".into(),
            target: crate::TARGET.into(),
            host_protocol: ProtocolRange { min: 1, max: 1 },
            components: BTreeMap::new(),
            files: Vec::new(),
            profile: None,
            mode: None,
            cargo_profile: None,
        };
        for component in components {
            let name = astra_platform::platform::executable(component);
            let bytes = format!("fixture for {component}").into_bytes();
            fs::write(source.join(&name), &bytes).unwrap();
            manifest
                .components
                .insert((*component).into(), "1.0".into());
            manifest.files.push(ReleaseFile {
                path: name,
                size: bytes.len() as u64,
                sha256: format!("{:x}", Sha256::digest(&bytes)),
            });
        }
        fs::write(
            source.join(MANIFEST_FILE),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        let paths = InstallPaths {
            scope: InstallScope::User,
            role: role_name(role).into(),
            root: temporary.path().join("installed").join(role_name(role)),
            entry: temporary.path().join("commands").join(if cfg!(windows) {
                format!("{}.lnk", role_name(role))
            } else {
                format!("astra-{}", role_name(role))
            }),
        };
        Self {
            temporary,
            source,
            paths,
        }
    }
    fn install(&self) -> Result<InstallationStatus> {
        let source = source(&self.source)?;
        let release = source.manifest.release_id.clone();
        let hash = source.hash.clone();
        match install_at(source, &release, &hash, self.paths.clone(), |_| {})? {
            InstallOutcome::Installed(status) => Ok(status),
            _ => anyhow::bail!("Unexpected privilege requirement"),
        }
    }
}

#[test]
fn complete_roles_install_without_expanding_the_client_update_policy() {
    for role in [
        ReleaseRole::Client,
        ReleaseRole::Server,
        ReleaseRole::Controller,
        ReleaseRole::Web,
    ] {
        let fixture = Fixture::new(role);
        let verified =
            verify_installation_directory(&fixture.source, &ImportOptions::default()).unwrap();
        assert_eq!(verified.role, role);
        assert_eq!(
            verified.validate(&ImportOptions::default()).is_ok(),
            role == ReleaseRole::Client
        );
        let status = fixture.install().unwrap();
        assert!(status.installed);
        assert!(status.directory.unwrap().join(MANIFEST_FILE).is_file());
        assert!(fixture.paths.entry.is_file());
    }
}

#[test]
fn install_ignores_private_neighbours_and_is_idempotent() {
    let fixture = Fixture::new(ReleaseRole::Client);
    fs::write(
        fixture.source.join("credentials.toml"),
        "private credential",
    )
    .unwrap();
    let first = fixture.install().unwrap();
    let directory = first.directory.unwrap();
    assert!(!directory.join("credentials.toml").exists());
    let record = fs::read(fixture.paths.root.join("current.json")).unwrap();
    let entry = fs::read(&fixture.paths.entry).unwrap();
    let second = fixture.install().unwrap();
    assert_eq!(second.directory.as_ref(), Some(&directory));
    assert_eq!(
        record,
        fs::read(fixture.paths.root.join("current.json")).unwrap()
    );
    assert_eq!(entry, fs::read(&fixture.paths.entry).unwrap());
    assert_eq!(
        fs::read_dir(fixture.paths.root.join("deployments"))
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn repair_publishes_a_new_directory_without_overwriting_the_old_program() {
    let fixture = Fixture::new(ReleaseRole::Web);
    let old = fixture.install().unwrap().directory.unwrap();
    let binary = old.join(astra_platform::platform::executable("astra-web"));
    fs::write(&binary, "damaged").unwrap();
    let new = fixture.install().unwrap().directory.unwrap();
    assert_ne!(old, new);
    assert_eq!(fs::read_to_string(binary).unwrap(), "damaged");
    assert!(platform::entry_matches(
        &fixture.paths,
        &new.join(astra_platform::platform::executable("astra-web"))
    )
    .unwrap());
}

#[test]
fn unrelated_entry_and_changed_preview_never_publish_an_installation() {
    let fixture = Fixture::new(ReleaseRole::Web);
    let source = source(&fixture.source).unwrap();
    assert!(install_at(source, "stale", "stale", fixture.paths.clone(), |_| {}).is_err());
    assert!(!fixture.paths.root.exists());
    fs::create_dir_all(fixture.paths.entry.parent().unwrap()).unwrap();
    fs::write(&fixture.paths.entry, "another application's entry").unwrap();
    assert!(fixture.install().is_err());
    assert!(!fixture.paths.root.exists());
    assert_eq!(
        fs::read_to_string(&fixture.paths.entry).unwrap(),
        "another application's entry"
    );
}

#[test]
fn copy_verification_failure_preserves_the_previous_installation() {
    let fixture = Fixture::new(ReleaseRole::Web);
    let first = fixture.install().unwrap();
    let record = fs::read(fixture.paths.root.join("current.json")).unwrap();
    let entry = fs::read(&fixture.paths.entry).unwrap();
    let path = fixture.source.join(MANIFEST_FILE);
    let mut manifest: ReleaseManifest = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    manifest.release_id = "fixture-2".into();
    fs::write(&path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
    let source = source(&fixture.source).unwrap();
    let release = source.manifest.release_id.clone();
    let hash = source.hash.clone();
    let result = install_at(source, &release, &hash, fixture.paths.clone(), |phase| {
        if phase == "Copying the current program package" {
            fs::write(
                fixture
                    .source
                    .join(astra_platform::platform::executable("astra-web")),
                "changed while copying",
            )
            .unwrap();
        }
    });
    assert!(result.is_err());
    assert_eq!(
        record,
        fs::read(fixture.paths.root.join("current.json")).unwrap()
    );
    assert_eq!(entry, fs::read(&fixture.paths.entry).unwrap());
    assert!(first.directory.unwrap().exists());
    assert_eq!(
        fs::read_dir(fixture.paths.root.join("staging"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn incomplete_role_and_linked_payloads_are_rejected() {
    let fixture = Fixture::new(ReleaseRole::Client);
    fs::remove_file(
        fixture
            .source
            .join(astra_platform::platform::executable("astra-manager")),
    )
    .unwrap();
    assert!(fixture.install().is_err());
    let fixture = Fixture::new(ReleaseRole::Web);
    let binary = fixture
        .source
        .join(astra_platform::platform::executable("astra-web"));
    fs::hard_link(&binary, fixture.temporary.path().join("alias")).unwrap();
    assert!(fixture.install().is_err());
    assert!(!fixture.paths.root.exists());
}

#[cfg(unix)]
#[test]
fn destination_symlinks_cannot_redirect_the_installation() {
    let fixture = Fixture::new(ReleaseRole::Web);
    let outside = fixture.temporary.path().join("outside");
    fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, fixture.temporary.path().join("installed")).unwrap();
    assert!(fixture.install().is_err());
    assert_eq!(fs::read_dir(outside).unwrap().count(), 0);
}

#[cfg(unix)]
#[test]
fn missing_execute_permissions_are_repaired_instead_of_treated_as_installed() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new(ReleaseRole::Web);
    let first = fixture.install().unwrap().directory.unwrap();
    fs::set_permissions(&fixture.paths.entry, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(
        !inspect(&source(&fixture.source).unwrap(), &fixture.paths)
            .unwrap()
            .installed
    );
    assert_eq!(fixture.install().unwrap().directory.as_ref(), Some(&first));
    assert!(platform::executable_ready(&fixture.paths.entry, InstallScope::User).unwrap());
    fs::set_permissions(
        first.join(astra_platform::platform::executable("astra-web")),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    assert!(
        !inspect(&source(&fixture.source).unwrap(), &fixture.paths)
            .unwrap()
            .installed
    );
    let second = fixture.install().unwrap().directory.unwrap();
    assert_ne!(first, second);
    assert!(platform::executable_ready(
        &second.join(astra_platform::platform::executable("astra-web")),
        InstallScope::User
    )
    .unwrap());
}

#[test]
fn verified_gui_components_select_native_frontends_and_tui_packages_stay_terminal() {
    for (role, component) in [
        (ReleaseRole::Client, "astra-gui"),
        (ReleaseRole::Server, "astra-server-gui"),
    ] {
        let fixture = Fixture::new(role);
        #[cfg(unix)]
        let mut fixture = fixture;
        let terminal = source(&fixture.source).unwrap();
        assert_eq!(
            entry_binary(&terminal.manifest),
            if role == ReleaseRole::Client {
                "astra-tui"
            } else {
                "astra-server-tui"
            }
        );
        let mut manifest = terminal.manifest;
        let name = astra_platform::platform::executable(component);
        let bytes = b"native frontend fixture";
        fs::write(fixture.source.join(&name), bytes).unwrap();
        manifest.components.insert(component.into(), "1.0".into());
        manifest.files.push(ReleaseFile {
            path: name,
            size: bytes.len() as u64,
            sha256: format!("{:x}", Sha256::digest(bytes)),
        });
        fs::write(
            fixture.source.join(MANIFEST_FILE),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        #[cfg(target_os = "macos")]
        {
            fixture.paths.entry = fixture
                .temporary
                .path()
                .join("Applications/Astra.app/Contents/MacOS/astra-launch");
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            fixture.paths.entry = fixture.temporary.path().join("applications/astra.desktop");
        }
        let verified = source(&fixture.source).unwrap();
        assert_eq!(entry_binary(&verified.manifest), component);
        let status = fixture.install().unwrap();
        let directory = status.directory.unwrap();
        assert!(platform::entry_matches(
            &fixture.paths,
            &executable(&directory, &verified.manifest)
        )
        .unwrap());
        assert!(fixture.install().unwrap().installed);
        #[cfg(target_os = "macos")]
        {
            let contents = fixture.paths.entry.parent().unwrap().parent().unwrap();
            assert!(contents.join("Resources/Astra.icns").is_file());
            let info = fs::read_to_string(contents.join("Info.plist")).unwrap();
            assert!(info.contains("NSHighResolutionCapable"));
            assert!(info.contains("<key>LSUIElement</key><false/>"));
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let entry = fs::read_to_string(&fixture.paths.entry).unwrap();
            assert!(entry.contains("Terminal=false"));
            assert!(entry.contains(component));
        }
    }
}

// The subprocess executes from the installed path, so Windows holds an actual
// executable image open (an ordinary open file is insufficient to test this).
#[test]
fn running_installed_frontend_child() {
    let Some(marker) = std::env::var_os("ASTRA_INSTALL_TEST_READY") else {
        return;
    };
    fs::write(marker, b"ready").unwrap();
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

#[test]
fn complete_package_updates_while_old_frontend_is_running() {
    struct Process(std::process::Child);
    impl Drop for Process {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    fn launch(binary: &Path, marker: &Path) -> Process {
        let mut process = Process(
            std::process::Command::new(binary)
                .args([
                    "--exact",
                    "installation::tests::running_installed_frontend_child",
                    "--nocapture",
                ])
                .env("ASTRA_INSTALL_TEST_READY", marker)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !marker.exists() {
            assert!(
                process.0.try_wait().unwrap().is_none(),
                "Installed frontend exited"
            );
            assert!(
                std::time::Instant::now() < deadline,
                "Installed frontend did not start"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        process
    }
    let fixture = Fixture::new(ReleaseRole::Client);
    let name = astra_platform::platform::executable("astra-tui");
    let bytes = fs::read(std::env::current_exe().unwrap()).unwrap();
    fs::write(fixture.source.join(&name), &bytes).unwrap();
    let mut manifest = source_manifest(&fixture.source);
    let file = manifest.files.iter_mut().find(|f| f.path == name).unwrap();
    file.size = bytes.len() as u64;
    file.sha256 = format!("{:x}", Sha256::digest(&bytes));
    fs::write(
        fixture.source.join(MANIFEST_FILE),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    let old = fixture.install().unwrap().directory.unwrap();
    let mut running = launch(
        &old.join(&name),
        &fixture.temporary.path().join("old-ready"),
    );
    manifest.release_id = "fixture-2".into();
    for file in &mut manifest.files {
        if file.path != name {
            let bytes = format!("new version of {}", file.path);
            fs::write(fixture.source.join(&file.path), &bytes).unwrap();
            file.size = bytes.len() as u64;
            file.sha256 = format!("{:x}", Sha256::digest(bytes.as_bytes()));
        }
    }
    fs::write(
        fixture.source.join(MANIFEST_FILE),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    let new = fixture.install().unwrap().directory.unwrap();
    assert_ne!(old, new);
    assert!(running.0.try_wait().unwrap().is_none());
    assert_eq!(
        verify_installation_directory(&new, &ImportOptions::default()).unwrap(),
        manifest
    );
    assert_eq!(
        verify_installation_directory(&old, &ImportOptions::default())
            .unwrap()
            .release_id,
        "fixture-1"
    );
    let mut replacement = launch(&new.join(name), &fixture.temporary.path().join("new-ready"));
    assert!(replacement.0.try_wait().unwrap().is_none());
}

fn source_manifest(directory: &Path) -> ReleaseManifest {
    serde_json::from_slice(&fs::read(directory.join(MANIFEST_FILE)).unwrap()).unwrap()
}
