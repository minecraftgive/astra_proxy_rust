//! Durable local maintenance state shared by the manager, helper and frontends.
use crate::{config::ManagerConfig, v2::InstanceKind};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
};

pub const CAPABILITY: &str = "maintenance-restart-v1";
pub const LIMIT: u64 = 4 * 1024 * 1024;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Prepared,
    Stopping,
    UpdatingService,
    Starting,
    Restoring,
    Ready,
    Complete,
    Failed,
    Cancelled,
}
impl Phase {
    pub fn terminal(self) -> bool {
        matches!(self, Self::Complete | Self::Cancelled)
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Prepared => "Maintenance prepared",
            Self::Stopping => "Stopping the old backend",
            Self::UpdatingService => "Updating the privilege service",
            Self::Starting => "Starting the new backend",
            Self::Restoring => "Restoring running instances",
            Self::Ready => "Backend updated; switching interface",
            Self::Complete => "Maintenance restart complete",
            Self::Failed => "Maintenance failed; manual action required",
            Self::Cancelled => "Maintenance cancelled",
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Instance {
    pub name: String,
    pub kind: InstanceKind,
    pub config: PathBuf,
    pub config_revision: String,
    pub process: astra_platform::lifecycle::ProcessIdentity,
    pub runtime: Option<astra_model::runtime::Overrides>,
}
impl Instance {
    pub fn endpoint(&self, home: &Path) -> PathBuf {
        if self.kind == InstanceKind::Server {
            home.join(format!("server-{}.sock", self.name))
        } else {
            home.join(format!("{}.sock", self.name))
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub version: u32,
    pub operation_id: String,
    pub checkpoint_id: String,
    pub phase: Phase,
    pub release_id: String,
    pub target: PathBuf,
    pub manifest_sha256: String,
    pub coordinator: PathBuf,
    pub coordinator_sha256: String,
    pub helper_sha256: String,
    pub previous: PathBuf,
    pub previous_manifest_sha256: Option<String>,
    pub old_epoch: String,
    pub old_process: astra_platform::lifecycle::ProcessIdentity,
    pub config: ManagerConfig,
    pub instances: Vec<Instance>,
    #[serde(default)]
    pub frontends: Vec<Frontend>,
    #[serde(default)]
    pub managed_processes: Vec<astra_platform::lifecycle::ProcessIdentity>,
    pub helper: Option<astra_platform::lifecycle::ProcessIdentity>,
    pub new_process: Option<astra_platform::lifecycle::ProcessIdentity>,
    pub new_epoch: Option<String>,
    pub restored: Vec<String>,
    pub error: Option<String>,
    #[serde(default)]
    pub failed_phase: Option<Phase>,
    pub service_update: bool,
    #[serde(default)]
    pub candidates: std::collections::BTreeMap<String, astra_platform::lifecycle::ProcessIdentity>,
}
impl Record {
    pub fn cancellable(&self) -> bool {
        self.phase == Phase::Prepared
            || (self.phase == Phase::Failed && self.failed_phase == Some(Phase::Prepared))
    }
}
pub fn directory(home: &Path) -> PathBuf {
    home.join("maintenance")
}
pub fn read(home: &Path) -> Result<Option<Record>> {
    let path = directory(home).join("current.json");
    let bytes = match read_plain(&path, LIMIT) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let record: Record =
        serde_json::from_slice(&bytes).context("Read maintenance recovery record")?;
    ensure!(
        record.version == 1 && record.config.state_dir == home,
        "Invalid maintenance recovery record"
    );
    ensure!(
        !record.operation_id.is_empty()
            && record
                .operation_id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-'),
        "Invalid maintenance operation ID"
    );
    Ok(Some(record))
}
pub fn write(record: &Record) -> Result<()> {
    let directory = directory(&record.config.state_dir);
    astra_platform::storage::private_dir(&directory)?;
    astra_platform::storage::atomic_write_default(
        &directory.join("current.json"),
        &serde_json::to_vec_pretty(record)?,
    )?;
    Ok(())
}
pub fn lock(home: &Path) -> Result<File> {
    let dir = directory(home);
    astra_platform::storage::private_dir(&dir)?;
    let path = dir.join("operation.lock");
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    if let Ok(metadata) = std::fs::symlink_metadata(&path) {
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "Invalid maintenance lock"
        );
    }
    let file = options.open(path)?;
    file.try_lock()
        .context("Another maintenance operation is running")?;
    Ok(file)
}
pub fn pending(home: &Path) -> Result<bool> {
    Ok(read(home)?.is_some_and(|r| !r.phase.terminal()))
}

pub fn request_cancel(home: &Path, id: &str) -> Result<()> {
    astra_platform::storage::atomic_write_default(
        &directory(home).join("cancel.json"),
        &serde_json::to_vec(id)?,
    )?;
    Ok(())
}
pub fn cancelled(record: &Record) -> Result<bool> {
    let path = directory(&record.config.state_dir).join("cancel.json");
    match astra_platform::storage::read_bounded(&path, 1024) {
        Ok(bytes) => Ok(serde_json::from_slice::<String>(&bytes)? == record.operation_id),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn read_plain(path: &Path, limit: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    for entry in [path.parent().unwrap_or(path), path] {
        let metadata = std::fs::symlink_metadata(entry)?;
        if metadata.file_type().is_symlink() {
            return Err(std::io::Error::other(
                "Maintenance state cannot be a symbolic link",
            ));
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 {
                return Err(std::io::Error::other(
                    "Maintenance state cannot be a reparse point",
                ));
            }
        }
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("Maintenance state is not a file"));
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(std::io::Error::other("Maintenance state exceeds limit"));
    }
    Ok(bytes)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Frontend {
    pub process: astra_platform::lifecycle::ProcessIdentity,
    pub executable: PathBuf,
}
pub struct FrontendLease {
    path: PathBuf,
}
impl Drop for FrontendLease {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
pub fn register_frontend(home: &Path, executable: &Path) -> Result<FrontendLease> {
    let directory = directory(home).join("frontends");
    astra_platform::storage::private_dir(&directory)?;
    let process = astra_platform::lifecycle::capture(std::process::id())?;
    let path = directory.join(format!("{}.json", process.pid));
    astra_platform::storage::atomic_write_default(
        &path,
        &serde_json::to_vec(&Frontend {
            process,
            executable: executable.into(),
        })?,
    )?;
    Ok(FrontendLease { path })
}
pub fn frontends(home: &Path) -> Result<Vec<Frontend>> {
    let directory = directory(home).join("frontends");
    if !directory.try_exists()? {
        return Ok(Vec::new());
    }
    let mut result = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        ensure!(result.len() < 256, "Too many maintenance interfaces");
        let frontend: Frontend = serde_json::from_slice(&read_plain(&entry?.path(), 16384)?)?;
        if astra_platform::lifecycle::matches_live(&frontend.process)? {
            result.push(frontend);
        }
    }
    Ok(result)
}
fn acknowledgement(home: &Path, process: &astra_platform::lifecycle::ProcessIdentity) -> PathBuf {
    directory(home)
        .join("checkpoints")
        .join(format!("{}.json", process.pid))
}
pub fn acknowledge(record: &Record) -> Result<()> {
    let own = astra_platform::lifecycle::capture(std::process::id())?;
    ensure!(
        record.phase == Phase::Prepared,
        "Maintenance checkpoint phase changed"
    );
    astra_platform::storage::private_dir(&directory(&record.config.state_dir).join("checkpoints"))?;
    astra_platform::storage::atomic_write_default(
        &acknowledgement(&record.config.state_dir, &own),
        &serde_json::to_vec(&(record.checkpoint_id.clone(), own))?,
    )?;
    Ok(())
}
pub fn checkpointed(
    record: &Record,
    process: &astra_platform::lifecycle::ProcessIdentity,
) -> Result<bool> {
    if !astra_platform::lifecycle::matches_live(process)? {
        return Ok(true);
    }
    match read_plain(&acknowledgement(&record.config.state_dir, process), 16384) {
        Ok(bytes) => {
            let (operation, identity): (String, astra_platform::lifecycle::ProcessIdentity) =
                serde_json::from_slice(&bytes)?;
            Ok(operation == record.checkpoint_id && identity == *process)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// The coordinator remains usable for manual recovery when another payload file
/// is damaged. Only its recorded executable hash authorizes running this helper.
pub fn coordinator(record: &Record) -> Result<PathBuf> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let binary = record
        .coordinator
        .join(astra_platform::platform::executable("astra-host"));
    let mut file =
        astra_platform::installation::open_package_file(&binary)?.take(512 * 1024 * 1024 + 1);
    let mut hash = Sha256::new();
    let size = std::io::copy(&mut file, &mut hash)?;
    ensure!(
        size <= 512 * 1024 * 1024 && format!("{:x}", hash.finalize()) == record.helper_sha256,
        "Maintenance helper executable changed; reinstall the complete package before recovery"
    );
    Ok(binary)
}

#[cfg(test)]
#[path = "test/maintenance_test.rs"]
mod tests;

#[derive(Clone, Serialize, Deserialize)]
struct Switch {
    operation: String,
    previous: astra_platform::lifecycle::ProcessIdentity,
    current: Option<astra_platform::lifecycle::ProcessIdentity>,
    executable: PathBuf,
    error: Option<String>,
}
fn switch_path(record: &Record, pid: u32) -> PathBuf {
    directory(&record.config.state_dir)
        .join("interfaces")
        .join(format!("{pid}.json"))
}
fn write_switch(record: &Record, switch: &Switch) -> Result<()> {
    let path = switch_path(record, switch.previous.pid);
    astra_platform::storage::private_dir(path.parent().unwrap())?;
    astra_platform::storage::atomic_write_default(&path, &serde_json::to_vec(switch)?)?;
    Ok(())
}
pub fn begin_switch(home: &Path, executable: &Path) -> Result<Option<String>> {
    let Some(record) = read(home)?.filter(|r| r.phase == Phase::Ready) else {
        return Ok(None);
    };
    ensure!(
        executable.parent() == Some(record.target.as_path()),
        "Interface target changed during maintenance"
    );
    let switch = Switch {
        operation: record.checkpoint_id.clone(),
        previous: astra_platform::lifecycle::capture(std::process::id())?,
        current: None,
        executable: executable.into(),
        error: None,
    };
    write_switch(&record, &switch)?;
    Ok(Some(serde_json::to_string(&switch)?))
}
pub fn finish_switch(home: &Path, executable: &Path) -> Result<()> {
    let Some(record) = read(home)?.filter(|r| r.phase == Phase::Ready) else {
        return Ok(());
    };
    let own = astra_platform::lifecycle::capture(std::process::id())?;
    let mut switch: Switch = if let Some(token) = std::env::var_os("ASTRA_MAINTENANCE_FRONTEND") {
        serde_json::from_str(
            token
                .to_str()
                .context("Invalid maintenance interface token")?,
        )?
    } else {
        // An interface already executing the target package need not exec itself.
        if !record.frontends.iter().any(|f| f.process == own) {
            return Ok(());
        }
        Switch {
            operation: record.checkpoint_id.clone(),
            previous: own.clone(),
            current: None,
            executable: executable.into(),
            error: None,
        }
    };
    if switch.operation != record.checkpoint_id {
        return Ok(());
    }
    ensure!(
        executable.parent() == Some(record.target.as_path()) && switch.executable == executable,
        "Maintenance interface readiness target differs"
    );
    switch.current = Some(own);
    switch.error = None;
    write_switch(&record, &switch)
}
pub fn fail_switch(home: &Path, token: &str, error: &str) -> Result<()> {
    let Some(record) = read(home)?.filter(|r| r.phase == Phase::Ready) else {
        return Ok(());
    };
    let mut switch: Switch = serde_json::from_str(token)?;
    ensure!(
        switch.operation == record.checkpoint_id,
        "Maintenance interface operation changed"
    );
    switch.error = Some(error.chars().take(8192).collect());
    write_switch(&record, &switch)
}
pub fn switched(record: &Record, frontend: &Frontend) -> Result<bool> {
    let path = switch_path(record, frontend.process.pid);
    match read_plain(&path, 16384) {
        Ok(bytes) => {
            let switch: Switch = serde_json::from_slice(&bytes)?;
            if switch.operation != record.checkpoint_id || switch.previous != frontend.process {
                return Ok(!astra_platform::lifecycle::matches_live(&frontend.process)?);
            }
            ensure!(
                switch.error.is_none(),
                "Interface switch failed: {}",
                switch.error.unwrap_or_default()
            );
            ensure!(
                switch.executable.parent() == Some(record.target.as_path())
                    && switch.executable.file_name() == frontend.executable.file_name(),
                "Interface switch target changed"
            );
            match switch.current {
                Some(current) => {
                    ensure!(
                        astra_platform::lifecycle::matches_live(&current)?,
                        "Updated interface exited before maintenance completed"
                    );
                    Ok(true)
                }
                None => {
                    ensure!(
                        astra_platform::lifecycle::matches_live(&frontend.process)?,
                        "Interface exited before readiness"
                    );
                    Ok(false)
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(!astra_platform::lifecycle::matches_live(&frontend.process)?)
        }
        Err(error) => Err(error.into()),
    }
}
