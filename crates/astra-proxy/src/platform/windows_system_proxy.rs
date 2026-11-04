//! Current-user WinINet LAN proxy with durable recovery and exclusive ownership.
use crate::config::ProxyConfig;
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::windows::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};
use windows_sys::Win32::{Foundation::GlobalFree, Networking::WinInet::*};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    flags: u32,
    server: Option<String>,
    bypass: Option<String>,
    auto_config_url: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Snapshot {
    provider: String,
    version: u32,
    original: Settings,
    #[serde(default)]
    expected: Option<Settings>,
}
#[derive(Default)]
pub struct Guard {
    original: Option<Settings>,
    journal: Option<PathBuf>,
    lock: Option<File>,
    monitor: Option<Monitor>,
    expected: Option<Settings>,
    lease: Option<super::network_recovery::Lease>,
}

// Stop and join before restoring settings so a pending repair cannot undo restore.
struct Monitor {
    stop: std::sync::mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Drop for Monitor {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
fn matches_expected(actual: &Settings, expected: &Settings) -> bool {
    actual.flags == expected.flags
        && actual.server.as_deref().unwrap_or("") == expected.server.as_deref().unwrap_or("")
        && actual.bypass.as_deref().unwrap_or("") == expected.bypass.as_deref().unwrap_or("")
        && actual.auto_config_url.as_deref().unwrap_or("")
            == expected.auto_config_url.as_deref().unwrap_or("")
}
fn monitor(expected: Settings) -> Result<Monitor> {
    let (stop, receiver) = std::sync::mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("system-proxy-monitor".into())
        .spawn(move || {
            while matches!(
                receiver.recv_timeout(std::time::Duration::from_secs(10)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ) {
                match query(INTERNET_PER_CONN_FLAGS_UI).or_else(|_| query(INTERNET_PER_CONN_FLAGS)) {
                    Ok(actual) if matches_expected(&actual, &expected) => continue,
                    Ok(_) => { tracing::warn!("Windows system proxy settings changed externally; preserving the change"); break; },
                    Err(error) => {
                        tracing::warn!(%error, "failed to check Windows system proxy; retrying next interval");
                        continue;
                    }
                }
            }
        })?;
    Ok(Monitor {
        stop,
        thread: Some(thread),
    })
}

fn option(option: u32) -> INTERNET_PER_CONN_OPTIONW {
    INTERNET_PER_CONN_OPTIONW {
        dwOption: option,
        ..Default::default()
    }
}
fn list(options: &mut [INTERNET_PER_CONN_OPTIONW]) -> INTERNET_PER_CONN_OPTION_LISTW {
    INTERNET_PER_CONN_OPTION_LISTW {
        dwSize: std::mem::size_of::<INTERNET_PER_CONN_OPTION_LISTW>() as u32,
        pszConnection: std::ptr::null_mut(), // LAN/current user, not a RAS/VPN connection.
        dwOptionCount: options.len() as u32,
        pOptions: options.as_mut_ptr(),
        ..Default::default()
    }
}
fn query(flags_option: u32) -> Result<Settings> {
    let mut options = [
        option(flags_option),
        option(INTERNET_PER_CONN_PROXY_SERVER),
        option(INTERNET_PER_CONN_PROXY_BYPASS),
        option(INTERNET_PER_CONN_AUTOCONFIG_URL),
    ];
    let mut list = list(&mut options);
    let mut size = list.dwSize;
    let ok = unsafe {
        InternetQueryOptionW(
            std::ptr::null(),
            INTERNET_OPTION_PER_CONNECTION_OPTION,
            (&mut list as *mut INTERNET_PER_CONN_OPTION_LISTW).cast(),
            &mut size,
        )
    };
    let error = std::io::Error::last_os_error();
    // WinINet allocates returned strings with GlobalAlloc, including empty values.
    let mut strings = Vec::new();
    for option in &options[1..] {
        let pointer = unsafe { option.Value.pszValue };
        if pointer.is_null() {
            strings.push(None);
            continue;
        }
        let mut len = 0;
        unsafe {
            while *pointer.add(len) != 0 {
                len += 1;
            }
            strings.push(Some(String::from_utf16_lossy(std::slice::from_raw_parts(
                pointer, len,
            ))));
            GlobalFree(pointer.cast());
        }
    }
    ensure!(ok != 0, "read Windows LAN proxy settings: {error}");
    Ok(Settings {
        flags: unsafe { options[0].Value.dwValue },
        server: strings[0].clone(),
        bypass: strings[1].clone(),
        auto_config_url: strings[2].clone(),
    })
}
fn wide(value: &Option<String>) -> Result<Option<Vec<u16>>> {
    value
        .as_ref()
        .map(|value| {
            ensure!(
                !value.contains('\0') && value.len() <= 65536,
                "invalid Windows proxy setting string"
            );
            Ok(value.encode_utf16().chain(Some(0)).collect())
        })
        .transpose()
}
fn write(settings: &Settings) -> Result<()> {
    let mut strings = [
        wide(&settings.server)?,
        wide(&settings.bypass)?,
        wide(&settings.auto_config_url)?,
    ];
    let mut options = [
        option(INTERNET_PER_CONN_FLAGS),
        option(INTERNET_PER_CONN_PROXY_SERVER),
        option(INTERNET_PER_CONN_PROXY_BYPASS),
        option(INTERNET_PER_CONN_AUTOCONFIG_URL),
    ];
    options[0].Value.dwValue = settings.flags;
    for (option, value) in options[1..].iter_mut().zip(&mut strings) {
        option.Value.pszValue = value
            .as_mut()
            .map_or(std::ptr::null_mut(), |v| v.as_mut_ptr());
    }
    let mut list = list(&mut options);
    let ok = unsafe {
        InternetSetOptionW(
            std::ptr::null(),
            INTERNET_OPTION_PER_CONNECTION_OPTION,
            (&mut list as *mut INTERNET_PER_CONN_OPTION_LISTW).cast(),
            list.dwSize,
        )
    };
    ensure!(
        ok != 0,
        "write Windows LAN proxy settings: {} (option index {})",
        std::io::Error::last_os_error(),
        list.dwOptionError
    );
    for flag in [INTERNET_OPTION_SETTINGS_CHANGED, INTERNET_OPTION_REFRESH] {
        let ok = unsafe { InternetSetOptionW(std::ptr::null(), flag, std::ptr::null(), 0) };
        ensure!(
            ok != 0,
            "notify Windows proxy change: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

fn lock(recovery: Option<&Path>) -> Result<File> {
    let directory = astra_platform::platform::home_dir()?.join(".astra");
    std::fs::create_dir_all(&directory)?;
    // The file contains only a journal path, never credentials. Exclusive sharing
    // provides a kernel lock across processes and sessions of the current user.
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(directory.join("system-proxy.lock"))
        .context("another Astra instance is managing this user's Windows proxy")?;
    let mut previous = String::new();
    (&mut file).take(32768).read_to_string(&mut previous)?;
    let previous = PathBuf::from(previous);
    if previous.is_file() {
        ensure!(recovery.is_some_and(|p| std::fs::canonicalize(p).ok() == std::fs::canonicalize(&previous).ok()),
            "an earlier Windows proxy snapshot is pending; recover {} first with --restore-system-proxy", previous.display());
    }
    file.seek(SeekFrom::Start(0))?;
    Ok(file)
}

impl Guard {
    pub fn apply(config: &ProxyConfig, journal: &Path) -> Result<Self> {
        let Some(settings) = &config.system_proxy else {
            return Ok(Self::default());
        };
        ensure!(
            settings.services.is_empty(),
            "system_proxy.services is macOS-only"
        );
        super::mode_config::system_proxy_address(config)?;
        let (http, socks) = super::mode_config::system_proxy_addresses(config)?;
        // Windows manual HTTP proxy uses a single host:port endpoint.
        // Keep SOCKS-only configuration explicitly typed, never treat it as HTTP.
        let server = match (http, socks) {
            (Some(address), _) => address.to_string(),
            (None, Some(address)) => format!("socks={address}"),
            (None, None) => unreachable!("listener validated above"),
        };
        super::network_recovery::ensure()?;
        let mut lock = lock(None)?;
        let original =
            query(INTERNET_PER_CONN_FLAGS_UI).or_else(|_| query(INTERNET_PER_CONN_FLAGS))?;
        let expected = Settings {
            flags: PROXY_TYPE_DIRECT | PROXY_TYPE_PROXY,
            server: Some(server),
            bypass: Some("<local>".into()),
            auto_config_url: Some(String::new()),
        };
        let snapshot = Snapshot {
            provider: "windows-wininet-lan".into(),
            version: 1,
            original: original.clone(),
            expected: Some(expected.clone()),
        };
        // Crash recovery is experimental: without it the original settings
        // stay in memory only and an abrupt core exit leaves them as applied.
        let recording = super::network_recovery::enabled();
        let lease = if recording {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(journal)
                .with_context(|| {
                    format!(
                        "recovery file exists or cannot be created: {}; restore it before starting",
                        journal.display()
                    )
                })?;
            let saved = (|| -> Result<()> {
                astra_platform::platform::set_private(journal, 0o600)?;
                file.write_all(toml::to_string(&snapshot)?.as_bytes())?;
                file.sync_all()?;
                Ok(())
            })();
            drop(file);
            if let Err(error) = saved {
                let _ = std::fs::remove_file(journal);
                return Err(error);
            }
            Some(super::network_recovery::Lease::prepare(
                super::network_recovery::Resource::SystemProxy {
                    path: std::fs::canonicalize(journal)?,
                    snapshot: Some(snapshot.clone()),
                },
            )?)
        } else {
            None
        };
        let mut guard = Self {
            original: Some(original),
            journal: recording.then(|| journal.to_owned()),
            lock: None,
            monitor: None,
            expected: Some(expected.clone()),
            lease,
        };
        lock.set_len(0)?;
        if recording {
            lock.write_all(
                std::fs::canonicalize(journal)?
                    .to_str()
                    .context("recovery path is not UTF-8")?
                    .as_bytes(),
            )?;
        }
        lock.sync_all()?;
        guard.lock = Some(lock);
        write(&expected)?;
        if let Some(lease) = &guard.lease {
            lease.commit()?;
        }
        guard.monitor = Some(monitor(expected)?);
        Ok(guard)
    }
    pub fn restore(&mut self) -> Result<()> {
        drop(self.monitor.take());
        if let Some(original) = &self.original {
            let actual =
                query(INTERNET_PER_CONN_FLAGS_UI).or_else(|_| query(INTERNET_PER_CONN_FLAGS))?;
            if !matches_expected(&actual, original) {
                ensure!(
                    self.expected
                        .as_ref()
                        .is_none_or(|expected| matches_expected(&actual, expected)),
                    "Windows proxy changed externally; recovery record retained"
                );
                write(original)?;
            }
            if let Some(lock) = &mut self.lock {
                lock.set_len(0)?;
                lock.sync_all()?;
            }
            if let Some(path) = &self.journal {
                match std::fs::remove_file(path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
            if let Some(lease) = &self.lease {
                lease.complete()?;
            }
            self.lease = None;
            self.original = None;
            self.journal = None;
            self.lock = None;
        }
        Ok(())
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            if let Some(lease) = &self.lease {
                lease.failed(&error);
            }
            tracing::error!(%error, "Windows proxy recovery remains pending");
        }
    }
}
pub fn recover(path: &Path) -> Result<()> {
    let text = super::network_recovery::read_private(path, 1024 * 1024)?;
    let snapshot: Snapshot = toml::from_str(std::str::from_utf8(&text)?)?;
    restore_snapshot(path, &snapshot)
}
fn restore_snapshot(path: &Path, snapshot: &Snapshot) -> Result<()> {
    ensure!(
        snapshot.version == 1 && snapshot.provider == "windows-wininet-lan",
        "unsupported Windows proxy recovery document"
    );
    let lock = lock(Some(path))?;
    Guard {
        original: Some(snapshot.original.clone()),
        journal: Some(path.to_owned()),
        lock: Some(lock),
        monitor: None,
        expected: snapshot.expected.clone(),
        lease: None,
    }
    .restore()
}
pub(super) fn recover_snapshot(path: &Path, snapshot: &Snapshot) -> Result<()> {
    ensure!(
        snapshot.expected.is_some(),
        "legacy proxy snapshot requires explicit --restore-system-proxy"
    );
    restore_snapshot(path, snapshot)
}
/// Automatic recovery requires the expected value; legacy records stay manual.
pub(super) fn recover_owned(path: &Path) -> Result<()> {
    let bytes = super::network_recovery::read_private(path, 1024 * 1024)?;
    let snapshot: Snapshot = toml::from_str(std::str::from_utf8(&bytes)?)?;
    ensure!(
        snapshot.expected.is_some(),
        "legacy proxy snapshot requires explicit --restore-system-proxy"
    );
    recover(path)
}

#[cfg(test)]
mod ownership_tests {
    use super::*;
    #[test]
    fn matching_proxy_requires_all_managed_properties() {
        let original = Settings {
            flags: 3,
            server: Some("127.0.0.1:8080".into()),
            bypass: Some("<local>".into()),
            auto_config_url: None,
        };
        let mut actual = original.clone();
        actual.auto_config_url = Some(String::new());
        assert!(matches_expected(&actual, &original));
        actual.server = Some("another.proxy:3128".into());
        assert!(!matches_expected(&actual, &original));
        actual = original.clone();
        actual.flags = 1;
        assert!(!matches_expected(&actual, &original));
        actual = original.clone();
        actual.auto_config_url = Some("https://example.invalid/pac".into());
        assert!(!matches_expected(&actual, &original));
    }
}
