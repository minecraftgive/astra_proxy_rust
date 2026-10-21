pub mod egress;
pub mod mode_config;
pub(crate) mod privilege;
pub(crate) mod process;
#[cfg(not(windows))]
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
