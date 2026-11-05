use super::*;

#[test]
fn silent_launch_cannot_accidentally_open_a_window_or_run_maintenance() {
    let launch = windows::Launch::default();
    assert!(validate_silent_launch(false, false, false, false, false, &launch).is_ok());
    assert_eq!(
        validate_silent_launch(true, false, false, false, false, &launch).is_ok(),
        cfg!(windows)
    );
    for (daemon, shutdown, operation, console) in [
        (true, false, false, false),
        (false, true, false, false),
        (false, false, true, false),
        (false, false, false, true),
    ] {
        assert!(
            validate_silent_launch(true, daemon, shutdown, operation, console, &launch).is_err()
        );
    }
    let detached = windows::Launch::detached(windows::View::Profiles, None, protocol::Kind::Proxy);
    assert!(validate_silent_launch(true, false, false, false, false, &detached).is_err());
}
use astra_update::{
    ImportOptions, ProtocolRange, ReleaseFile, ReleaseManifest, ReleaseStore,
    HOST_PROTOCOL_VERSION, TARGET,
};
use sha2::{Digest, Sha256};

#[test]
fn server_default_view_is_primary_but_explicit_view_is_detached() {
    let mut primary = windows::Launch {
        kind: Some(protocol::Kind::Server),
        ..Default::default()
    };
    finish_launch(&mut primary, Some(protocol::Kind::Server));
    assert_eq!(primary.view, Some(windows::View::Profiles));
    assert!(primary.window_id.is_none());
    let mut detached = windows::Launch {
        view: Some(windows::View::Profiles),
        kind: Some(protocol::Kind::Server),
        ..Default::default()
    };
    finish_launch(&mut detached, Some(protocol::Kind::Server));
    assert!(detached.window_id.is_some());
    let existing = detached.window_id.clone();
    finish_launch(&mut detached, Some(protocol::Kind::Server));
    assert_eq!(detached.window_id, existing);
}

struct Directory {
    path: PathBuf,
    parent: PathBuf,
}
impl Directory {
    fn new() -> Self {
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let parent = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = parent.join(format!(
            "astra-entry-{}-{nonce}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir(&path).unwrap();
        Self { path, parent }
    }
    fn install(&self, host: bool) -> (ReleaseStore, PathBuf) {
        let source = self.path.join("source");
        std::fs::create_dir(&source).unwrap();
        let mut manifest = ReleaseManifest {
            schema_version: 1,
            role: Default::default(),
            release_id: "selected".into(),
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
        let mut components = vec!["astra-proxy", "astra-rule", "astra-tui"];
        if host {
            components.push("astra-host");
        }
        for component in components {
            let name = astra_platform::platform::executable(component);
            std::fs::write(source.join(&name), b"fixture").unwrap();
            manifest.components.insert(component.into(), "0.1.0".into());
            manifest.files.push(ReleaseFile {
                path: name,
                size: 7,
                sha256: format!("{:x}", Sha256::digest(b"fixture")),
            });
        }
        std::fs::write(
            source.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let home = self.path.join("state");
        let store = ReleaseStore::new(&home);
        store.import(source, &ImportOptions::default()).unwrap();
        store.commit_current("selected").unwrap();
        (store, home)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        if let Ok(target) = std::fs::canonicalize(&self.path) {
            assert_eq!(target.parent(), Some(self.parent.as_path()));
            let _ = std::fs::remove_dir_all(target);
        }
    }
}

#[test]
fn startup_uses_verified_current_host_and_explicit_bin_bypasses_selection() {
    let fixture = Directory::new();
    let (store, home) = fixture.install(true);
    let (directory, host) = selected_startup_release(&home, false).unwrap().unwrap();
    assert_eq!(directory, store.release_dir("selected").unwrap());
    assert_eq!(
        host,
        directory.join(astra_platform::platform::executable("astra-host"))
    );
    std::fs::write(store.root().join("current.json"), b"invalid").unwrap();
    assert!(selected_startup_release(&home, true).unwrap().is_none());
    assert!(selected_startup_release(&home, false).is_err());
}

#[test]
fn startup_does_not_fall_back_when_selected_release_has_no_host() {
    let fixture = Directory::new();
    let (store, home) = fixture.install(true);
    let manifest_path = store.release_dir("selected").unwrap().join("manifest.json");
    let mut manifest: ReleaseManifest =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    manifest.components.remove("astra-host");
    std::fs::write(manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    assert!(selected_startup_release(&home, false)
        .unwrap_err()
        .to_string()
        .contains("astra-host"));
    store.clear_current().unwrap();
    assert!(selected_startup_release(&home, false).unwrap().is_none());
}

#[test]
fn daemon_redirect_requires_no_existing_endpoint_and_no_explicit_bin() {
    let fixture = Directory::new();
    let executable = fixture.path.join("old-host");
    let selected = fixture.path.join("new-host");
    let socket = fixture.path.join("manager.sock");
    std::fs::write(&executable, b"old").unwrap();
    std::fs::write(&selected, b"new").unwrap();
    assert!(redirect_host(&executable, &selected, false, &socket).unwrap());
    assert!(!redirect_host(&executable, &selected, true, &socket).unwrap());
    assert!(!redirect_host(&selected, &selected, false, &socket).unwrap());
    std::fs::write(&socket, b"existing endpoint").unwrap();
    assert!(redirect_host(&executable, &selected, false, &socket).is_err());
}

#[test]
fn frontend_redirect_preserves_explicit_bin_and_allows_an_existing_manager() {
    let fixture = Directory::new();
    let (store, home) = fixture.install(true);
    let bin = store.release_dir("selected").unwrap();
    let original = fixture.path.join("old-tui");
    std::fs::write(&original, b"old").unwrap();
    std::fs::write(home.join("manager.sock"), b"live manager marker").unwrap();
    let selected = bin.join(astra_platform::platform::executable("astra-tui"));
    assert_eq!(
        selected_frontend(&original, &bin, true, false).unwrap(),
        Some(selected.clone())
    );
    assert!(selected_frontend(&original, &bin, true, true)
        .unwrap()
        .is_none());
    assert!(selected_frontend(&original, &bin, false, false)
        .unwrap()
        .is_none());
    assert!(selected_frontend(&selected, &bin, true, false)
        .unwrap()
        .is_none());
}
