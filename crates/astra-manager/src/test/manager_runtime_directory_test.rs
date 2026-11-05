use super::*;
#[test]
fn configured_runtime_directories_isolate_state_and_child_environment() {
    let directory = tempfile::tempdir().unwrap();
    let profile = directory.path().join("profile.toml");
    fs::write(&profile, "standalone=true").unwrap();
    let mut managers = Vec::new();
    for name in ["one", "two"] {
        let home = directory.path().join(format!("state-{name}"));
        private_dir(&home).unwrap();
        managers.push(
            Manager::load_with_runtime(
                home,
                directory.path().join(format!("config-{name}")),
                directory.path().into(),
                Some(directory.path().join(format!("runtime-{name}"))),
            )
            .unwrap(),
        );
    }
    let state = astra_model::runtime::Overrides {
        tun: Some(true),
        ..Default::default()
    };
    let first =
        astra_platform::storage::runtime_path_in(&managers[0].runtime_dir, &profile, "overrides")
            .unwrap();
    astra_platform::storage::write_json(&first, &state).unwrap();
    assert_eq!(
        astra_platform::runtime::load_overrides_in(&managers[0].runtime_dir, &profile)
            .unwrap()
            .tun,
        Some(true)
    );
    assert_eq!(
        astra_platform::runtime::load_overrides_in(&managers[1].runtime_dir, &profile)
            .unwrap()
            .tun,
        Some(false)
    );
    for manager in managers {
        let process = core_process(manager.bin.join("astra-proxy"), &manager.runtime_dir);
        assert!(process
            .as_std()
            .get_envs()
            .any(|(key, value)| key == "ASTRA_RUNTIME_DIR"
                && value == Some(manager.runtime_dir.as_os_str())));
    }
}
