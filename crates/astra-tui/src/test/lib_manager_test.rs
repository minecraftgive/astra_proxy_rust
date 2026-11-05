//! Local filesystem compatibility helpers; no daemon implementation is linked into the UI.
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
pub async fn serve(home: std::path::PathBuf, bin: std::path::PathBuf) -> anyhow::Result<()> {
    astra_manager::run_config(astra_management::config::ManagerConfig {
        config_dir: home.with_extension("configs"),
        state_dir: home,
        bin_dir: bin,
        ..Default::default()
    })
    .await
}
