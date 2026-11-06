use super::*;

fn package(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    let name = astra_platform::platform::executable("astra-manager");
    let bytes = b"test manager executable";
    std::fs::write(path.join(&name), bytes).unwrap();
    let manifest = serde_json::json!({
        "schema_version": 1, "role": "controller", "release_id": "fixture-controller",
        "target": astra_update::TARGET, "host_protocol": {"min": 1, "max": 1},
        "components": {"astra-manager": "0.1.0"},
        "files": [{"path": name, "size": bytes.len(), "sha256": format!("{:x}", Sha256::digest(bytes))}]
    });
    std::fs::write(
        path.join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
}

#[test]
fn discovery_only_uses_target_context_and_deduplicates_complete_packages() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("中文 source");
    package(&source);
    std::fs::write(source.join("private-config.toml"), "do not install").unwrap();
    let config = ManagerConfig {
        bin_dir: source.clone(),
        web: astra_management::config::WebConfig {
            binary: Some(source.join("astra-web")),
            ..Default::default()
        },
        ..Default::default()
    };
    let (sources, _) = discover(&config);
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].path, std::fs::canonicalize(&source).unwrap());
    assert_eq!(role_name(&sources[0].manifest), "controller");
    assert_eq!(
        std::fs::read_to_string(source.join("private-config.toml")).unwrap(),
        "do not install"
    );
}

#[test]
fn selection_is_invalidated_by_manifest_changes_even_at_same_release_id() {
    let root = tempfile::tempdir().unwrap();
    package(root.path());
    let config = ManagerConfig {
        bin_dir: root.path().into(),
        ..Default::default()
    };
    let before = inspect(root.path()).unwrap();
    assert!(selected(&config, &before.id, &before.manifest.release_id).is_ok());
    let manifest = root.path().join("manifest.json");
    let mut bytes = std::fs::read(&manifest).unwrap();
    bytes.push(b'\n');
    std::fs::write(manifest, bytes).unwrap();
    let error = selected(&config, &before.id, &before.manifest.release_id)
        .err()
        .unwrap();
    assert!(error.to_string().starts_with("Conflict:"));
    assert!(selected(&config, &inspect(root.path()).unwrap().id, "stale-release").is_err());
}

#[test]
fn development_directory_is_unavailable_without_creating_installation_state() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("build");
    std::fs::create_dir(&source).unwrap();
    let config = ManagerConfig {
        bin_dir: source.clone(),
        state_dir: root.path().join("state"),
        ..Default::default()
    };
    let status = status(&config, "selected target".into()).unwrap();
    assert!(status.packages.is_empty());
    assert_eq!(status.unavailable_roles.len(), 4);
    assert_eq!(status.target, "selected target");
    assert!(!status.warnings.is_empty());
    assert!(!config.state_dir.exists());
    assert_eq!(std::fs::read_dir(source).unwrap().count(), 0);
}

#[test]
fn privileged_helper_rejects_arbitrary_arguments_and_user_scope() {
    assert!(run_helper(vec![]).is_err());
    assert!(run_helper(vec!["relative".into(), "release".into(), "digest".into()]).is_err());
    assert!(prepare(
        &ManagerConfig::default(),
        "id",
        "release",
        InstallationScope::User
    )
    .is_err());
}

#[test]
fn launch_hint_preserves_custom_configuration_without_overriding_update_selection() {
    let config = ManagerConfig {
        state_dir: "custom state".into(),
        config_dir: "custom config".into(),
        runtime_dir: Some("custom runtime".into()),
        ..Default::default()
    };
    let command = launch_command(&config, Path::new("installed/astra-tui"), "client");
    assert!(command.contains("--state-dir 'custom state'"));
    assert!(command.contains("--config-dir 'custom config'"));
    assert!(command.contains("ASTRA_RUNTIME_DIR"));
    assert!(!command.contains("--bin-dir"));
}
