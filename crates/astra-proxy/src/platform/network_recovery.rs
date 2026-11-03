//! A separate, same-binary process survives core termination and restores only
//! resources whose ownership was durably confirmed. No command strings are run.
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{BufRead, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Stdio},
    sync::{Mutex, OnceLock},
    time::Duration,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub(super) enum Resource {
    SystemProxy {
        path: PathBuf,
        #[serde(default)]
        snapshot: Option<super::system_proxy::Snapshot>,
    },
    #[cfg(windows)]
    WindowsRoute(super::windows_network::RouteIdentity),
    #[cfg(windows)]
    WindowsAdapter(super::windows_network::AdapterSnapshot),
    #[cfg(not(windows))]
    UnixRoute(super::tun::RouteRecord),
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Item {
    id: String,
    committed: bool,
    resource: Resource,
    error: Option<String>,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u32,
    run: String,
    resources: Vec<Item>,
    #[serde(default)]
    owner: Option<astra_model::control::ProcessIdentity>,
    #[serde(default)]
    boot_id: String,
    #[serde(default)]
    notice: Option<PathBuf>,
    #[serde(default)]
    checkpoint_error: Option<String>,
}
struct Owner {
    path: PathBuf,
    journal: Journal,
    _lock: File,
    child: Child,
    _lifeline: ChildStdin,
}
static OWNER: OnceLock<Mutex<Option<Owner>>> = OnceLock::new();
static PROFILE: OnceLock<PathBuf> = OnceLock::new();
pub fn set_profile(path: &Path) {
    let _ = PROFILE.set(path.to_owned());
}
fn owner() -> &'static Mutex<Option<Owner>> {
    OWNER.get_or_init(|| Mutex::new(None))
}
fn save(path: &Path, journal: &Journal) -> Result<()> {
    astra_platform::storage::write_json(path, journal).context("persist network recovery record")
}
fn open_lock(path: &Path) -> Result<File> {
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "network lifetime lock must be a regular file"
        );
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .mode(0o600);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    validate_file(&file, path)?;
    astra_platform::platform::set_private(path, 0o600)?;
    Ok(file)
}
pub(super) fn validate_file(file: &File, path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        let invoking = (unsafe { libc::geteuid() } == 0)
            .then(|| std::env::var("ASTRA_ELEVATED_OWNER").ok())
            .flatten()
            .and_then(|s| s.split(':').next()?.parse::<u32>().ok());
        ensure!(
            metadata.is_file()
                && metadata.nlink() == 1
                && (metadata.uid() == unsafe { libc::geteuid() }
                    || invoking == Some(metadata.uid())),
            "invalid network recovery owner or hard link: {}",
            path.display()
        );
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT,
        };
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        ensure!(
            unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } != 0,
            "inspect network lock handle: {}",
            std::io::Error::last_os_error()
        );
        ensure!(
            info.nNumberOfLinks == 1 && info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0,
            "network lock cannot be a link/reparse point"
        );
        astra_platform::elevation::validate_user_file(&file, path)?;
    }
    Ok(())
}
pub(super) fn read_private(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    validate_file(&file, path)?;
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "network recovery document exceeds size limit"
    );
    Ok(bytes)
}
pub fn recover_abandoned() -> Result<()> {
    let directory = astra_platform::storage::runtime_root()?.join("network-recovery");
    let metadata = match std::fs::symlink_metadata(&directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "network recovery directory must be a real directory"
    );
    // Recover abandoned records before making a new system change. Live owners
    // are never inspected or disturbed, even if their management IPC is down.
    for entry in std::fs::read_dir(&directory)? {
        let path = entry?.path();
        if path.extension().is_some_and(|v| v == "json") {
            let lock = open_lock(&path.with_extension("lock"))?;
            match lock.try_lock() {
                Ok(()) => recover_locked(&path)?,
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(())
}
pub fn finish(result: Result<()>) -> Result<()> {
    match (result, cleanup_status()) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Err(primary), Err(cleanup)) => {
            Err(anyhow::anyhow!("{primary:#}; additionally, {cleanup:#}"))
        }
    }
}
fn start() -> Result<Owner> {
    let directory = astra_platform::storage::runtime_root()?.join("network-recovery");
    #[cfg(windows)]
    astra_platform::elevation::create_private_directory(&directory)?;
    #[cfg(not(windows))]
    astra_platform::storage::prepare_parent(&directory.join("record"))?;
    recover_abandoned()?;
    let run = uuid::Uuid::new_v4().to_string();
    let path = directory.join(format!("{run}.json"));
    let lock = open_lock(&path.with_extension("lock"))?;
    lock.try_lock().context("claim network lifetime lock")?;
    let notice = PROFILE
        .get()
        .map(|profile| {
            astra_platform::recovery::path(
                profile,
                std::env::var_os("ASTRA_CONTROL_SOCKET")
                    .as_deref()
                    .map(Path::new),
            )
        })
        .transpose()?;
    let journal = Journal {
        version: 1,
        run,
        resources: vec![],
        owner: Some(astra_platform::lifecycle::capture(std::process::id())?),
        boot_id: astra_platform::lifecycle::boot_id().to_owned(),
        notice,
        checkpoint_error: None,
    };
    save(&path, &journal)?;
    let error_path = path.with_extension("log");
    let error_log = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&error_path)?;
    astra_platform::platform::set_private(&error_path, 0o600)?;
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command.env_remove("ASTRA_SERVICE_OWNER");
    command
        .arg("--network-guardian")
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(error_log);
    astra_platform::platform::background_command(&mut command, true);
    let mut child = command.spawn().context("start network recovery guardian")?;
    let lifeline = child.stdin.take().context("guardian lifetime channel")?;
    let stdout = child.stdout.take().context("guardian readiness channel")?;
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut ready = String::new();
        let result = std::io::BufReader::new(stdout)
            .read_line(&mut ready)
            .map(|_| ready);
        let _ = tx.send(result);
    });
    let expected = astra_platform::lifecycle::capture(child.id());
    let ready = rx
        .recv_timeout(Duration::from_secs(5))
        .ok()
        .and_then(|value| value.ok())
        .and_then(|line| {
            line.trim().strip_prefix("READY ").and_then(|identity| {
                serde_json::from_str::<astra_model::control::ProcessIdentity>(identity).ok()
            })
        });
    if !matches!((&expected,&ready),(Ok(expected),Some(actual)) if expected==actual && astra_platform::lifecycle::matches_live(actual).unwrap_or(false))
    {
        let _ = child.kill();
        let _ = child.wait();
        anyhow::bail!("network recovery guardian did not prove its readiness/identity; no system changes made");
    }
    Ok(Owner {
        path,
        journal,
        _lock: lock,
        child,
        _lifeline: lifeline,
    })
}
pub fn ensure() -> Result<()> {
    let mut guard = owner()
        .lock()
        .map_err(|_| anyhow::anyhow!("network recovery coordinator panicked"))?;
    if guard.is_none() {
        *guard = Some(start()?);
    }
    ensure!(
        guard.as_mut().unwrap().child.try_wait()?.is_none(),
        "network recovery guardian exited unexpectedly"
    );
    Ok(())
}
pub fn healthy() -> Result<()> {
    let mut guard = owner()
        .lock()
        .map_err(|_| anyhow::anyhow!("network recovery coordinator panicked"))?;
    if let Some(owner) = guard.as_mut() {
        ensure!(
            owner.child.try_wait()?.is_none(),
            "network recovery guardian exited unexpectedly; recovery record: {}",
            owner.path.display()
        );
    }
    Ok(())
}
fn change_journal(
    journal: &mut Journal,
    change: impl FnOnce(&mut Journal) -> Result<()>,
    checkpoint: impl FnOnce(&Journal) -> Result<()>,
) -> Result<()> {
    let previous = journal.resources.clone();
    if let Err(error) = change(journal) {
        journal.resources = previous;
        return Err(error);
    }
    journal.checkpoint_error = None;
    if let Err(error) = checkpoint(journal) {
        journal.resources = previous;
        journal.checkpoint_error = Some(format!("{error:#}"));
        return Err(error);
    }
    Ok(())
}
pub fn cleanup_status() -> Result<()> {
    let guard = owner()
        .lock()
        .map_err(|_| anyhow::anyhow!("network recovery coordinator panicked"))?;
    if let Some(owner) = guard.as_ref() {
        ensure!(
            owner.journal.checkpoint_error.is_none(),
            "network recovery checkpoint remains pending: {}",
            owner
                .journal
                .checkpoint_error
                .as_deref()
                .unwrap_or_default()
        );
        let failures = owner
            .journal
            .resources
            .iter()
            .filter_map(|r| r.error.as_deref())
            .collect::<Vec<_>>();
        ensure!(
            owner.journal.resources.is_empty(),
            "network restoration remains pending for {} resources in {}: {}",
            owner.journal.resources.len(),
            owner.path.display(),
            failures.join("; ")
        );
    }
    Ok(())
}
/// The resource's actual OS owner retains this lease. Shared routes retain their
/// existing Arc owner across candidate/old/current generations.
#[derive(Debug)]
pub(super) struct Lease {
    id: String,
}
impl Lease {
    pub fn prepare(resource: Resource) -> Result<Self> {
        ensure()?;
        let mut guard = owner()
            .lock()
            .map_err(|_| anyhow::anyhow!("network recovery coordinator panicked"))?;
        let owner = guard
            .as_mut()
            .context("network recovery guardian missing")?;
        let id = uuid::Uuid::new_v4().to_string();
        owner.journal.resources.push(Item {
            id: id.clone(),
            committed: false,
            resource,
            error: None,
        });
        save(&owner.path, &owner.journal)?;
        Ok(Self { id })
    }
    pub fn failed(&self, error: &anyhow::Error) {
        if let Ok(mut guard) = owner().lock() {
            if let Some(owner) = guard.as_mut() {
                if let Some(item) = owner
                    .journal
                    .resources
                    .iter_mut()
                    .find(|item| item.id == self.id)
                {
                    item.error = Some(format!("{error:#}"));
                }
                let _ = save(&owner.path, &owner.journal);
                let _ = record_notice(&owner.path, &owner.journal);
            }
        }
    }
    pub fn commit(&self) -> Result<()> {
        self.update(false)
    }
    #[cfg(not(windows))]
    pub fn commit_resource(&self, resource: Resource) -> Result<()> {
        let mut guard = owner()
            .lock()
            .map_err(|_| anyhow::anyhow!("network recovery coordinator panicked"))?;
        let owner = guard
            .as_mut()
            .context("network recovery guardian missing")?;
        let path = owner.path.clone();
        change_journal(
            &mut owner.journal,
            |journal| {
                let item = journal
                    .resources
                    .iter_mut()
                    .find(|item| item.id == self.id)
                    .context("recovery resource missing")?;
                item.resource = resource;
                item.committed = true;
                Ok(())
            },
            |journal| save(&path, journal),
        )
    }

