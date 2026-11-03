//! Local filesystem compatibility helpers; no daemon implementation is linked into the UI.
#[cfg(test)]
pub fn test_binaries() -> std::path::PathBuf {
    std::env::var_os("ASTRA_UPDATE_TEST_BIN")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_exe()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .to_owned()
        })
}
#[cfg(test)]
pub async fn serve(home: std::path::PathBuf, bin: std::path::PathBuf) -> anyhow::Result<()> {
    astra_manager::run_config(astra_management::config::ManagerConfig {
        config_dir: home.with_extension("configs"),
        state_dir: home,
        bin_dir: bin,
        ..Default::default()
    })
    .await
}
pub fn private_dir(path: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(path)?;
    anyhow::ensure!(
        !std::fs::symlink_metadata(path)?.file_type().is_symlink(),
        "State directory cannot be a symlink"
    );
    astra_platform::platform::set_private(path, 0o700)?;
    Ok(())
}
pub fn atomic_write(path: &std::path::Path, data: &[u8]) -> anyhow::Result<()> {
    astra_platform::storage::atomic_write(path, data, Default::default())?;
    Ok(())
}
