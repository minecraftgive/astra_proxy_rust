//! Read-only program metadata. Platform support never implies a feature is
//! enabled, a required driver is installed, or a core is currently running.
use serde::{Deserialize, Serialize};

#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildInfo {
    pub name: String,
    pub version: String,
    pub compiled_at: String,
    pub target: String,
    pub profile: String,
    pub rustc: String,
    pub git_revision: Option<String>,
    pub debug_assertions: bool,
}

/// Expand in the executable's crate, which includes build_metadata.rs.
#[macro_export]
macro_rules! build_info {
    () => {
        $crate::program_info::BuildInfo {
            name: env!("CARGO_PKG_NAME").into(),
            version: env!("CARGO_PKG_VERSION").into(),
            compiled_at: $crate::program_info::format_build_time(env!("ASTRA_BUILD_TIMESTAMP")),
            target: env!("ASTRA_BUILD_TARGET").into(),
            profile: env!("ASTRA_BUILD_PROFILE").into(),
            rustc: env!("ASTRA_BUILD_RUSTC").into(),
            git_revision: match env!("ASTRA_BUILD_REVISION") {
                "" => None,
                revision => Some(revision.into()),
            },
            debug_assertions: cfg!(debug_assertions),
        }
    };
}

#[doc(hidden)]
pub fn format_build_time(timestamp: &str) -> String {
    timestamp
        .parse::<i64>()
        .ok()
        .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0))
        .map(|date| date.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| "unknown".into())
}

#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemInfo {
    pub os: String,
    pub os_version: Option<String>,
    pub architecture: String,
    pub family: String,
    pub logical_cpus: Option<u32>,
    pub process_id: u32,
    pub elevated: Option<bool>,
    pub executable: Option<String>,
    pub ipc_transport: String,
}

#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeatureSupport {
    pub id: String,
    pub label: String,
    pub supported: bool,
    pub detail: String,
}
impl FeatureSupport {
    pub fn new(id: &str, label: &str, supported: bool, detail: &str) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            supported,
            detail: detail.into(),
        }
    }
}

#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgramInfo {
    pub build: BuildInfo,
    pub system: SystemInfo,
    pub features: Vec<FeatureSupport>,
    pub platform_features: Vec<FeatureSupport>,
}
impl ProgramInfo {
    pub fn collect(build: BuildInfo, features: Vec<FeatureSupport>) -> Self {
        let platform = astra_model::document::Platform::current();
        Self {
            build,
            system: SystemInfo {
                os: std::env::consts::OS.into(),
                os_version: os_version(),
                architecture: std::env::consts::ARCH.into(),
                family: std::env::consts::FAMILY.into(),
                logical_cpus: std::thread::available_parallelism()
                    .ok()
                    .and_then(|n| u32::try_from(n.get()).ok()),
                process_id: std::process::id(),
                elevated: elevated(),
                executable: std::env::current_exe()
                    .ok()
                    .map(|p| p.display().to_string()),
                ipc_transport: if cfg!(windows) {
                    "Named pipes"
                } else {
                    "Unix sockets"
                }
                .into(),
            },
            features,
            platform_features: vec![
                FeatureSupport::new(
                    "tun",
                    "TUN",
                    platform.option(None, "tun"),
                    "Requires core configuration, permissions and a working TUN device/driver.",
                ),
                FeatureSupport::new(
                    "system_proxy",
                    "System proxy",
                    platform.option(None, "system_proxy"),
                    "Support depends on the desktop environment and proxy configuration.",
                ),
                FeatureSupport::new(
                    "tproxy",
                    "TProxy",
                    platform.option(None, "tproxy"),
                    "Linux only; requires privileges and firewall routing rules.",
                ),
                FeatureSupport::new(
                    "redirect",
                    "Redirect",
                    platform.option(None, "redirect"),
                    "Linux only; requires privileges and firewall redirection rules.",
                ),
                FeatureSupport::new(
                    "local_ipc",
                    "Local IPC",
                    cfg!(any(windows, unix)),
                    "Private local management transport.",
                ),
            ],
        }
    }
}

fn elevated() -> Option<bool> {
    #[cfg(windows)]
    {
        crate::elevation::is_elevated().ok()
    }
    #[cfg(unix)]
    {
        Some(unsafe { libc::geteuid() } == 0)
    }
    #[cfg(not(any(windows, unix)))]
    {
        None
    }
}

#[cfg(unix)]
fn os_version() -> Option<String> {
    let mut info = std::mem::MaybeUninit::<libc::utsname>::uninit();
    if unsafe { libc::uname(info.as_mut_ptr()) } != 0 {
        return None;
    }
    let info = unsafe { info.assume_init() };
    let release = unsafe { std::ffi::CStr::from_ptr(info.release.as_ptr()) };
    Some(release.to_string_lossy().into_owned())
}

#[cfg(windows)]
fn os_version() -> Option<String> {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
    fn value(name: &str) -> Option<String> {
        let key: Vec<u16> = "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\0"
            .encode_utf16()
            .collect();
        let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let mut buffer = [0u16; 256];
        let mut size = std::mem::size_of_val(&buffer) as u32;
        let status = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                buffer.as_mut_ptr().cast(),
                &mut size,
            )
        };
        if status != 0 {
            return None;
        }
        let len = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
        Some(String::from_utf16_lossy(&buffer[..len]))
    }
    let build = value("CurrentBuildNumber")?;
    Some(match value("DisplayVersion") {
        Some(version) => format!("{version} (build {build})"),
        None => format!("build {build}"),
    })
}

#[cfg(not(any(windows, unix)))]
fn os_version() -> Option<String> {
    None
}