    pub fn complete(&self) -> Result<()> {
        self.update(true)
    }
    fn update(&self, remove: bool) -> Result<()> {
        let mut guard = owner()
            .lock()
            .map_err(|_| anyhow::anyhow!("network recovery coordinator panicked"))?;
        let owner = guard
            .as_mut()
            .context("network recovery guardian missing")?;
        let path = owner.path.clone();
        change_journal(
            &mut owner.journal,
            |journal| {
                if remove {
                    journal.resources.retain(|item| item.id != self.id);
                } else {
                    journal
                        .resources
                        .iter_mut()
                        .find(|item| item.id == self.id)
                        .context("recovery resource missing")?
                        .committed = true;
                }
                Ok(())
            },
            |journal| save(&path, journal),
        )
    }
}
pub fn recover(path: &Path) -> Result<()> {
    let lock = open_lock(&path.with_extension("lock"))?;
    lock.try_lock()
        .context("original network owner is still alive or recovery is already running")?;
    recover_locked(path)
}
fn recover_locked(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "recovery document must be a regular file"
    );
    let mut journal: Journal = serde_json::from_slice(&read_private(path, 4 * 1024 * 1024)?)?;
    ensure!(
        journal.version == 1 && path.file_stem().is_some_and(|v| v == journal.run.as_str()),
        "invalid network recovery identity/version"
    );
    if let Some(owner) = &journal.owner {
        ensure!(
            !astra_platform::lifecycle::matches_live(owner)
                .context("verify network owner has exited")?,
            "original network owner is still alive; refusing recovery"
        );
    }
    let restored = restore_items(path, &mut journal, |item| match &item.resource {
        Resource::SystemProxy { path, snapshot } => match std::fs::symlink_metadata(path) {
            Ok(metadata) => {
                ensure!(
                    metadata.is_file() && !metadata.file_type().is_symlink(),
                    "system proxy snapshot must be a regular file"
                );
                if let Some(snapshot) = snapshot {
                    super::system_proxy::recover_snapshot(path, snapshot)
                } else {
                    super::system_proxy::recover_owned(path)
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let snapshot = snapshot
                    .as_ref()
                    .context("system proxy snapshot is missing; recovery remains pending")?;
                super::system_proxy::recover_snapshot(path, snapshot)
            }
            Err(error) => Err(error.into()),
        },
        #[cfg(windows)]
        Resource::WindowsRoute(route) => {
            super::windows_network::recover_route(route, item.committed)
        }
        #[cfg(windows)]
        Resource::WindowsAdapter(adapter) => {
            super::windows_network::recover_adapter(adapter, item.committed)
        }
        #[cfg(not(windows))]
        Resource::UnixRoute(route) => super::tun::recover_route(route, item.committed),
    });
    if let Err(primary) = restored {
        return match record_notice(path, &journal) {
            Ok(()) => Err(primary),
            Err(notice) => Err(anyhow::anyhow!(
                "{primary:#}; recovery notice also failed: {notice:#}"
            )),
        };
    }
    if !journal.resources.is_empty() {
        record_notice(path, &journal)?;
    }
    ensure!(
        journal.resources.is_empty(),
        "network recovery remains pending in {}: {}",
        path.display(),
        journal
            .resources
            .iter()
            .filter_map(|r| r.error.as_deref())
            .collect::<Vec<_>>()
            .join("; ")
    );
    let completion = (|| {
        std::fs::remove_file(path)?;
        astra_platform::storage::sync_directory(path.parent().context("recovery directory")?)?;
        record_notice(path, &journal)
    })();
    if let Err(error) = completion {
        // Preserve an empty retryable record when deleting/syncing the journal
        // or persisting its final outcome fails. The OS cleanup is idempotent.
        journal.checkpoint_error = Some(format!("finalize network recovery: {error:#}"));
        let checkpoint = save(path, &journal);
        let notice = record_notice(path, &journal);
        let mut message = format!("finalize network recovery: {error:#}");
        if let Err(error) = checkpoint {
            message.push_str(&format!("; retain recovery record: {error:#}"));
        }
        if let Err(error) = notice {
            message.push_str(&format!("; persist recovery notice: {error:#}"));
        }
        anyhow::bail!(message);
    }
    Ok(())
}
fn record_notice(path: &Path, journal: &Journal) -> Result<()> {
    let Some(destination) = &journal.notice else {
        return Ok(());
    };
    use astra_platform::recovery::{Notice, RecoveryState};
    let errors = journal
        .resources
        .iter()
        .filter_map(|r| r.error.as_deref())
        .collect::<Vec<_>>();
    let state = if journal.checkpoint_error.is_some() {
        RecoveryState::Pending
    } else if journal.resources.is_empty() {
        RecoveryState::Recovered
    } else if errors
        .iter()
        .any(|e| e.contains("externally") || e.contains("unknown") || e.contains("identity"))
    {
        RecoveryState::Conflict
    } else {
        RecoveryState::Pending
    };
    let message = if let Some(error) = &journal.checkpoint_error {
        format!("Network cleanup checkpoint failed; retry the retained recovery record: {error}")
    } else if journal.resources.is_empty() {
        "Owned network settings restored".to_owned()
    } else {
        format!(
            "{} network resources require recovery: {}",
            journal.resources.len(),
            errors.join("; ")
        )
    }
    .chars()
    .take(8_000)
    .collect::<String>();
    astra_platform::recovery::write(
        destination,
        &Notice {
            state,
            timestamp_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .min(u64::MAX as u128) as u64,
            message,
            journal: path.to_owned(),
            boot_id: journal.boot_id.clone(),
        },
    )?;
    Ok(())
}
fn restore_items(
    path: &Path,
    journal: &mut Journal,
    restore: impl FnMut(&Item) -> Result<()>,
) -> Result<()> {
    restore_items_with(journal, restore, |journal| save(path, journal))
}
fn restore_items_with(
    journal: &mut Journal,
    mut restore: impl FnMut(&Item) -> Result<()>,
    mut checkpoint: impl FnMut(&Journal) -> Result<()>,
) -> Result<()> {
    journal.checkpoint_error = None;
    let mut errors = vec![];
    for index in (0..journal.resources.len()).rev() {
        match restore(&journal.resources[index]) {
            Ok(()) => {
                journal.resources.remove(index);
            }
            Err(error) => journal.resources[index].error = Some(format!("{error:#}")),
        }
        if let Err(error) = checkpoint(journal) {
            errors.push(format!("{error:#}"));
        }
    }
    // An empty journal still needs a durable checkpoint before it can be removed.
    if let Err(error) = checkpoint(journal) {
        errors.push(format!("{error:#}"));
    }
    if !errors.is_empty() {
        let message = errors.join("; ");
        journal.checkpoint_error = Some(message.clone());
        let _ = checkpoint(journal);
        anyhow::bail!("network restoration checkpoints failed: {message}");
    }
    Ok(())
}
pub(super) fn entry() -> Option<Result<()>> {
    let mut args = std::env::args_os().skip(1);
    let mode = args.next()?;
    if mode != "--network-guardian" && mode != "--recover-network" {
        return None;
    }
    Some((|| {
        let path = PathBuf::from(args.next().context("expected network recovery record")?);
        ensure!(args.next().is_none(), "unexpected recovery argument");
        if mode == "--recover-network" {
            recover(&path)?;
            println!("Owned network settings restored: {}", path.display());
            return Ok(());
        }
        guardian(&path)
    })())
}

