#[path = "shared_egress.rs"]
pub mod egress;
#[path = "shared_mode_config.rs"]
pub mod mode_config;
#[path = "shared_privilege.rs"]
pub(crate) mod privilege;
pub(crate) mod worker;
#[path = "shared_process.rs"]
pub(crate) mod process;
#[cfg(not(windows))]
#[path = "unix_system_proxy.rs"]
pub mod system_proxy;
#[cfg(windows)]
#[path = "windows_system_proxy.rs"]
pub mod system_proxy;
#[path = "shared_transparent.rs"]
pub mod transparent;
#[path = "shared_tun.rs"]
pub mod tun;
#[path = "shared_tun_ingress.rs"]
mod tun_ingress;
#[cfg(not(windows))]
mod unix_network;
#[cfg(windows)]
pub mod windows_network;

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
