//! Rendering-independent services shared by Astra frontends.
pub mod availability;
pub mod clipboard;
pub mod export;
pub mod external_editor;
pub mod i18n;
pub mod import;
pub mod log_stream;
pub mod network_interfaces;
pub mod protocol;
pub mod self_update;
pub mod shell;
#[cfg(windows)]
pub mod startup;
#[cfg(any(target_os = "macos", target_os = "linux", test))]
pub mod startup_unix;
pub mod terminals;
pub mod windows;
pub use astra_management::subscription_view;
pub use astra_platform::tui_log;
