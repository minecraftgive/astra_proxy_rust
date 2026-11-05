//! Narrow Windows privilege broker, independent of the management API.
#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() -> std::io::Result<()> {
    #[cfg(windows)]
    return astra_platform::windows_service::run_cli();
    #[cfg(not(windows))]
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "Windows privilege service is only supported on Windows",
    ))
}
