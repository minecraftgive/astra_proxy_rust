pub mod egress;
pub mod mode_config;
pub mod network_recovery;
pub(crate) mod privilege;
pub(crate) mod process;
#[cfg(not(windows))]
#[path = "unix_system_proxy.rs"]
pub mod system_proxy;
#[cfg(windows)]
#[path = "windows_system_proxy.rs"]
pub mod system_proxy;
pub mod transparent;
pub mod tun;
mod tun_ingress;
#[cfg(not(windows))]
mod unix_network;
#[cfg(windows)]
pub mod windows_network;
pub(crate) mod worker;

pub(crate) fn system_listener_changed(
    new: &crate::config::ProxyConfig,
    old: &crate::config::ProxyConfig,
) -> bool {
    if cfg!(windows) {
        new.http_listen != old.http_listen
    } else {
        new.socks5_listen != old.socks5_listen
    }
}

pub(crate) mod icmp;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod unix_elevated;
/// Perform privileged hand-offs before starting worker threads.
pub fn entry() -> anyhow::Result<()> {
    if let Some(result) = network_recovery::entry() {
        return result;
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if let Some(result) = unix_elevated::entry() {
        return result;
    }
    // SAFETY: this entrypoint is called before constructing any runtime threads.
    #[cfg(unix)]
    unsafe {
        worker::initialize_owner()?;
    }
    let result = {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        runtime.block_on(crate::run())
    };
    network_recovery::finish(result)
}
