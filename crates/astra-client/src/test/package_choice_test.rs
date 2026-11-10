use super::*;

pub(crate) fn package(directory: &Path, content: &[u8]) -> Package {
    std::fs::create_dir_all(directory).unwrap();
    let mut names = vec![
        "astra-host",
        "astra-manager",
        "astra-tui",
        "astra-gui",
        "astra-rule",
        "astra-proxy",
        "astra-convert",
    ];
    if cfg!(windows) {
        names.push("astra-service");
    }
    let mut files = Vec::new();
    for name in &names {
        let path = astra_platform::platform::executable(name);
        std::fs::write(directory.join(&path), content).unwrap();
        files.push(serde_json::json!({"path": path, "size": content.len(), "sha256": format!("{:x}",Sha256::digest(content))}));
    }
    std::fs::write(directory.join("manifest.json"), serde_json::to_vec(&serde_json::json!({
        "schema_version":1,"role":"client","release_id":"0.1.0","target":astra_update::TARGET,
        "host_protocol":{"min":1,"max":1}, "files": files,
        "components": names.into_iter().map(|n|(n,"0.1.0")).collect::<std::collections::BTreeMap<_,_>>()
    })).unwrap()).unwrap();
    Package::inspect(directory).unwrap()
}
fn options(root: &Path, opened: &Package) -> Options {
    let mut options = Options::parse(None, Vec::<String>::new()).unwrap();
    options.home = root.join("state");
    options.config_home = root.join("config");
    options.executable = opened
        .directory
        .join(astra_platform::platform::executable("astra-tui"));
    options.bin = opened.directory.clone();
    options
}
pub(crate) fn select(root: &Path, selected: &Package) -> PathBuf {
    let directory = root.join("state/maintenance");
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("launch.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({
            "directory": selected.directory, "manifest_sha256": selected.manifest_sha256
        }))
        .unwrap(),
    )
    .unwrap();
    path
}

pub(crate) fn maintenance_record(
    root: &Path,
    target: &Package,
) -> astra_management::maintenance::Record {
    serde_json::from_value(serde_json::json!({
        "version": 1, "operation_id": "test-switch", "checkpoint_id": "test-checkpoint", "phase": "complete",
        "release_id": target.release_id, "target": target.directory, "manifest_sha256": target.manifest_sha256,
        "coordinator": target.directory.join("astra-manager"), "coordinator_sha256": "unused", "helper_sha256": "unused",
        "previous": root.join("old"), "old_epoch": "old",
        "old_process": astra_platform::lifecycle::capture(std::process::id()).unwrap(),
        "config": astra_management::config::ManagerConfig { state_dir: root.join("state"), ..Default::default() },
        "instances": [], "restored": [], "service_update": false,
    })).unwrap()
}

#[test]
fn same_version_different_build_requires_a_choice_without_changing_saved_selection() {
    let root = crate::import::tests::Fixture::new();
    let old = package(&root.0.join("old"), b"old payload");
    let new = package(&root.0.join("new"), b"new payload");
    let saved = select(&root.0, &old);
    let before = std::fs::read(&saved).unwrap();
    for frontend in ["astra-tui", "astra-gui"] {
        let mut options = options(&root.0, &new);
        options.executable = new
            .directory
            .join(astra_platform::platform::executable(frontend));
        let choice = Choice::discover(&options).unwrap().unwrap();
        assert_eq!(choice.opened, new);
        assert_eq!(choice.selected, old);
        assert_eq!(new.release_id, old.release_id);
        choice.choose(true, &mut options).unwrap();
        assert_eq!(options.bin, new.directory);
        assert!(Choice::discover(&options).unwrap().is_none());
        choice.choose(false, &mut options).unwrap();
        assert_eq!(options.bin, old.directory);
    }
    assert_eq!(std::fs::read(saved).unwrap(), before);
    assert!(!root.0.join("state/management.sock").exists());
    assert!(!root.0.join("state/logs").exists());
}

#[test]
fn explicit_headless_remote_and_resumed_launches_do_not_prompt() {
    let root = crate::import::tests::Fixture::new();
    let old = package(&root.0.join("old"), b"old");
    let new = package(&root.0.join("new"), b"new");
    select(&root.0, &old);
    for change in [
        |o: &mut Options| o.explicit_bin = true,
        |o: &mut Options| o.silent = true,
        |o: &mut Options| o.daemon = true,
        |o: &mut Options| o.shutdown = true,
        |o: &mut Options| o.resume_ui = true,
        |o: &mut Options| o.endpoint = Some("remote".into()),
        |o: &mut Options| o.launch.window_id = Some("child".into()),
    ] {
        let mut options = options(&root.0, &new);
        change(&mut options);
        assert!(Choice::discover(&options).unwrap().is_none());
    }
    let duplicate = package(&root.0.join("duplicate"), b"old");
    assert!(Choice::discover(&options(&root.0, &duplicate))
        .unwrap()
        .is_none());
}

#[test]
fn changing_the_package_after_the_prompt_never_launches_it() {
    let root = crate::import::tests::Fixture::new();
    let old = package(&root.0.join("old"), b"old");
    let new = package(&root.0.join("new"), b"new");
    select(&root.0, &old);
    let mut options = options(&root.0, &new);
    let choice = Choice::discover(&options).unwrap().unwrap();
    package(&new.directory, b"replacement with the same version");
    assert!(choice.choose(true, &mut options).is_err());
    assert!(!options.explicit_bin);
    assert_eq!(options.bin, new.directory);
}

#[test]
fn unfinished_maintenance_takes_priority_over_the_version_prompt() {
    let root = crate::import::tests::Fixture::new();
    let old = package(&root.0.join("old"), b"old");
    let new = package(&root.0.join("new"), b"new");
    select(&root.0, &old);
    let mut record = maintenance_record(&root.0, &new);
    record.phase = astra_management::maintenance::Phase::Prepared;
    astra_management::maintenance::write(&record).unwrap();
    assert!(Choice::discover(&options(&root.0, &new)).unwrap().is_none());
}
