//! Current-user WinINet LAN proxy with durable recovery and exclusive ownership.
use crate::config::ProxyConfig;
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    net::SocketAddr,
    os::windows::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};
use windows_sys::Win32::{Foundation::GlobalFree, Networking::WinInet::*};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    flags: u32,
    server: Option<String>,
    bypass: Option<String>,
    auto_config_url: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    provider: String,
    version: u32,
    original: Settings,
}
#[derive(Default)]
pub struct Guard {
    original: Option<Settings>,
    journal: Option<PathBuf>,
    lock: Option<File>,
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
    let directory = astra_ipc::platform::home_dir()?.join(".astra");
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
        let address: SocketAddr = config
            .http_listen
            .as_ref()
            .context("Windows system proxy requires http_listen")?
            .parse()?;
        ensure!(
            address.ip().is_loopback() && address.port() != 0,
            "Windows system proxy requires a loopback HTTP listener"
        );
        let mut lock = lock(None)?;
        let original =
            query(INTERNET_PER_CONN_FLAGS_UI).or_else(|_| query(INTERNET_PER_CONN_FLAGS))?;
        let snapshot = Snapshot {
            provider: "windows-wininet-lan".into(),
            version: 1,
            original: original.clone(),
        };
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
            astra_ipc::platform::set_private(journal, 0o600)?;
            file.write_all(toml::to_string(&snapshot)?.as_bytes())?;
            file.sync_all()?;
            Ok(())
        })();
        drop(file);
        if let Err(error) = saved {
            let _ = std::fs::remove_file(journal);
            return Err(error);
        }
        let mut guard = Self {
            original: Some(original),
            journal: Some(journal.to_owned()),
            lock: None,
        };
        lock.set_len(0)?;
        lock.write_all(
            std::fs::canonicalize(journal)?
                .to_str()
                .context("recovery path is not UTF-8")?
                .as_bytes(),
        )?;
        lock.sync_all()?;
        guard.lock = Some(lock);
        // Only DIRECT + PROXY: temporarily suppress PAC and auto-detection.
        // HTTP/HTTPS both target the local HTTP proxy (HTTPS uses CONNECT).
        write(&Settings {
            flags: PROXY_TYPE_DIRECT | PROXY_TYPE_PROXY,
            server: Some(format!("http={address};https={address}")),
            bypass: Some("<local>".into()),
            auto_config_url: Some(String::new()),
        })?;
        Ok(guard)
    }
    pub fn restore(&mut self) -> Result<()> {
        if let Some(original) = &self.original {
            write(original)?;
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
            tracing::error!(%error, "Windows proxy recovery remains pending");
        }
    }
}
pub fn recover(path: &Path) -> Result<()> {
    let lock = lock(Some(path))?;
    let mut text = String::new();
    File::open(path)?
        .take(1024 * 1024)
        .read_to_string(&mut text)?;
    let snapshot: Snapshot = toml::from_str(&text)?;
    ensure!(
        snapshot.version == 1 && snapshot.provider == "windows-wininet-lan",
        "unsupported Windows proxy recovery document"
    );
    Guard {
        original: Some(snapshot.original),
        journal: Some(path.to_owned()),
        lock: Some(lock),
    }
    .restore()
}
