pub mod egress;
pub(crate) mod file_limit;
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
    // Metadata must not initialize routing, privileges, profiles or workers.
    if std::env::args().nth(1).as_deref() == Some("--program-info") {
        use astra_platform::program_info::{FeatureSupport, ProgramInfo};
        let info = ProgramInfo::collect(
            astra_platform::build_info!(),
            vec![FeatureSupport::new(
                astra_model::control::NETWORK_DIAGNOSTICS_STREAM_CAPABILITY,
                "Network diagnostic stream",
                true,
                "Isolated system/interface TCP and DNS diagnostics; no profile initialization.",
            ), FeatureSupport::new(
                astra_model::control::NETWORK_DIAGNOSTICS_PINNED_CAPABILITY,
                "Pinned network diagnostics",
                true,
                "One verified node or DIRECT path; constrained resolution and no group refresh.",
            ), FeatureSupport::new(
                astra_model::control::NETWORK_DIAGNOSTICS_RESOLUTION_CAPABILITY,
                "Constrained diagnostic resolution",
                true,
                "Explicit resolvers and verified interface-scoped resolvers; no implicit system fallback.",
            ), FeatureSupport::new(
                astra_model::control::NETWORK_DIAGNOSTICS_TOOLS_CAPABILITY,
                "HTTP, native-root TLS and transport evidence", true,
                "Bounded HTTP/TLS, multi-port TCP, address-family and STUN diagnostics.",
            ), FeatureSupport::new(
                astra_model::control::NETWORK_DIAGNOSTICS_NAT_CAPABILITY,
                "Physical-interface NAT diagnostics", true,
                "Requires a manually selected physical interface verified at runtime; RFC 5780 depends on server evidence.",
            ), FeatureSupport::new(
                astra_model::control::NETWORK_DIAGNOSTICS_PATH_CAPABILITY,
                "ICMP and per-hop path evidence", true,
                "Runtime permissions and physical-path verification are checked before probing; never elevates privileges.",
            )],
        );
        println!("{}", serde_json::to_string(&info)?);
        return Ok(());
    }
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
            .worker_threads(std::thread::available_parallelism().map_or(1, |n| n.get().min(4)))
            .max_blocking_threads(32)
            .enable_all()
            .build()?;
        runtime.block_on(crate::run())
    };
    network_recovery::finish(result)
}
