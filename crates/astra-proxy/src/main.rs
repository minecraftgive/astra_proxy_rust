fn main() -> anyhow::Result<()> {
    // The elevated hand-off forks and drops privileges before any Tokio
    // worker exists; it must not go through the normal runtime path.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if let Some(result) = astra_proxy::unix_elevated::entry() {
        return result;
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(astra_proxy::run())
}
