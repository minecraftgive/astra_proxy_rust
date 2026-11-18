//! One journal entry per DNS property. The marker serializes instances even
//! across runtime directories and binds delayed recovery to its original write.
use super::{
    allowed, args, flush, mac_values, resolved_values, run, search, Change, NETWORKSETUP,
    RESOLVECTL,
};
use crate::platform::network_recovery::{self, Lease, Resource};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Seek, Write},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Setting {
    change: Change,
    identity: Identity,
    token: uuid::Uuid,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
enum Identity {
    MacService(String),
    ResolvedLink {
        boot: String,
        namespace: u64,
        index: u32,
    },
    WindowsAdapter(uuid::Uuid),
}

#[cfg(target_os = "linux")]
fn link_location() -> Result<(String, u64)> {
    use std::os::unix::fs::MetadataExt;
    Ok((
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?
            .trim()
            .to_owned(),
        std::fs::metadata("/proc/self/ns/net")?.ino(),
    ))
}

impl Change {
    fn key_len(&self) -> Result<usize> {
        ensure!(
            allowed(&self.apply) && allowed(&self.restore),
            "unsupported DNS setting"
        );
        let n = match self.apply[0].as_str() {
            NETWORKSETUP if cfg!(target_os = "macos") => 3,
            RESOLVECTL if cfg!(target_os = "linux") => 3,
            search::WIN_SET | search::WIN_SERVERS_SET if cfg!(windows) => 2,
            _ => bail!("DNS journal belongs to a different platform"),
        };
        ensure!(
            self.apply.get(..n) == self.restore.get(..n),
            "DNS snapshot targets differ"
        );
        Ok(n)
    }

    pub(super) fn current(&self) -> Result<Option<Vec<String>>> {
        let n = self.key_len()?;
        let a = &self.apply;
        let mut current = a[..n].to_vec();
        let values = match a[0].as_str() {
            NETWORKSETUP => {
                let (op, kind) = if a[1] == "-setdnsservers" {
                    ("-getdnsservers", "DNS Servers")
                } else {
                    ("-getsearchdomains", "Search Domains")
                };
                mac_values(&a[2], kind, &run(&args(&[NETWORKSETUP, op, &a[2]]))?)?
            }
            RESOLVECTL => resolved_values(&run(&current)?)?,
            _ => {
                let raw = run(&args(&[search::WIN_BY_GUID, &a[1]]))?;
                let state: Option<(uuid::Uuid, Vec<std::net::Ipv4Addr>, String)> =
                    serde_json::from_str(raw.trim())?;
                let Some((id, ips, suffix)) = state else {
                    return Ok(None);
                };
                ensure!(
                    id == uuid::Uuid::parse_str(&a[1])?,
                    "DNS adapter identity changed"
                );
                if a[0] == search::WIN_SET {
                    vec![suffix]
                } else {
                    ips.iter().map(ToString::to_string).collect()
                }
            }
        };
        current.extend(values);
        ensure!(
            allowed(&current),
            "unrecognized live DNS settings; preserving them"
        );
        Ok(Some(current))
    }

    fn identity(&self) -> Result<Option<Identity>> {
        self.key_len()?;
        #[cfg(target_os = "macos")]
        return Ok(Some(Identity::MacService(search::mac_service_id(
            &self.apply[2],
        )?)));
        #[cfg(target_os = "linux")]
        {
            let link = std::ffi::CString::new(self.apply[2].as_str())?;
            let index = unsafe { libc::if_nametoindex(link.as_ptr()) };
            if index == 0 {
                let error = std::io::Error::last_os_error();
                if matches!(error.raw_os_error(), Some(libc::ENXIO | libc::ENODEV)) {
                    return Ok(None);
                }
                return Err(error.into());
            }
            let (boot, namespace) = link_location()?;
            Ok(Some(Identity::ResolvedLink {
                boot,
                namespace,
                index,
            }))
        }
        #[cfg(windows)]
        return Ok(Some(Identity::WindowsAdapter(uuid::Uuid::parse_str(
            &self.apply[1],
        )?)));
        #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
        bail!("unsupported DNS platform")
    }
}

impl Setting {
    fn lock_path(&self) -> Result<PathBuf> {
        self.change.key_len()?;
        let property = if cfg!(windows) {
            &self.change.apply[0]
        } else {
            &self.change.apply[1]
        };
        let key = Sha256::digest(serde_json::to_vec(&(&self.identity, property))?);
        #[cfg(unix)]
        let root = PathBuf::from("/var/tmp");
        #[cfg(windows)]
        let root = astra_platform::platform::program_data_dir()?;
        Ok(root.join(format!("astra-dns-{key:x}.lock")))
    }

    fn present(&self) -> Result<bool> {
        #[cfg(target_os = "linux")]
        if let Identity::ResolvedLink {
            boot,
            namespace,
            index,
        } = &self.identity
        {
            let (live_boot, live_ns) = link_location()?;
            if *boot != live_boot {
                return Ok(false);
            }
            ensure!(
                live_ns == *namespace,
                "DNS recovery requires the original network namespace"
            );
            let mut name = [0; libc::IF_NAMESIZE];
            if unsafe { libc::if_indextoname(*index, name.as_mut_ptr()) }.is_null() {
                let error = std::io::Error::last_os_error();
                if matches!(error.raw_os_error(), Some(libc::ENXIO | libc::ENODEV)) {
                    return Ok(false);
                }
                return Err(error.into());
            }
        }
        ensure!(
            self.change.identity()?.as_ref() == Some(&self.identity),
            "DNS resource identity changed; manual review required"
        );
        Ok(true)
    }
    fn restore(&self) -> Result<()> {
        if !self.present()? {
            return Ok(());
        }
        let Some(current) = self.change.current()? else {
            return Ok(());
        };
        restore_if_owned(&self.change, &current, |cmd| {
            run(cmd)?;
            let actual = self.change.current()?;
            ensure!(
                actual.as_ref().is_none_or(|v| v == cmd),
                "DNS restoration readback failed"
            );
            Ok(())
        })
    }

    pub(crate) fn recover(&self) -> Result<()> {
        let path = self.lock_path()?;
        let Some((file, owner)) = lock(&path, false)? else {
            // The marker is removed only after rollback, or has not yet been
            // created. Never replay a setting without independent ownership.
            ensure!(
                !self.present()?
                    || self
                        .change
                        .current()?
                        .is_none_or(|v| v == self.change.restore),
                "DNS ownership marker missing; manual review required"
            );
            return Ok(());
        };
        if owner.is_empty() {
            // Preparation precedes arming the marker; an incomplete clear or
            // missing token is not evidence that we may overwrite live state.
            ensure!(
                !self.present()?
                    || self
                        .change
                        .current()?
                        .is_none_or(|v| v == self.change.restore),
                "DNS ownership unconfirmed"
            );
        } else {
            ensure!(
                owner == self.token.to_string(),
                "another DNS takeover owns this setting"
            );
            self.restore()?;
        }
        release(&file, &path)?;
        flush();
        Ok(())
    }
}

fn restore_if_owned(
    change: &Change,
    current: &[String],
    write: impl FnOnce(&[String]) -> Result<()>,
) -> Result<()> {
    if current == change.restore {
        return Ok(());
    }
    ensure!(
        current == change.apply,
        "DNS setting changed externally; preserving it"
    );
    write(&change.restore)
}

// Opening an existing marker never changes its contents or permissions. Unix
// checks the link count after acquiring the lock to reject an unlinked inode.
fn lock(path: &Path, create: bool) -> Result<Option<(File, String)>> {
    #[cfg(unix)]
    let opened = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(path)
    };
    #[cfg(windows)]
    let opened = astra_platform::elevation::open_dns_marker(path, create);
    let mut file = match opened {
        Ok(file) => file,
        Err(error) if !create && error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    file.try_lock()
        .context("another Astra instance owns this DNS property")?;
    network_recovery::validate_file(&file, path)?;
    let mut token = String::new();
    (&mut file).take(64).read_to_string(&mut token)?;
    Ok(Some((file, token)))
}

fn release(held: &File, path: &Path) -> Result<()> {
    // Keep the token intact until unlink succeeds, including on I/O errors.
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let owned = held.metadata()?;
        let current = std::fs::symlink_metadata(path)?;
        ensure!(
            current.dev() == owned.dev() && current.ino() == owned.ino(),
            "DNS marker replaced"
        );
        std::fs::remove_file(path)?;
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            FileDispositionInfo, SetFileInformationByHandle, FILE_DISPOSITION_INFO,
        };
        let info = FILE_DISPOSITION_INFO { DeleteFile: true };
        if unsafe {
            SetFileInformationByHandle(
                held.as_raw_handle(),
                FileDispositionInfo,
                (&raw const info).cast(),
                std::mem::size_of_val(&info) as u32,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    astra_platform::storage::sync_directory(path.parent().context("DNS lock directory")?)?;
    Ok(())
}

pub(super) struct Active {
    setting: Setting,
    lease: Lease,
    file: File,
    path: PathBuf,
}
impl Active {
    pub(super) fn apply(change: Change) -> Result<Self> {
        let identity = change.identity()?.context("DNS target vanished")?;
        let setting = Setting {
            change,
            identity,
            token: uuid::Uuid::new_v4(),
        };
        let path = setting.lock_path()?;
        // Recover abandoned records before claiming this setting's lock.
        network_recovery::ensure()?;
        let (mut file, token) = lock(&path, true)?.context("could not create DNS lock")?;
        ensure!(token.is_empty(), "previous DNS restoration remains pending");
        // Keep empty markers out of the shared directory on preparation
        // failures. Until the setter is called there is no OS change to undo.
        let prepared = (|| -> Result<Lease> {
            ensure!(
                setting.present()?
                    && setting.change.current()?.as_ref() == Some(&setting.change.restore),
                "DNS settings changed during preparation"
            );
            let lease = Lease::prepare(Resource::DnsSetting(setting.clone()))?;
            if network_recovery::enabled() {
                let saved = (|| -> Result<()> {
                    file.rewind()?;
                    file.write_all(setting.token.to_string().as_bytes())?;
                    file.sync_all()?;
                    astra_platform::storage::sync_directory(
                        path.parent().context("DNS lock directory")?,
                    )?;
                    Ok(())
                })();
                if let Err(error) = saved {
                    lease
                        .complete()
                        .context("release unapplied DNS journal entry")?;
                    return Err(error);
                }
            }
            Ok(lease)
        })();
        let lease = match prepared {
            Ok(lease) => lease,
            Err(error) => {
                release(&file, &path)
                    .with_context(|| format!("release unused DNS marker after {error:#}"))?;
                return Err(error);
            }
        };
        let active = Self {
            setting,
            lease,
            file,
            path,
        };
        let change = &active.setting.change;
        if change.apply != change.restore {
            run(&change.apply)?;
            ensure!(
                change.current()?.as_ref() == Some(&change.apply),
                "DNS takeover readback failed"
            );
        }
        active.lease.commit()?;
        Ok(active)
    }
}
impl Drop for Active {
    fn drop(&mut self) {
        let restored = self.setting.restore();
        // A failed recorded change needs its token for retry. An unrecorded
        // failure is reported by finish() and has no guardian to retry it.
        let release_marker = restored.is_ok() || !network_recovery::enabled();
        let result = if release_marker {
            release(&self.file, &self.path).and(restored)
        } else {
            restored
        };
        if let Err(error) = result.and_then(|()| self.lease.complete()) {
            self.lease.failed(&error);
            tracing::error!(%error, "DNS restoration incomplete");
        }
    }
}