fn guardian(path: &Path) -> Result<()> {
    let lock = open_lock(&path.with_extension("lock"))?;
    match lock.try_lock() {
        Err(std::fs::TryLockError::WouldBlock) => {}
        Ok(()) => anyhow::bail!("guardian requires a live network owner"),
        Err(error) => return Err(error.into()),
    }
    let journal: Journal = serde_json::from_slice(&read_private(path, 4 * 1024 * 1024)?)?;
    ensure!(
        journal.version == 1
            && path.file_stem().is_some_and(|v| v == journal.run.as_str())
            && !journal.boot_id.is_empty(),
        "guardian journal identity invalid"
    );
    let owner = journal
        .owner
        .as_ref()
        .context("guardian requires owner identity")?;
    ensure!(
        owner.pid != std::process::id() && astra_platform::lifecycle::matches_live(owner)?,
        "guardian owner identity is no longer live"
    );
    println!(
        "READY {}",
        serde_json::to_string(&astra_platform::lifecycle::capture(std::process::id())?)?
    );
    std::io::stdout().flush()?;
    let _ = std::io::copy(&mut std::io::stdin(), &mut std::io::sink());
    lock.lock().context("wait for network owner death")?;
    // Windows releases file locks during process teardown slightly before its
    // process handle becomes signalled. Wait for that independent death proof;
    // neither an EOF nor acquiring the file lock alone authorizes restoration.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while astra_platform::lifecycle::matches_live(owner)? {
        ensure!(
            std::time::Instant::now() < deadline,
            "network owner still alive after releasing lifetime lock; recovery retained"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    recover_locked(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_documents_are_retained() {
        let directory =
            std::env::temp_dir().join(format!("astra-network-journal-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("run.json");
        std::fs::write(&path, br#"{"version":999,"run":"run","resources":[]}"#).unwrap();
        assert!(recover(&path).is_err());
        assert!(path.exists());
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn live_owner_is_never_recovered() {
        let directory =
            std::env::temp_dir().join(format!("astra-network-owner-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("run.json");
        save(
            &path,
            &Journal {
                version: 1,
                run: "run".into(),
                resources: vec![],
                ..Default::default()
            },
        )
        .unwrap();
        let lock = open_lock(&path.with_extension("lock")).unwrap();
        lock.lock().unwrap();
        assert!(recover(&path).is_err());
        assert!(path.exists());
        drop(lock);
        recover(&path).unwrap();
        assert!(!path.exists());
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn recovery_is_reverse_order_and_retains_only_failed_resources() {
        let directory =
            std::env::temp_dir().join(format!("astra-network-steps-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("run.json");
        let mut journal = Journal {
            version: 1,
            run: "run".into(),
            resources: (0..3)
                .map(|i| Item {
                    id: i.to_string(),
                    committed: true,
                    resource: Resource::SystemProxy {
                        path: PathBuf::from("unused"),
                        snapshot: None,
                    },
                    error: None,
                })
                .collect(),
            ..Default::default()
        };
        save(&path, &journal).unwrap();
        let mut attempted = vec![];
        restore_items(&path, &mut journal, |item| {
            attempted.push(item.id.clone());
            ensure!(item.id != "1", "injected restore error");
            Ok(())
        })
        .unwrap();
        assert_eq!(attempted, ["2", "1", "0"]);
        let persisted: Journal = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(persisted.resources.len(), 1);
        assert_eq!(persisted.resources[0].id, "1");
        assert!(persisted.resources[0]
            .error
            .as_ref()
            .unwrap()
            .contains("injected"));
        restore_items(&path, &mut journal, |_| Ok(())).unwrap();
        assert!(journal.resources.is_empty());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    #[ignore = "subprocess entrypoint"]
    fn guardian_test_child() {
        if let Some(path) = std::env::var_os("ASTRA_GUARDIAN_TEST_PATH") {
            guardian(Path::new(&path)).unwrap();
            std::process::exit(0);
        }
    }
    #[test]
    #[ignore = "subprocess entrypoint"]
    fn owner_test_child() {
        let Some(path) = std::env::var_os("ASTRA_GUARDIAN_OWNER_TEST_PATH") else {
            return;
        };
        let path = PathBuf::from(path);
        let lock = open_lock(&path.with_extension("lock")).unwrap();
        lock.lock().unwrap();
        let mut journal: Journal = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        journal.owner = Some(astra_platform::lifecycle::capture(std::process::id()).unwrap());
        journal.boot_id = astra_platform::lifecycle::boot_id().to_owned();
        save(&path, &journal).unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "platform::network_recovery::tests::guardian_test_child",
                "--ignored",
                "--nocapture",
            ])
            .env("ASTRA_GUARDIAN_TEST_PATH", &path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        for line in std::io::BufReader::new(output).lines() {
            if line.unwrap().contains("READY") {
                println!("OWNER_READY");
                std::io::stdout().flush().unwrap();
                break;
            }
        }
        std::hint::black_box((&lock, &input, &child));
        loop {
            std::thread::park();
        }
    }
    #[test]
    fn guardian_survives_forced_owner_exit_without_network_mutation() {
        let directory =
            std::env::temp_dir().join(format!("astra-network-crash-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("run.json");
        save(
            &path,
            &Journal {
                version: 1,
                run: "run".into(),
                resources: vec![],
                ..Default::default()
            },
        )
        .unwrap();
        let mut owner = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "platform::network_recovery::tests::owner_test_child",
                "--ignored",
                "--nocapture",
            ])
            .env("ASTRA_GUARDIAN_OWNER_TEST_PATH", &path)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let output = owner.stdout.take().unwrap();
        let (sender, ready) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(output).lines() {
                if line.unwrap_or_default().contains("OWNER_READY") {
                    let _ = sender.send(());
                    break;
                }
            }
        });
        let started = ready.recv_timeout(Duration::from_secs(10));
        let _ = owner.kill();
        let _ = owner.wait();
        started.unwrap();
        let until = std::time::Instant::now() + Duration::from_secs(5);
        while path.exists() && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !path.exists(),
            "independent guardian did not finish recovery after owner was killed"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn hard_linked_lock_is_rejected_without_modifying_target() {
        let directory =
            std::env::temp_dir().join(format!("astra-network-links-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let target = directory.join("target");
        let link = directory.join("lock");
        std::fs::write(&target, b"keep").unwrap();
        std::fs::hard_link(&target, &link).unwrap();
        assert!(open_lock(&link).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn failed_checkpoints_do_not_skip_remaining_network_restoration() {
        let mut journal = Journal {
            version: 1,
            run: "run".into(),
            resources: (0..3)
                .map(|i| Item {
                    id: i.to_string(),
                    committed: true,
                    resource: Resource::SystemProxy {
                        path: PathBuf::from("unused"),
                        snapshot: None,
                    },
                    error: None,
                })
                .collect(),
            ..Default::default()
        };
        let original = serde_json::to_vec(&journal).unwrap();
        let mut restored = vec![];
        let result = restore_items_with(
            &mut journal,
            |item| {
                restored.push(item.id.clone());
                Ok(())
            },
            |_| anyhow::bail!("injected disk full"),
        );
        assert!(result.is_err());
        assert_eq!(restored, ["2", "1", "0"]);
        assert!(journal.checkpoint_error.is_some());
        let durable: Journal = serde_json::from_slice(&original).unwrap();
        assert_eq!(durable.resources.len(), 3);
        restore_items_with(
            &mut journal,
            |_| panic!("already-restored operation was repeated"),
            |_| Ok(()),
        )
        .unwrap();
        assert!(journal.checkpoint_error.is_none());
    }
    #[test]
    fn failed_commit_or_release_checkpoint_retains_resource_ownership() {
        let mut journal = Journal {
            resources: vec![Item {
                id: "route".into(),
                committed: false,
                resource: Resource::SystemProxy {
                    path: PathBuf::from("unused"),
                    snapshot: None,
                },
                error: None,
            }],
            ..Default::default()
        };
        assert!(change_journal(
            &mut journal,
            |j| {
                j.resources[0].committed = true;
                Ok(())
            },
            |_| anyhow::bail!("injected commit failure")
        )
        .is_err());
        assert!(!journal.resources[0].committed);
        assert!(journal.checkpoint_error.is_some());
        assert!(change_journal(
            &mut journal,
            |j| {
                j.resources.clear();
                Ok(())
            },
            |_| anyhow::bail!("injected cleanup failure")
        )
        .is_err());
        assert_eq!(journal.resources.len(), 1);
        change_journal(
            &mut journal,
            |j| {
                j.resources.clear();
                Ok(())
            },
            |_| Ok(()),
        )
        .unwrap();
        assert!(journal.resources.is_empty());
        assert!(journal.checkpoint_error.is_none());
    }
}
